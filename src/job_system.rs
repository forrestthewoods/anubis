use core::{num, sync};
use dashmap::DashMap;
use downcast_rs::{impl_downcast, DowncastSync};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::Arc;

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
    pub job_intake: Option<crossbeam::channel::Receiver<Job>>,
    pub blocked_jobs: DashMap<JobId, Job>,
    pub job_queue: Option<crossbeam::channel::Sender<Job>>,
    pub job_results: DashMap<JobId, anyhow::Result<Box<dyn JobResult>>>,
}

impl JobSystem {
    pub fn next_id(&self) -> i64 {
        self.next_job_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn run_to_completion(
        &self,
        num_workers: i64,
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

        let active_workers = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..num_workers {
                let work_rx = rx.clone();
                let active_workers = active_workers.clone();
                // Capture self by reference since we are in a thread::scope
                let job_results = &self.job_results;
                let status = &self.status;
                scope.spawn(move || {
                    use crossbeam::channel::RecvTimeoutError;
                    use std::time::Duration;
                    loop {
                        // If the system has failed, exit early
                        if status.load(Ordering::SeqCst) == JobSystemStatus::Failed as u8 {
                            break;
                        }
                        match work_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(job) => {
                                active_workers.fetch_add(1, Ordering::SeqCst);
                                let job_result = (job.job_fn)();
                                let is_err = job_result.is_err();
                                job_results.insert(job.id, job_result);
                                if is_err {
                                    status.store(JobSystemStatus::Failed as u8, Ordering::SeqCst);
                                }
                                active_workers.fetch_sub(1, Ordering::SeqCst);
                            },
                            Err(RecvTimeoutError::Timeout) => {
                                if active_workers.load(Ordering::SeqCst) == 0 && work_rx.is_empty() {
                                    break;
                                }
                            },
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                });
            }
        });

        Ok(())
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
    // A dummy implementation of JobResult for testing.



    #[test]
    fn trivial_job() -> anyhow::Result<()> {
        let jobsys: JobSystem = Default::default();
        let job = Job::new(jobsys.next_id(), Box::new(|| Ok(Box::new(TrivialResult(42)))));

        let jobs = [job];

        jobsys.run_to_completion(1, jobs.into_iter())?;

        Ok(())
    }

    type Test = dyn JobResult + Send + Sync + 'static;

    fn create_dummy_job(id: JobId) -> Job {
        Job {
            id,
            job_fn: Box::new(|| Ok(Box::new(TrivialResult(0)) as Box<Test>)),
            result: None,
            depends_on: HashSet::new(),
            blocks: HashSet::new(),
        }
    }
}
