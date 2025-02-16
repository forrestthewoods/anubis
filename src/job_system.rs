use core::{num, sync};
use dashmap::DashMap;
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
    status: std::sync::atomic::AtomicU8,
    next_job_id: Arc<AtomicI64>,
    job_intake: Option<crossbeam::channel::Receiver<Job>>,
    blocked_jobs: DashMap<JobId, Job>,
    job_queue: Option<crossbeam::channel::Sender<Job>>,
    job_results: DashMap<JobId, anyhow::Result<Box<dyn JobResult>>>,
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
        for job in initial_jobs.into_iter() {
            if job.depends_on.is_empty() {
                tx.send(job)?;
            } else {
                self.blocked_jobs.insert(job.id, job);
            }
        }

        let work_rx = rx.clone();
        let do_work = move || {
            loop {
                // Check system for error
                if self.status.load(Ordering::SeqCst) == JobSystemStatus::Failed as u8 {
                    break;
                }

                // Get next job
                if let Ok(job) = work_rx.recv() {
                    // Run job
                    let job_result = (job.job_fn)();
                    let is_err = job_result.is_err();

                    // Store job results
                    self.job_results.insert(job.id, job_result);

                    // Update jobsys on error
                    if is_err {
                        self.status.store(JobSystemStatus::Failed as u8, Ordering::SeqCst);
                    }
                } else {
                    // Receiver errored
                    break;
                }
            }
        };

        // Create workers
        std::thread::scope(|scope| {
            for _ in 0..num_workers {
                scope.spawn(do_work.clone());
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

pub trait JobResult: Send + Sync + 'static {}

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
