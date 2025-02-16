use core::sync;
use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

pub type JobId = i64;

pub struct Job {
    id: JobId,
    job_fn: Box<dyn Fn() -> anyhow::Result<Box<dyn JobResult>>>,
    result: Option<anyhow::Result<Box<dyn JobResult>>>,
    depends_on: HashSet<JobId>,
    blocks: HashSet<JobId>,
}

impl Job {
    fn depend_on(&mut self, other: &mut Job) {
        self.depends_on.insert(other.id);
        other.blocks.insert(self.id);
    }
}

#[derive(Default)]
pub enum JobSystemStatus {
    #[default]
    Idle,
    Running,
    Succeeded,
    Failed,
}

#[derive(Default)]
pub struct JobSystem {
    status: JobSystemStatus,
    next_job_id: Arc<AtomicI64>,
    job_intake: Option<crossbeam::channel::Receiver<Job>>,
    blocked_jobs: DashMap<JobId, Job>,
    job_queue: Option<crossbeam::channel::Sender<Job>>,
    job_results: DashMap<JobId, anyhow::Result<Box<dyn JobResult>>>,
}

impl JobSystem {
    fn run_to_completion(&self, num_workers: i64, initial_jobs: impl Iterator<Item=Job>) -> anyhow::Result<()> {
        let (tx, rx) = crossbeam::channel::unbounded::<Job>();

        for job in initial_jobs.into_iter() {
            if job.depends_on.is_empty() {
                tx.send(job)?;
            } else {
                self.blocked_jobs.insert(job.id, job);
            }
        }

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

pub trait JobResult {}

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

#[cfg(test)]
mod tests {
    use super::*;
    // A dummy implementation of JobResult for testing.

    struct TrivialResult(i64);
    impl JobResult for TrivialResult {}

    #[test]
    fn trivial_job() {
        let jobsys: JobSystem = Default::default();
    }
}
