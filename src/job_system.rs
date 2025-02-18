use core::{num, sync};
use crossbeam::channel::RecvTimeoutError;
use dashmap::DashMap;
use downcast_rs::{impl_downcast, DowncastSync};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::job_system;

// ID for jobs
pub type JobId = i64;

// Function that does the actual work of a job
pub type JobFn = dyn Fn(Job, &JobContext) -> JobFnResult + Send + Sync + 'static;

// Trait to help with void* dynamic casts
pub trait JobResult: DowncastSync + Send + Sync + 'static {}
impl_downcast!(sync JobResult);

// Return value of a JobFn
pub enum JobFnResult {
    Deferred(JobDeferral),
    Error(anyhow::Error),
    Success(Box<dyn JobResult>),
}

// Info for a job
pub struct Job {
    id: JobId,
    desc: String,
    job_fn: Option<Box<JobFn>>,
}

// Central hub for JobSystem
#[derive(Default)]
pub struct JobSystem {
    pub abort_flag: AtomicBool,
    pub next_job_id: Arc<AtomicI64>,
    pub blocked_jobs: DashMap<JobId, Job>,
    pub job_results: DashMap<JobId, anyhow::Result<Box<dyn JobResult>>>,
    pub job_graph: Arc<Mutex<JobGraph>>,
}

// JobInfo: defines the "graph" of job dependencies
#[derive(Default)]
pub struct JobGraphNode {
    pub job_id: JobId,
    pub finished: bool,
    pub depends_on: HashSet<JobId>,
    pub blocks: HashSet<JobId>,
}

#[derive(Default)]
pub struct JobGraph {
    pub blocked_by: HashMap<JobId, HashSet<JobId>>,
    pub blocks: HashMap<JobId, HashSet<JobId>>,
}

pub struct JobGraphEdge {
    blocked: JobId,
    blocker: JobId,
}

pub struct JobDeferral {
    new_jobs: Vec<Job>,
    graph_updates: Vec<JobGraphEdge>,
}

// Context obj passed into job fn
#[derive(Clone)]
pub struct JobContext {
    pub next_id: Arc<AtomicI64>,
}

// Context obj for workers
#[derive(Clone)]
pub struct WorkerContext {
    pub sender: crossbeam::channel::Sender<Job>,
    pub receiver: crossbeam::channel::Receiver<Job>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").field("id", &self.id).field("desc", &self.desc).finish()
    }
}

impl Job {
    pub fn new(id: JobId, desc: String, job_fn: Box<JobFn>) -> Self {
        Job {
            id,
            desc,
            job_fn: Some(job_fn),
        }
    }
}

impl JobSystem {
    pub fn next_id(&self) -> i64 {
        self.next_job_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn run_to_completion(
        job_sys: Arc<JobSystem>,
        num_workers: usize,
        initial_edges: Vec<JobGraphEdge>,
        initial_jobs: Vec<Job>,
    ) -> anyhow::Result<()> {
        let (tx, rx) = crossbeam::channel::unbounded::<Job>();

        let job_context = JobContext {
            next_id: job_sys.next_job_id.clone(),
        };

        let worker_context = WorkerContext {
            sender: tx.clone(),
            receiver: rx.clone(),
        };

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
                graph.blocked_by.entry(edge.blocked).or_default().insert(edge.blocker);
                graph.blocks.entry(edge.blocker).or_default().insert(edge.blocked);
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

        handle_new_jobs(&job_sys, initial_jobs, &initial_edges, &tx)?;

        let idle_workers = Arc::new(AtomicUsize::new(0));

        // Create N workers
        std::thread::scope(|scope| {
            for _ in 0..num_workers {
                let worker_context = worker_context.clone();
                let job_context = job_context.clone();
                let idle_workers = idle_workers.clone();
                let job_sys = job_sys.clone();

                scope.spawn(move || {
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

                                // // Execute job and store result
                                let job_id = job.id;
                                let job_fn = job.job_fn.take().unwrap();
                                let job_result = job_fn(job, &job_context);

                                match job_result {
                                    JobFnResult::Deferred(deferral) => {
                                        // TODO: handle error
                                        handle_new_jobs(
                                            &job_sys,
                                            deferral.new_jobs,
                                            &deferral.graph_updates,
                                            &worker_context.sender,
                                        )
                                        .unwrap();
                                    }
                                    JobFnResult::Error(e) => {
                                        // Store error
                                        job_sys.job_results.insert(job_id, anyhow::Result::Err(e));

                                        // Abort everything
                                        job_sys.abort_flag.store(true, Ordering::SeqCst);
                                    }
                                    JobFnResult::Success(result) => {
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
                                                            worker_context
                                                                .sender
                                                                .send(unblocked_job)
                                                                .unwrap();
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
                });
            }
        });

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

    pub fn expect_result<T: JobResult>(&self, job_id: JobId) -> anyhow::Result<T> {
        if let Some((_, res)) = self.job_results.remove(&job_id) {
            let boxed_result = res?;
            boxed_result.downcast::<T>().map(|boxed| *boxed).map_err(|_| {
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

    pub fn any_errors(&self) -> bool {
        self.job_results.iter().any(|r| r.is_err())
    }

    pub fn get_errors(&self) -> Vec<anyhow::Error> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    pub struct TrivialResult(pub i64);
    impl JobResult for TrivialResult {}

    #[test]
    fn trivial_job() -> anyhow::Result<()> {
        let jobsys: Arc<JobSystem> = Default::default();
        let job = Job::new(
            jobsys.next_id(),
            "TrivialJob".to_owned(),
            Box::new(|_, _| JobFnResult::Success(Box::new(TrivialResult(42)))),
        );

        JobSystem::run_to_completion(jobsys.clone(), 1, vec![], vec![job])?;

        let result = jobsys.expect_result::<TrivialResult>(0)?;
        assert_eq!(result.0, 42);

        Ok(())
    }

    #[test]
    fn basic_dependency() -> anyhow::Result<()> {
        let jobsys: Arc<JobSystem> = Default::default();

        let mut flag = Arc::new(AtomicBool::new(false));

        // Create job A
        let a_flag = flag.clone();
        let mut a = Job::new(
            jobsys.next_id(),
            "job_a".to_owned(),
            Box::new(move |_, _| {
                a_flag.store(true, Ordering::SeqCst);
                JobFnResult::Success(Box::new(TrivialResult(42)))
            }),
        );

        // Create job B
        let b_flag = flag.clone();
        let mut b = Job::new(
            jobsys.next_id(),
            "job_b".to_owned(),
            Box::new(move |_, _| {
                if b_flag.load(Ordering::SeqCst) {
                    JobFnResult::Success(Box::new(TrivialResult(1337)))
                } else {
                    JobFnResult::Error(anyhow::anyhow!("Job B expected flag to be set by Job A"))
                }
            }),
        );

        // B must run after A
        //b.depend_on(&mut a);

        // Run jobs
        // Note we pass job_b before job_a
        JobSystem::run_to_completion(
            jobsys.clone(),
            1,
            vec![JobGraphEdge {
                blocker: a.id,
                blocked: b.id,
            }],
            vec![b, a],
        )?;

        //jobsys.run_to_completion(1, [b, a].into_iter())?;

        // Ensure both jobs successfully completed with the given value
        assert_eq!(
            jobsys.expect_result::<TrivialResult>(0).unwrap(),
            TrivialResult(42)
        );
        assert_eq!(
            jobsys.expect_result::<TrivialResult>(1).unwrap(),
            TrivialResult(1337)
        );
        assert_eq!(jobsys.any_errors(), false);

        Ok(())
    }

    #[test]
    fn basic_dynamic_dependency() -> anyhow::Result<()> {
        let jobsys: Arc<JobSystem> = Default::default();

        let a = Job::new(
            jobsys.next_id(),
            "job a".to_owned(),
            Box::new(|mut job: Job, ctx: &JobContext| {
                // Create a new dependent job
                let mut dep_job = Job::new(
                    ctx.next_id.fetch_add(1, Ordering::SeqCst),
                    "child job".to_owned(),
                    Box::new(|job, ctx| JobFnResult::Success(Box::new(TrivialResult(1337)))),
                );

                let mut edges = vec![JobGraphEdge {
                    blocker: dep_job.id,
                    blocked: job.id,
                }];

                // New fn for "this" job
                job.job_fn = Some(Box::new(|job, ctx| {
                    JobFnResult::Success(Box::new(TrivialResult(42)))
                }));

                JobFnResult::Deferred(JobDeferral {
                    new_jobs: vec![dep_job, job],
                    graph_updates: edges,
                })
            }),
        );

        // Run jobs
        JobSystem::run_to_completion(jobsys.clone(), 2, vec![], vec![a])?;

        // Verify results
        assert_eq!(jobsys.abort_flag.load(Ordering::SeqCst), false);
        let errors = jobsys.get_errors();
        assert_eq!(errors.len(), 0, "Errors: {:?}", errors);
        assert_eq!(jobsys.job_results.len(), 2);
        assert_eq!(
            jobsys.expect_result::<TrivialResult>(0).unwrap(),
            TrivialResult(42)
        );
        assert_eq!(
            jobsys.expect_result::<TrivialResult>(1).unwrap(),
            TrivialResult(1337)
        );

        Ok(())
    }
}
