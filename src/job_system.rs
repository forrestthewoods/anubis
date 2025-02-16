use core::{num, sync};
use crossbeam::channel::RecvTimeoutError;
use dashmap::DashMap;
use downcast_rs::{impl_downcast, DowncastSync};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub type JobId = i64;

pub type JobFn = dyn Fn() -> anyhow::Result<Box<dyn JobResult>> + Send + Sync + 'static;

pub struct Job {
    id: JobId,
    job_fn: Box<JobFn>,
    result: Option<anyhow::Result<Box<dyn JobResult>>>,
    depends_on: HashSet<JobId>,
    blocks: HashSet<JobId>,
}

impl Job {
    pub fn new(id: JobId, job_fn: Box<JobFn>) -> Self {
        Job {
            id,
            job_fn,
            result: None,
            depends_on: Default::default(),
            blocks: Default::default(),
        }
    }

    pub fn depend_on(&mut self, other: &mut Job) {
        self.depends_on.insert(other.id);
        other.blocks.insert(self.id);
    }
}

#[derive(Default)]
#[repr(u8)]
pub enum JobSystemStatus {
    #[default]
    Idle,
    Running,
    Succeeded,
    Failed,
}

#[derive(Default)]
pub struct JobSystem {
    pub status: std::sync::atomic::AtomicU8,
    pub next_job_id: Arc<AtomicI64>,
    pub blocked_jobs: DashMap<JobId, Job>,
    pub job_results: DashMap<JobId, anyhow::Result<Box<dyn JobResult>>>,
}

#[derive(Clone)]
pub struct JobContext {
    pub next_id: Arc<AtomicI64>,
    pub sender: crossbeam::channel::Sender<Job>,
    pub receiver: crossbeam::channel::Receiver<Job>,
}

impl JobSystem {
    pub fn next_id(&self) -> i64 {
        self.next_job_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn run_to_completion(
        &self,
        num_workers: usize,
        initial_jobs: impl Iterator<Item = Job>,
    ) -> anyhow::Result<()> {
        let (tx, rx) = crossbeam::channel::unbounded::<Job>();

        // Seed jobs
        for job in initial_jobs {
            if job.depends_on.is_empty() {
                tx.send(job)?;
            } else {
                self.blocked_jobs.insert(job.id, job);
            }
        }

        let idle_workers = Arc::new(AtomicUsize::new(0));

        // Create N workers
        std::thread::scope(|scope| {
            for _ in 0..num_workers {
                let work_rx = rx.clone();
                let idle_workers = idle_workers.clone();

                let job_results = &self.job_results;
                let status = &self.status;
                scope.spawn(move || {
                    let mut idle = false;

                    // Loop until complete
                    loop {
                        // If the system has failed, exit early
                        if status.load(Ordering::SeqCst) == JobSystemStatus::Failed as u8 {
                            break;
                        }

                        // Get next job
                        match work_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(job) => {
                                if idle {
                                    idle = false;
                                    idle_workers.fetch_sub(1, Ordering::SeqCst);
                                }

                                // Execute job and store result
                                let job_result = (job.job_fn)();
                                let is_err = job_result.is_err();
                                job_results.insert(job.id, job_result);

                                if is_err {
                                    status.store(JobSystemStatus::Failed as u8, Ordering::SeqCst);
                                }
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if !idle {
                                    idle = true;
                                    idle_workers.fetch_add(1, Ordering::SeqCst);
                                }

                                // Timeout: check if jobsys is complete, otherwise loop and get a new job
                                if idle_workers.load(Ordering::SeqCst) == num_workers && work_rx.is_empty() {
                                    break;
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                });
            }
        });

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
}

struct JobWorker {
    next_job_id: Arc<AtomicI64>,
    job_intake: crossbeam::channel::Sender<Job>,
    job_queue: crossbeam::channel::Receiver<Job>,
}

fn do_stuff() {
    let js: JobSystem = Default::default();
}

pub trait JobResult: DowncastSync + Send + Sync + 'static {}
impl_downcast!(sync JobResult);

// build a target
// create a job system
// create a job cache
// create a build rule job
// look-up function to build rule
// creates list sub-jobs
// creates new job with dependency on subjobs
// this subjob writes its output to the original job

// need to create a hash for a job
// job hash:
// rule: target + vars?
// compile_obj: file + vars?
// job can be queued, processing, completed, failed, depfailed

// TODO: move to tests later
pub struct TrivialResult(pub i64);
impl JobResult for TrivialResult {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_job() -> anyhow::Result<()> {
        let jobsys: JobSystem = Default::default();
        let job = Job::new(jobsys.next_id(), Box::new(|| Ok(Box::new(TrivialResult(42)))));
        
        let num_cpus = num_cpus::get_physical();
        jobsys.run_to_completion(num_cpus, [job].into_iter())?;
    
        let result = jobsys.expect_result::<TrivialResult>(0)?;
        assert_eq!(result.0, 42);
    
        Ok(())
    }
}
