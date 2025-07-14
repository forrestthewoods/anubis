use anyhow::Context;
use crossbeam::channel::RecvTimeoutError;
use dashmap::DashMap;
use downcast_rs::{impl_downcast, DowncastSync};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::anubis::ArcResult;
use crate::function_name;
use crate::{anubis, bail_loc, job_system, toolchain};

// ----------------------------------------------------------------------------
// Declarations
// ----------------------------------------------------------------------------

// ID for jobs
pub type JobId = i64;

// Function that does the actual work of a job
pub type JobFn = dyn Fn(Job) -> JobFnResult + Send + Sync + 'static;

// Trait to help with void* dynamic casts
pub trait JobResult: DowncastSync + Debug + Send + Sync + 'static {}
impl_downcast!(sync JobResult);

// Return value of a JobFn
pub enum JobFnResult {
    Deferred(JobDeferral),
    Error(anyhow::Error),
    Success(Arc<dyn JobResult>),
}

// Info for a job
pub struct Job {
    pub id: JobId,
    pub desc: String,
    pub ctx: Arc<JobContext>,
    pub job_fn: Option<Box<JobFn>>,
}

// Central hub for JobSystem
pub struct JobSystem {
    pub next_id: Arc<AtomicI64>,
    abort_flag: AtomicBool,
    blocked_jobs: DashMap<JobId, Job>,
    job_graph: Arc<Mutex<JobGraph>>,
    job_results: DashMap<JobId, anyhow::Result<Arc<dyn JobResult>>>,
    tx: crossbeam::channel::Sender<Job>,
    rx: crossbeam::channel::Receiver<Job>,
}

// JobInfo: defines the "graph" of job dependencies
#[derive(Default)]
struct JobGraphNode {
    job_id: JobId,
    finished: bool,
    depends_on: HashSet<JobId>,
    blocks: HashSet<JobId>,
}

#[derive(Default)]
struct JobGraph {
    blocked_by: HashMap<JobId, HashSet<JobId>>,
    blocks: HashMap<JobId, HashSet<JobId>>,
}

pub struct JobGraphEdge {
    pub blocked: JobId,
    pub blocker: JobId,
}

pub struct JobDeferral {
    pub blocked_by: Vec<JobId>,
    pub deferred_job: Job,
}

// Context obj passed into job fn
#[derive(Clone)]
pub struct JobContext {
    pub anubis: Arc<anubis::Anubis>,
    pub job_system: Arc<JobSystem>,
    pub mode: Option<Arc<toolchain::Mode>>,
    pub toolchain: Option<Arc<toolchain::Toolchain>>,
}

// Context obj for workers
#[derive(Clone)]
pub struct WorkerContext {
    pub sender: crossbeam::channel::Sender<Job>,
    pub receiver: crossbeam::channel::Receiver<Job>,
}

// ----------------------------------------------------------------------------
// Implementations
// ----------------------------------------------------------------------------
impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").field("id", &self.id).field("desc", &self.desc).finish()
    }
}

impl Job {
    pub fn new(id: JobId, desc: String, ctx: Arc<JobContext>, job_fn: Box<JobFn>) -> Self {
        Job {
            id,
            desc,
            ctx,
            job_fn: Some(job_fn),
        }
    }
}

impl JobContext {
    pub fn new() -> Self {
        JobContext {
            anubis: Default::default(),
            job_system: JobSystem::new().into(),
            mode: None,
            toolchain: None,
        }
    }

    pub fn get_next_id(&self) -> i64 {
        self.job_system.next_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn new_job(self: &Arc<JobContext>, desc: String, f: Box<JobFn>) -> Job {
        Job::new(self.get_next_id(), desc, self.clone(), f)
    }

    pub fn new_job_with_id(self: &Arc<JobContext>, id: i64, desc: String, f: Box<JobFn>) -> Job {
        assert!(id < self.job_system.next_id.load(Ordering::SeqCst));
        Job::new(id, desc, self.clone(), f)
    }
}

impl JobSystem {
    // ----------------------------------------------------
    // public methods
    // ----------------------------------------------------
    pub fn new() -> Self {
        let (tx, rx) = crossbeam::channel::unbounded::<Job>();
        JobSystem {
            next_id: Default::default(),
            abort_flag: Default::default(),
            blocked_jobs: Default::default(),
            job_graph: Default::default(),
            job_results: Default::default(),
            tx,
            rx,
        }
    }

    pub fn add_job(&self, job: Job) -> anyhow::Result<()> {
        Ok(self.tx.send(job)?)
    }

    pub fn add_job_with_deps(&self, job: Job, deps: &[JobId]) -> anyhow::Result<()> {
        let mut blocked = false;

        // update graph
        {
            let mut job_graph = self.job_graph.lock().unwrap();

            // Update blocked_by
            for &dep in deps {
                // See if job we're blocked by has already finished
                if let Some(dep_result) = self.job_results.get(&dep) {
                    match dep_result.value() {
                        Ok(_) => continue,
                        Err(e) => bail_loc!(
                            "Job [{}] can't be added because dep [{}] failed with [{}]",
                            job.desc,
                            dep,
                            e
                        ),
                    }
                }

                blocked = true;
                job_graph.blocks.entry(dep).or_default().insert(job.id);
                job_graph.blocked_by.entry(job.id).or_default().insert(dep);
            }
        }

        // Send job
        if !blocked {
            Ok(self.tx.send(job)?)
        } else {
            self.blocked_jobs.insert(job.id, job);
            Ok(())
        }
    }

    pub fn run_to_completion(job_sys: Arc<JobSystem>, num_workers: usize) -> anyhow::Result<()> {
        let (tx, rx) = (job_sys.tx.clone(), job_sys.rx.clone());

        let worker_context = WorkerContext {
            sender: tx.clone(),
            receiver: rx.clone(),
        };

        let idle_workers = Arc::new(AtomicUsize::new(0));

        // Create N workers
        std::thread::scope(|scope| {
            for _ in 0..num_workers {
                let worker_context = worker_context.clone();
                let idle_workers = idle_workers.clone();
                let job_sys = job_sys.clone();

                scope.spawn(move || {
                    let maybe_error = || -> anyhow::Result<()> {
                        let mut idle = false;

                        // Loop until complete or abort
                        while !job_sys.abort_flag.load(Ordering::SeqCst) {
                            // Get next job
                            match worker_context.receiver.recv_timeout(Duration::from_millis(100)) {
                                Ok(mut job) => {
                                    // Clear idle flag if set
                                    if idle {
                                        idle = false;
                                        idle_workers.fetch_sub(1, Ordering::SeqCst);
                                    }

                                    // Execute job and store result
                                    let job_id = job.id;
                                    let job_desc = job.desc.clone();
                                    let job_fn = job.job_fn.take().ok_or_else(|| {
                                        anyhow::anyhow!("Job [{}:{}] missing job fn", job.id, job.desc)
                                    })?;
                                    println!("Running job [{}]: {}", job_id, job_desc);
                                    let job_result = job_fn(job);

                                    match job_result {
                                        JobFnResult::Deferred(deferral) => {
                                            println!(
                                                "   Job [{}/{}] deferred. Blocking on [{:?}]",
                                                job_id, deferral.deferred_job.id, deferral.blocked_by
                                            );
                                            job_sys.add_job_with_deps(
                                                deferral.deferred_job,
                                                &deferral.blocked_by,
                                            )?;
                                        }
                                        JobFnResult::Error(e) => {
                                            println!("   Job [{}] failed [{}]", job_id, e);

                                            // Store error
                                            let s = e.to_string();
                                            let job_result = anyhow::Result::Err(e).context(format!(
                                                "Job Failed:\n    Desc: {}\n    Err:{}",
                                                job_desc, s
                                            ));
                                            job_sys.job_results.insert(job_id, job_result);

                                            // Abort everything
                                            job_sys.abort_flag.store(true, Ordering::SeqCst);
                                        }
                                        JobFnResult::Success(result) => {
                                            println!("   Job [{}] succeeded!", job_id);

                                            let finished_job = job_id;

                                            // Store result
                                            job_sys.job_results.insert(job_id, Ok(result));

                                            // Notify blocked_jobs this job is complete
                                            let mut graph = job_sys.job_graph.lock().unwrap();
                                            if let Some(blocked_jobs) = graph.blocks.remove(&finished_job) {
                                                for blocked_job in blocked_jobs {
                                                    if let Some(blocked_by) =
                                                        graph.blocked_by.get_mut(&blocked_job)
                                                    {
                                                        blocked_by.remove(&finished_job);
                                                        if blocked_by.is_empty() {
                                                            if let Some((_, unblocked_job)) =
                                                                job_sys.blocked_jobs.remove(&blocked_job)
                                                            {
                                                                worker_context.sender.send(unblocked_job)?;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(RecvTimeoutError::Timeout) => {
                                    if !idle {
                                        idle = true;
                                        idle_workers.fetch_add(1, Ordering::SeqCst);
                                    }

                                    // Timeout: check if jobsys is complete, otherwise loop and get a new job
                                    if idle_workers.load(Ordering::SeqCst) == num_workers
                                        && worker_context.receiver.is_empty()
                                    {
                                        break;
                                    }
                                }
                                Err(RecvTimeoutError::Disconnected) => break,
                            }
                        }

                        Ok(())
                    }();

                    if let Err(e) = maybe_error {
                        eprintln!("JobSystem failed. Error: {e}");
                        job_sys.abort_flag.store(true, Ordering::SeqCst);
                    }
                });
            }
        });

        // Check for any errors
        if job_sys.abort_flag.load(Ordering::SeqCst) {
            let errors = job_sys
                .job_results
                .iter()
                .filter_map(|v| match v.value() {
                    Ok(_) => None,
                    Err(e) => Some(e.to_string()),
                })
                .fold(
                    String::new(),
                    |acc, s| {
                        if acc.is_empty() {
                            s
                        } else {
                            acc + "\n" + &s
                        }
                    },
                );

            anyhow::bail!("JobSystem failed. Errors:\n{}", errors);
        }

        // Sanity check: ensure all jobs actually completed
        if !job_sys.blocked_jobs.is_empty() {
            anyhow::bail!(
                "JobSystem finished but had [{}] jobs that weren't finished. [{:?}]",
                job_sys.blocked_jobs.len(),
                job_sys.blocked_jobs
            );
        }

        // Success!
        Ok(())
    }

    pub fn try_get_result(&self, job_id: JobId) -> Option<anyhow::Result<Arc<dyn JobResult>>> {
        if let Some(kvp) = self.job_results.get(&job_id) {
            let arc_result = kvp.as_ref().map_err(|e| anyhow::anyhow!("{}", e)).cloned();
            Some(arc_result)
        } else {
            None
        }
    }

    pub fn expect_result<T: JobResult>(&self, job_id: JobId) -> ArcResult<T> {
        if let Some(kvp) = self.job_results.get(&job_id) {
            let arc_result = kvp.as_ref().map_err(|e| anyhow::anyhow!("{}", e))?.clone();
            arc_result.downcast_arc::<T>().map(|v| v.clone()).map_err(|_| {
                anyhow::anyhow!(
                    "Job result for job id {} could not be cast to the expected type",
                    job_id
                )
            })
        } else {
            let mut errors = Vec::new();
            for entry in self.job_results.iter() {
                if let Err(err) = entry.value() {
                    errors.push(format!("Job id {}: {}", entry.key(), err));
                }
            }
            if errors.is_empty() {
                Err(anyhow::anyhow!(
                    "No job result found for job id {} and no job errors recorded",
                    job_id
                ))
            } else {
                Err(anyhow::anyhow!("Aggregated job errors: {}", errors.join("; ")))
            }
        }
    }

    // ----------------------------------------------------
    // private methods
    // ----------------------------------------------------
    fn handle_new_jobs(
        job_sys: &Arc<JobSystem>,
        new_jobs: Vec<Job>,
        new_edges: &[JobGraphEdge],
        tx: &crossbeam::channel::Sender<Job>,
    ) -> anyhow::Result<()> {
        // Seed jobs
        let mut graph = job_sys.job_graph.lock().unwrap();

        // Insert edges
        for edge in new_edges {
            let already_finished = job_sys.job_results.get(&edge.blocker).is_some();

            // don't insert edge if blocker is already finished
            if !already_finished {
                graph.blocked_by.entry(edge.blocked).or_default().insert(edge.blocker);
                graph.blocks.entry(edge.blocker).or_default().insert(edge.blocked);
            }
        }

        // Push initial_jobs into either blocked_job or work queue
        for job in new_jobs {
            if job.job_fn.is_none() {
                anyhow::bail!("Job [{}:{}] had no job fn", job.id, job.desc);
            }

            // Determine if blocked
            let is_blocked = if let Some(blocked_by) = graph.blocked_by.get(&job.id) {
                !blocked_by.is_empty()
            } else {
                false
            };

            if is_blocked {
                // Store in blocked list
                job_sys.blocked_jobs.insert(job.id, job);
            } else {
                // Insert into work queue
                tx.send(job)?;
            }
        }

        Ok(())
    }

    fn any_errors(&self) -> bool {
        self.job_results.iter().any(|r| r.is_err())
    }

    fn get_errors(&self) -> Vec<anyhow::Error> {
        let mut errors = Vec::new();
        // Collect the keys of entries that contain an error.
        let error_keys: Vec<JobId> = self
            .job_results
            .iter()
            .filter(|entry| entry.value().is_err())
            .map(|entry| *entry.key())
            .collect();

        // Remove the entries one by one and collect the error values.
        for key in error_keys {
            if let Some((_, result)) = self.job_results.remove(&key) {
                if let Err(e) = result {
                    errors.push(e);
                }
            }
        }

        errors
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    pub struct TrivialResult(pub i64);
    impl JobResult for TrivialResult {}

    #[test]
    fn trivial_job() -> anyhow::Result<()> {
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        let job = Job::new(
            ctx.get_next_id(),
            "TrivialJob".to_owned(),
            ctx,
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(42)))),
        );
        jobsys.add_job(job)?;

        JobSystem::run_to_completion(jobsys.clone(), 1)?;

        let result = jobsys.expect_result::<TrivialResult>(0)?;
        assert_eq!(result.0, 42);

        Ok(())
    }

    #[test]
    fn basic_dependency() -> anyhow::Result<()> {
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        let mut flag = Arc::new(AtomicBool::new(false));

        // Create job A
        let a_flag = flag.clone();
        let mut a = Job::new(
            ctx.get_next_id(),
            "job_a".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                a_flag.store(true, Ordering::SeqCst);
                JobFnResult::Success(Arc::new(TrivialResult(42)))
            }),
        );

        // Create job B
        let b_flag = flag.clone();
        let mut b = Job::new(
            ctx.get_next_id(),
            "job_b".to_owned(),
            ctx,
            Box::new(move |_| {
                if b_flag.load(Ordering::SeqCst) {
                    JobFnResult::Success(Arc::new(TrivialResult(1337)))
                } else {
                    JobFnResult::Error(anyhow::anyhow!("Job B expected flag to be set by Job A"))
                }
            }),
        );

        // Add jobs
        let a_id = b.id;
        jobsys.add_job(a)?;
        jobsys.add_job_with_deps(b, &vec![a_id])?;

        // Run jobs
        // Note we pass job_b before job_a
        JobSystem::run_to_completion(jobsys.clone(), 1)?;

        // Ensure both jobs successfully completed with the given value
        assert_eq!(
            *jobsys.expect_result::<TrivialResult>(0).unwrap(),
            TrivialResult(42)
        );
        assert_eq!(
            *jobsys.expect_result::<TrivialResult>(1).unwrap(),
            TrivialResult(1337)
        );
        assert_eq!(jobsys.any_errors(), false);

        Ok(())
    }

    #[test]
    fn basic_dynamic_dependency() -> anyhow::Result<()> {
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        let a = Job::new(
            ctx.get_next_id(),
            "job a".to_owned(),
            ctx.clone(),
            Box::new(|mut job: Job| {
                // Create a new dependent job
                let mut dep_job = Job::new(
                    job.ctx.get_next_id(),
                    "child job".to_owned(),
                    job.ctx.clone(),
                    Box::new(|job| JobFnResult::Success(Arc::new(TrivialResult(1337)))),
                );

                let mut edges = vec![JobGraphEdge {
                    blocker: dep_job.id,
                    blocked: job.id,
                }];

                // New fn for "this" job
                job.job_fn = Some(Box::new(|job| JobFnResult::Success(Arc::new(TrivialResult(42)))));

                JobFnResult::Deferred(JobDeferral {
                    new_jobs: vec![dep_job, job],
                    graph_updates: edges,
                })
            }),
        );

        // Add jobs
        jobsys.add_job(a)?;

        // Run jobs
        JobSystem::run_to_completion(jobsys.clone(), 2)?;

        // Verify results
        assert_eq!(jobsys.abort_flag.load(Ordering::SeqCst), false);
        let errors = jobsys.get_errors();
        assert_eq!(errors.len(), 0, "Errors: {:?}", errors);
        assert_eq!(jobsys.job_results.len(), 2);
        assert_eq!(
            *jobsys.expect_result::<TrivialResult>(0).unwrap(),
            TrivialResult(42)
        );
        assert_eq!(
            *jobsys.expect_result::<TrivialResult>(1).unwrap(),
            TrivialResult(1337)
        );

        Ok(())
    }
}
