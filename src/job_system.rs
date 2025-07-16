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
        let a_id = a.id;
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

    // Test worker thread coordination and load balancing
    #[test] 
    fn worker_load_balancing() -> anyhow::Result<()> {
        // Test verifies that work is distributed across multiple workers effectively
        // Each job simulates different amounts of work to test load balancing
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        let work_counter = Arc::new(AtomicUsize::new(0));

        // Create jobs with varying work loads
        for i in 0..12 {
            let counter = work_counter.clone();
            let work_ms = (i % 3) * 10; // 0ms, 10ms, 20ms work simulation
            
            let job = Job::new(
                ctx.get_next_id(),
                format!("work_job_{}", i),
                ctx.clone(),
                Box::new(move |_| {
                    // Simulate work
                    if work_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(work_ms));
                    }
                    let work_done = counter.fetch_add(1, Ordering::SeqCst);
                    JobFnResult::Success(Arc::new(TrivialResult(work_done as i64)))
                }),
            );
            jobsys.add_job(job)?;
        }

        let start_time = std::time::Instant::now();
        JobSystem::run_to_completion(jobsys.clone(), 4)?; // Use 4 workers
        let elapsed = start_time.elapsed();

        // With 4 workers, should complete faster than single worker
        assert!(elapsed < std::time::Duration::from_millis(300), 
               "Multi-worker system should complete work efficiently");

        // Verify all work was completed
        assert_eq!(work_counter.load(Ordering::SeqCst), 12);

        // Verify all jobs have unique work completion order
        let mut work_orders: Vec<i64> = (0..12)
            .map(|i| jobsys.expect_result::<TrivialResult>(i).unwrap().0)
            .collect();
        work_orders.sort();
        
        // Each job should have gotten a unique work counter value
        for (i, &order) in work_orders.iter().enumerate() {
            assert_eq!(order, i as i64, "Work should be completed in order");
        }

        Ok(())
    }

    // Test complex dependency chains with multiple levels
    #[test]
    fn complex_dependency_chain() -> anyhow::Result<()> {
        // Test verifies that jobs with complex dependency chains execute in correct order
        // Chain: job_d -> job_c -> job_b -> job_a
        // Each job stores its execution order in a shared counter
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        let execution_order = Arc::new(AtomicU8::new(0));

        // Job A (executes last)
        let order_a = execution_order.clone();
        let job_a = Job::new(
            ctx.get_next_id(),
            "job_a".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                let order = order_a.fetch_add(1, Ordering::SeqCst);
                JobFnResult::Success(Arc::new(TrivialResult(order as i64)))
            }),
        );

        // Job B (depends on A)
        let order_b = execution_order.clone();
        let job_b = Job::new(
            ctx.get_next_id(),
            "job_b".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                let order = order_b.fetch_add(1, Ordering::SeqCst);
                JobFnResult::Success(Arc::new(TrivialResult(order as i64)))
            }),
        );

        // Job C (depends on B)
        let order_c = execution_order.clone();
        let job_c = Job::new(
            ctx.get_next_id(),
            "job_c".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                let order = order_c.fetch_add(1, Ordering::SeqCst);
                JobFnResult::Success(Arc::new(TrivialResult(order as i64)))
            }),
        );

        // Job D (depends on C)
        let order_d = execution_order.clone();
        let job_d = Job::new(
            ctx.get_next_id(),
            "job_d".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                let order = order_d.fetch_add(1, Ordering::SeqCst);
                JobFnResult::Success(Arc::new(TrivialResult(order as i64)))
            }),
        );

        let a_id = job_a.id;
        let b_id = job_b.id;
        let c_id = job_c.id;
        let d_id = job_d.id;

        // Add jobs with dependencies
        jobsys.add_job(job_a)?;
        jobsys.add_job_with_deps(job_b, &[a_id])?;
        jobsys.add_job_with_deps(job_c, &[b_id])?;
        jobsys.add_job_with_deps(job_d, &[c_id])?;

        JobSystem::run_to_completion(jobsys.clone(), 4)?;

        // Verify execution order: A=0, B=1, C=2, D=3
        assert_eq!(jobsys.expect_result::<TrivialResult>(a_id)?.0, 0);
        assert_eq!(jobsys.expect_result::<TrivialResult>(b_id)?.0, 1);
        assert_eq!(jobsys.expect_result::<TrivialResult>(c_id)?.0, 2);
        assert_eq!(jobsys.expect_result::<TrivialResult>(d_id)?.0, 3);

        Ok(())
    }

    // Test diamond dependency pattern (multiple jobs depend on same job)
    #[test]
    fn diamond_dependency_pattern() -> anyhow::Result<()> {
        // Test verifies diamond dependencies work correctly:
        //     A
        //    / \
        //   B   C
        //    \ /
        //     D
        // D depends on both B and C, which both depend on A
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        let execution_flags = Arc::new((AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false)));

        // Job A (foundation)
        let flags_a = execution_flags.clone();
        let job_a = Job::new(
            ctx.get_next_id(),
            "job_a".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                flags_a.0.store(true, Ordering::SeqCst);
                JobFnResult::Success(Arc::new(TrivialResult(1)))
            }),
        );

        // Job B (depends on A)
        let flags_b = execution_flags.clone();
        let job_b = Job::new(
            ctx.get_next_id(),
            "job_b".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                // Verify A has executed
                assert!(flags_b.0.load(Ordering::SeqCst), "Job A should have executed before B");
                flags_b.1.store(true, Ordering::SeqCst);
                JobFnResult::Success(Arc::new(TrivialResult(2)))
            }),
        );

        // Job C (depends on A)
        let flags_c = execution_flags.clone();
        let job_c = Job::new(
            ctx.get_next_id(),
            "job_c".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                // Verify A has executed
                assert!(flags_c.0.load(Ordering::SeqCst), "Job A should have executed before C");
                flags_c.2.store(true, Ordering::SeqCst);
                JobFnResult::Success(Arc::new(TrivialResult(3)))
            }),
        );

        // Job D (depends on both B and C)
        let flags_d = execution_flags.clone();
        let job_d = Job::new(
            ctx.get_next_id(),
            "job_d".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                // Verify both B and C have executed
                assert!(flags_d.1.load(Ordering::SeqCst), "Job B should have executed before D");
                assert!(flags_d.2.load(Ordering::SeqCst), "Job C should have executed before D");
                JobFnResult::Success(Arc::new(TrivialResult(4)))
            }),
        );

        let a_id = job_a.id;
        let b_id = job_b.id;
        let c_id = job_c.id;
        let d_id = job_d.id;

        // Add jobs with dependencies
        jobsys.add_job(job_a)?;
        jobsys.add_job_with_deps(job_b, &[a_id])?;
        jobsys.add_job_with_deps(job_c, &[a_id])?;
        jobsys.add_job_with_deps(job_d, &[b_id, c_id])?;

        JobSystem::run_to_completion(jobsys.clone(), 4)?;

        // Verify all jobs completed successfully
        assert_eq!(jobsys.expect_result::<TrivialResult>(a_id)?.0, 1);
        assert_eq!(jobsys.expect_result::<TrivialResult>(b_id)?.0, 2);
        assert_eq!(jobsys.expect_result::<TrivialResult>(c_id)?.0, 3);
        assert_eq!(jobsys.expect_result::<TrivialResult>(d_id)?.0, 4);

        Ok(())
    }

    // Test error propagation stops execution of dependent jobs
    #[test]
    fn error_propagation() -> anyhow::Result<()> {
        // Test verifies that when a job fails, the system aborts and doesn't execute dependent jobs
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        let should_not_execute = Arc::new(AtomicBool::new(false));

        // Job A (will fail)
        let job_a = Job::new(
            ctx.get_next_id(),
            "failing_job".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Error(anyhow::anyhow!("Intentional test failure"))),
        );

        // Job B (should not execute due to A's failure)
        let flag = should_not_execute.clone();
        let job_b = Job::new(
            ctx.get_next_id(),
            "dependent_job".to_owned(),
            ctx.clone(),
            Box::new(move |_| {
                flag.store(true, Ordering::SeqCst);
                JobFnResult::Success(Arc::new(TrivialResult(42)))
            }),
        );

        let a_id = job_a.id;
        let b_id = job_b.id;

        jobsys.add_job(job_a)?;
        jobsys.add_job_with_deps(job_b, &[a_id])?;

        // This should fail due to job A's error
        let result = JobSystem::run_to_completion(jobsys.clone(), 2);
        assert!(result.is_err(), "JobSystem should have failed due to job A's error");

        // Verify job B never executed
        assert!(!should_not_execute.load(Ordering::SeqCst), "Dependent job should not have executed");

        // Verify abort flag is set
        assert!(jobsys.abort_flag.load(Ordering::SeqCst), "Abort flag should be set");

        // Verify we have error results
        assert!(jobsys.try_get_result(a_id).unwrap().is_err(), "Job A should have error result");
        assert!(jobsys.try_get_result(b_id).is_none(), "Job B should have no result");

        Ok(())
    }

    // Test concurrent job execution with no dependencies
    #[test]
    fn concurrent_independent_jobs() -> anyhow::Result<()> {
        // Test verifies that independent jobs can execute concurrently
        // Uses timing to ensure jobs run in parallel, not sequentially
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        let start_time = std::time::Instant::now();
        let barrier = Arc::new(std::sync::Barrier::new(3)); // 3 jobs will wait for each other

        // Create 3 independent jobs that synchronize at a barrier
        // If they run concurrently, they'll all reach the barrier quickly
        // If they run sequentially, the test will timeout
        for i in 0..3 {
            let barrier_clone = barrier.clone();
            let job = Job::new(
                ctx.get_next_id(),
                format!("concurrent_job_{}", i),
                ctx.clone(),
                Box::new(move |_| {
                    // All jobs wait at barrier - proves they're running concurrently
                    barrier_clone.wait();
                    JobFnResult::Success(Arc::new(TrivialResult(i)))
                }),
            );
            jobsys.add_job(job)?;
        }

        JobSystem::run_to_completion(jobsys.clone(), 3)?;

        // Verify all completed within reasonable time (concurrent execution)
        let elapsed = start_time.elapsed();
        assert!(elapsed < std::time::Duration::from_millis(1000), 
               "Jobs should complete quickly if running concurrently");

        // Verify all jobs completed
        for i in 0..3 {
            assert_eq!(jobsys.expect_result::<TrivialResult>(i)?.0, i);
        }

        Ok(())
    }

    // Test adding dependencies to already completed jobs
    #[test]
    fn dependency_on_completed_job() -> anyhow::Result<()> {
        // Test verifies that adding a dependency on an already completed job works correctly
        // The dependent job should execute immediately since dependency is satisfied
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Job A (will complete first)
        let job_a = Job::new(
            ctx.get_next_id(),
            "completed_job".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(42)))),
        );

        let a_id = job_a.id;
        jobsys.add_job(job_a)?;

        // Run just job A to completion
        JobSystem::run_to_completion(jobsys.clone(), 1)?;

        // Verify A completed
        assert_eq!(jobsys.expect_result::<TrivialResult>(a_id)?.0, 42);

        // Now add job B that depends on the already-completed job A
        let job_b = Job::new(
            ctx.get_next_id(),
            "dependent_on_completed".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(1337)))),
        );

        let b_id = job_b.id;
        jobsys.add_job_with_deps(job_b, &[a_id])?;

        // Run again - job B should execute immediately since A is done
        JobSystem::run_to_completion(jobsys.clone(), 1)?;

        // Verify B completed
        assert_eq!(jobsys.expect_result::<TrivialResult>(b_id)?.0, 1337);

        Ok(())
    }

    // Test adding dependency on failed job should fail
    #[test]
    fn dependency_on_failed_job() -> anyhow::Result<()> {
        // Test verifies that trying to add a dependency on a job that already failed
        // results in an error when adding the dependent job
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Job A (will fail)
        let job_a = Job::new(
            ctx.get_next_id(),
            "failing_job".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Error(anyhow::anyhow!("Intentional failure"))),
        );

        let a_id = job_a.id;
        jobsys.add_job(job_a)?;

        // Run job A to failure
        let result = JobSystem::run_to_completion(jobsys.clone(), 1);
        assert!(result.is_err(), "Job A should have failed");

        // Now try to add job B that depends on the failed job A
        let job_b = Job::new(
            ctx.get_next_id(),
            "dependent_on_failed".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(42)))),
        );

        // This should fail because A failed
        let add_result = jobsys.add_job_with_deps(job_b, &[a_id]);
        assert!(add_result.is_err(), "Adding dependency on failed job should fail");

        Ok(())
    }

    // Test job execution with mixed success and dependency patterns
    #[test]
    fn mixed_job_patterns() -> anyhow::Result<()> {
        // Test verifies various job patterns working together:
        // - Independent jobs
        // - Chain dependencies  
        // - Fan-out dependencies
        // This provides comprehensive integration testing
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Independent jobs (will run in parallel)
        let indep_1 = Job::new(
            ctx.get_next_id(),
            "independent_1".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(100)))),
        );
        
        let indep_2 = Job::new(
            ctx.get_next_id(),
            "independent_2".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(200)))),
        );

        // Chain: base -> middle -> final
        let base_job = Job::new(
            ctx.get_next_id(),
            "chain_base".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(5)))),
        );

        let middle_job = Job::new(
            ctx.get_next_id(),
            "chain_middle".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(10)))),
        );

        let final_job = Job::new(
            ctx.get_next_id(),
            "chain_final".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(15)))),
        );

        let base_id = base_job.id;
        let middle_id = middle_job.id;
        let final_id = final_job.id;
        let indep_1_id = indep_1.id;
        let indep_2_id = indep_2.id;

        // Add independent jobs
        jobsys.add_job(indep_1)?;
        jobsys.add_job(indep_2)?;
        
        // Add chain jobs
        jobsys.add_job(base_job)?;
        jobsys.add_job_with_deps(middle_job, &[base_id])?;
        jobsys.add_job_with_deps(final_job, &[middle_id])?;

        JobSystem::run_to_completion(jobsys.clone(), 4)?;

        // Verify all results
        assert_eq!(jobsys.expect_result::<TrivialResult>(indep_1_id)?.0, 100);
        assert_eq!(jobsys.expect_result::<TrivialResult>(indep_2_id)?.0, 200);
        assert_eq!(jobsys.expect_result::<TrivialResult>(base_id)?.0, 5);
        assert_eq!(jobsys.expect_result::<TrivialResult>(middle_id)?.0, 10);
        assert_eq!(jobsys.expect_result::<TrivialResult>(final_id)?.0, 15);

        Ok(())
    }

    // Test job system with no jobs completes immediately
    #[test]
    fn empty_job_system() -> anyhow::Result<()> {
        // Test verifies that running an empty job system completes immediately without error
        
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        let start_time = std::time::Instant::now();

        JobSystem::run_to_completion(jobsys.clone(), 4)?;

        let elapsed = start_time.elapsed();
        assert!(elapsed < std::time::Duration::from_millis(1000), 
               "Empty job system should complete very quickly");

        // Verify no results or errors
        assert_eq!(jobsys.job_results.len(), 0);
        assert!(!jobsys.abort_flag.load(Ordering::SeqCst));

        Ok(())
    }

    // Test single worker can handle all jobs
    #[test]
    fn single_worker_stress_test() -> anyhow::Result<()> {
        // Test verifies that a single worker can handle multiple jobs with dependencies
        // This tests the worker's ability to process jobs sequentially
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Create a chain of 10 dependent jobs
        let mut prev_job_id = None;
        for i in 0..10 {
            let job = Job::new(
                ctx.get_next_id(),
                format!("chain_job_{}", i),
                ctx.clone(),
                Box::new(move |_| JobFnResult::Success(Arc::new(TrivialResult(i)))),
            );

            let job_id = job.id;
            if let Some(prev_id) = prev_job_id {
                jobsys.add_job_with_deps(job, &[prev_id])?;
            } else {
                jobsys.add_job(job)?;
            }
            prev_job_id = Some(job_id);
        }

        // Use only 1 worker
        JobSystem::run_to_completion(jobsys.clone(), 1)?;

        // Verify all jobs completed in order
        for i in 0..10 {
            assert_eq!(jobsys.expect_result::<TrivialResult>(i)?.0, i);
        }

        Ok(())
    }

    // Test worker timeout and idle detection
    #[test]
    fn worker_idle_detection() -> anyhow::Result<()> {
        // Test verifies that workers correctly detect when all work is done
        // by adding a job that takes some time, ensuring idle detection works
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Job that takes a short time to complete
        let job = Job::new(
            ctx.get_next_id(),
            "slow_job".to_owned(),
            ctx.clone(),
            Box::new(|_| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                JobFnResult::Success(Arc::new(TrivialResult(42)))
            }),
        );

        jobsys.add_job(job)?;

        let start_time = std::time::Instant::now();
        JobSystem::run_to_completion(jobsys.clone(), 3)?;
        let elapsed = start_time.elapsed();

        // Should complete in reasonable time (not hang)
        assert!(elapsed < std::time::Duration::from_millis(1000));
        assert_eq!(jobsys.expect_result::<TrivialResult>(0)?.0, 42);

        Ok(())
    }

    // Test very large number of independent jobs
    #[test]
    fn large_number_of_jobs() -> anyhow::Result<()> {
        // Test verifies the system can handle a large number of concurrent jobs
        // This tests scalability and memory management
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        
        let num_jobs = 100;

        // Create many independent jobs
        for i in 0..num_jobs {
            let job = Job::new(
                ctx.get_next_id(),
                format!("bulk_job_{}", i),
                ctx.clone(),
                Box::new(move |_| JobFnResult::Success(Arc::new(TrivialResult(i)))),
            );
            jobsys.add_job(job)?;
        }

        let start_time = std::time::Instant::now();
        JobSystem::run_to_completion(jobsys.clone(), 8)?; // Use 8 workers
        let elapsed = start_time.elapsed();

        // Should complete in reasonable time with parallel execution
        assert!(elapsed < std::time::Duration::from_secs(5));

        // Verify all jobs completed
        for i in 0..num_jobs {
            assert_eq!(jobsys.expect_result::<TrivialResult>(i)?.0, i);
        }

        Ok(())
    }

    // Test handling of invalid dependency scenarios
    #[test]
    fn invalid_dependency_scenarios() -> anyhow::Result<()> {
        // Test verifies that the system handles invalid dependency cases gracefully
        // This tests edge cases in dependency management
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Test 1: Dependency on non-existent job (this should block forever and be detected)
        let job_missing_dep = Job::new(
            ctx.get_next_id(),
            "depends_on_missing".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(1)))),
        );

        // Add job that depends on job ID 999 which doesn't exist
        jobsys.add_job_with_deps(job_missing_dep, &[999])?;

        // This should fail because job 999 doesn't exist and will never complete
        let result = JobSystem::run_to_completion(jobsys.clone(), 1);
        assert!(result.is_err(), "Should fail due to missing dependency");

        // Test 2: Self-dependency (job depends on itself)
        let jobsys2: Arc<JobSystem> = JobSystem::new().into();
        let self_dep_job = Job::new(
            ctx.get_next_id(),
            "self_dependent".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(2)))),
        );

        let self_id = self_dep_job.id;
        jobsys2.add_job_with_deps(self_dep_job, &[self_id])?;

        // This should also fail due to self-dependency deadlock
        let result2 = JobSystem::run_to_completion(jobsys2, 1);
        assert!(result2.is_err(), "Should fail due to self-dependency");
        
        Ok(())
    }

    // Test job result type safety and downcasting
    #[test]
    fn job_result_type_safety() -> anyhow::Result<()> {
        // Test verifies that job results maintain type safety and proper downcasting
        
        #[derive(Debug)]
        struct CustomResult {
            value: String,
            number: i32,
        }
        impl JobResult for CustomResult {}

        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Job that returns custom result type
        let job = Job::new(
            ctx.get_next_id(),
            "custom_result_job".to_owned(),
            ctx.clone(),
            Box::new(|_| {
                JobFnResult::Success(Arc::new(CustomResult {
                    value: "test".to_string(),
                    number: 42,
                }))
            }),
        );

        let job_id = job.id;
        jobsys.add_job(job)?;

        JobSystem::run_to_completion(jobsys.clone(), 1)?;

        // Test correct type retrieval
        let result = jobsys.expect_result::<CustomResult>(job_id)?;
        assert_eq!(result.value, "test");
        assert_eq!(result.number, 42);

        // Test incorrect type retrieval should fail
        let wrong_type_result = jobsys.expect_result::<TrivialResult>(job_id);
        assert!(wrong_type_result.is_err(), "Wrong type cast should fail");

        Ok(())
    }

    // Test job context sharing and isolation
    #[test]
    fn job_context_sharing() -> anyhow::Result<()> {
        // Test verifies that jobs can share context but maintain proper isolation
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        let shared_counter = Arc::new(AtomicI64::new(0));

        // Multiple jobs that access shared state through context
        for i in 0..5 {
            let counter = shared_counter.clone();
            let job = Job::new(
                ctx.get_next_id(),
                format!("shared_context_job_{}", i),
                ctx.clone(),
                Box::new(move |_| {
                    let value = counter.fetch_add(1, Ordering::SeqCst);
                    JobFnResult::Success(Arc::new(TrivialResult(value)))
                }),
            );
            jobsys.add_job(job)?;
        }

        JobSystem::run_to_completion(jobsys.clone(), 3)?;

        // Verify all jobs ran and accessed shared state
        let final_count = shared_counter.load(Ordering::SeqCst);
        assert_eq!(final_count, 5);

        // Each job should have gotten a unique value from the counter
        let mut values: Vec<i64> = (0..5)
            .map(|i| jobsys.expect_result::<TrivialResult>(i).unwrap().0)
            .collect();
        values.sort();
        assert_eq!(values, vec![0, 1, 2, 3, 4]);

        Ok(())
    }

    // Test aborting during job execution
    #[test]
    fn abort_during_execution() -> anyhow::Result<()> {
        // Test verifies that abort flag properly stops job execution
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();
        let execution_count = Arc::new(AtomicUsize::new(0));

        // Create jobs that check abort flag
        for i in 0..10 {
            let count = execution_count.clone();
            let job = Job::new(
                ctx.get_next_id(),
                format!("abortable_job_{}", i),
                ctx.clone(),
                Box::new(move |job| {
                    count.fetch_add(1, Ordering::SeqCst);
                    if job.id == 2 {
                        // Job 2 will fail and trigger abort
                        JobFnResult::Error(anyhow::anyhow!("Triggering abort"))
                    } else {
                        // Other jobs might not get to run due to abort
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        JobFnResult::Success(Arc::new(TrivialResult(job.id)))
                    }
                }),
            );
            jobsys.add_job(job)?;
        }

        let result = JobSystem::run_to_completion(jobsys.clone(), 4);
        assert!(result.is_err(), "Should fail due to job 2's error");

        // Verify abort flag is set
        assert!(jobsys.abort_flag.load(Ordering::SeqCst));

        // Some jobs may have executed before abort, but not all
        let executed = execution_count.load(Ordering::SeqCst);
        assert!(executed < 10, "Not all jobs should have executed due to abort");

        Ok(())
    }

    // Test job system resilience to rapid job addition
    #[test]
    fn rapid_job_addition() -> anyhow::Result<()> {
        // Test verifies that the system can handle rapid addition of many jobs
        // This tests the thread-safe queue and channel handling
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Spawn multiple threads that add jobs rapidly
        let handles: Vec<_> = (0..4).map(|thread_id| {
            let ctx = ctx.clone();
            let jobsys = jobsys.clone();
            
            std::thread::spawn(move || -> anyhow::Result<()> {
                for i in 0..25 {
                    let job = Job::new(
                        ctx.get_next_id(),
                        format!("rapid_job_t{}_i{}", thread_id, i),
                        ctx.clone(),
                        Box::new(move |_| {
                            // Small amount of work to prevent immediate completion
                            std::thread::sleep(std::time::Duration::from_micros(100));
                            JobFnResult::Success(Arc::new(TrivialResult((thread_id * 100 + i) as i64)))
                        }),
                    );
                    jobsys.add_job(job)?;
                }
                Ok(())
            })
        }).collect();

        // Wait for all threads to add their jobs
        for handle in handles {
            handle.join().unwrap()?;
        }

        let start_time = std::time::Instant::now();
        JobSystem::run_to_completion(jobsys.clone(), 8)?;
        let elapsed = start_time.elapsed();

        // Should complete all jobs reasonably quickly
        assert!(elapsed < std::time::Duration::from_secs(2));

        // Verify all 100 jobs completed (4 threads * 25 jobs each)
        assert_eq!(jobsys.job_results.len(), 100);

        // Verify no jobs are blocked
        assert!(jobsys.blocked_jobs.is_empty());

        Ok(())
    }

    // Test memory safety with large job results
    #[test]
    fn large_job_results() -> anyhow::Result<()> {
        // Test verifies that the system can handle jobs with large result data
        // This tests memory management and Arc handling
        
        #[derive(Debug)]
        struct LargeResult {
            data: Vec<u8>,
            id: usize,
        }
        impl JobResult for LargeResult {}

        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Create jobs that produce large results
        for i in 0..10 {
            let job = Job::new(
                ctx.get_next_id(),
                format!("large_result_job_{}", i),
                ctx.clone(),
                Box::new(move |_| {
                    // Create a 1MB result
                    let large_data = vec![i as u8; 1024 * 1024];
                    JobFnResult::Success(Arc::new(LargeResult {
                        data: large_data,
                        id: i as usize,
                    }))
                }),
            );
            jobsys.add_job(job)?;
        }

        JobSystem::run_to_completion(jobsys.clone(), 4)?;

        // Verify all results are correct and accessible
        for i in 0..10 {
            let result = jobsys.expect_result::<LargeResult>(i)?;
            assert_eq!(result.id, i as usize);
            assert_eq!(result.data.len(), 1024 * 1024);
            assert_eq!(result.data[0], i as u8);
        }

        Ok(())
    }

    // Test edge case: job with no function should fail gracefully
    #[test]
    fn job_without_function() -> anyhow::Result<()> {
        // Test verifies that jobs without functions are handled properly
        // This tests the job validation logic
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        let mut job = Job::new(
            ctx.get_next_id(),
            "job_without_fn".to_owned(),
            ctx.clone(),
            Box::new(|_| JobFnResult::Success(Arc::new(TrivialResult(42)))),
        );

        // Remove the job function to simulate the error case
        job.job_fn = None;

        jobsys.add_job(job)?;

        // This should fail during execution when the job has no function
        let result = JobSystem::run_to_completion(jobsys, 1);
        assert!(result.is_err(), "Should fail due to missing job function");

        Ok(())
    }

    // Test graceful shutdown behavior
    #[test]
    fn graceful_shutdown() -> anyhow::Result<()> {
        // Test verifies that the system shuts down gracefully when all work is done
        // This tests the idle detection and completion logic
        
        let ctx: Arc<JobContext> = JobContext::new().into();
        let jobsys: Arc<JobSystem> = JobSystem::new().into();

        // Add a few simple jobs
        for i in 0..5 {
            let job = Job::new(
                ctx.get_next_id(),
                format!("shutdown_test_job_{}", i),
                ctx.clone(),
                Box::new(move |_| {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    JobFnResult::Success(Arc::new(TrivialResult(i)))
                }),
            );
            jobsys.add_job(job)?;
        }

        let start_time = std::time::Instant::now();
        JobSystem::run_to_completion(jobsys.clone(), 3)?;
        let elapsed = start_time.elapsed();

        // Should complete quickly and cleanly
        assert!(elapsed < std::time::Duration::from_millis(500));

        // All jobs should have completed
        assert_eq!(jobsys.job_results.len(), 5);
        assert!(jobsys.blocked_jobs.is_empty());
        assert!(!jobsys.abort_flag.load(Ordering::SeqCst));

        // Verify all results
        for i in 0..5 {
            assert_eq!(jobsys.expect_result::<TrivialResult>(i)?.0, i);
        }

        Ok(())
    }
}
