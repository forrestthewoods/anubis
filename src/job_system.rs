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
  Deferred(Job),
  Error(anyhow::Error),
  Success(Box<dyn JobResult>),
}

// Info for a job
pub struct Job {
  id: JobId,
  desc: String,
  job_fn: Option<Box<JobFn>>,
  depends_on: HashSet<JobId>, // TODO: remove
  blocks: HashSet<JobId>,     // TODO: remove
}

// Central hub for JobSystem
#[derive(Default)]
pub struct JobSystem {
  pub abort_flag: AtomicBool,
  pub next_job_id: Arc<AtomicI64>,
  pub blocked_jobs: DashMap<JobId, Job>,
  pub job_results: DashMap<JobId, anyhow::Result<Box<dyn JobResult>>>,
  pub job_infos: Arc<Mutex<HashMap<JobId, JobInfo>>>,
}

// JobInfo: defines the "graph" of job dependencies
#[derive(Default)]
pub struct JobInfo {
  pub job_id: JobId,
  pub finished: bool,
  pub depends_on: HashSet<JobId>,
  pub blocks: HashSet<JobId>,
}

// Context obj passed into job fn
#[derive(Clone)]
pub struct JobContext {
  pub next_id: Arc<AtomicI64>,
  pub sender: crossbeam::channel::Sender<Job>,
  pub receiver: crossbeam::channel::Receiver<Job>,
  pub job_infos: Arc<Mutex<HashMap<JobId, JobInfo>>>,
}

impl std::fmt::Debug for Job {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Job")
      .field("id", &self.id)
      .field("desc", &self.desc)
      .field("depends_on", &self.depends_on)
      .field("blocks", &self.blocks)
      .finish()
  }
}

impl Job {
  pub fn new(id: JobId, desc: String, job_fn: Box<JobFn>) -> Self {
    Job { id, desc, job_fn: Some(job_fn), depends_on: Default::default(), blocks: Default::default() }
  }

  pub fn depend_on(&mut self, other: &mut Job) {
    self.depends_on.insert(other.id);
    other.blocks.insert(self.id);
  }
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

    let job_context = JobContext {
      next_id: self.next_job_id.clone(),
      sender: tx.clone(),
      receiver: rx.clone(),
      job_infos: self.job_infos.clone(),
    };

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
        let job_context = job_context.clone();
        let idle_workers = idle_workers.clone();

        let job_results = &self.job_results;
        let abort_flag = &self.abort_flag;
        scope.spawn(move || {
          let mut idle = false;

          // Loop until complete or abort
          while !abort_flag.load(Ordering::SeqCst) {
            // Get next job
            match job_context.receiver.recv_timeout(Duration::from_millis(100)) {
              Ok(mut job) => {
                assert!(job.depends_on.is_empty());

                if idle {
                  idle = false;
                  idle_workers.fetch_sub(1, Ordering::SeqCst);
                }

                // // Execute job and store result
                let job_id = job.id;
                let children = job.blocks.clone(); // This is an extra copy
                let job_fn = job.job_fn.take().unwrap();
                let job_result = job_fn(job, &job_context);

                match job_result {
                  JobFnResult::Deferred(deferred_job) => {
                    // Ensure deferred job has dependencies
                    if !deferred_job.depends_on.is_empty() {
                      self.blocked_jobs.insert(deferred_job.id, deferred_job);
                    } else {
                      // Error! Deferred job must have dependencies
                      self.job_results.insert(
                        deferred_job.id,
                        anyhow::Result::Err(anyhow::anyhow!("Deferred job had no dependencies")),
                      );

                      // Abort!
                      abort_flag.store(true, Ordering::SeqCst);
                    }
                  }
                  JobFnResult::Error(e) => {
                    // Store error
                    self.job_results.insert(job_id, anyhow::Result::Err(e));

                    // Abort everything
                    abort_flag.store(true, Ordering::SeqCst);
                  }
                  JobFnResult::Success(result) => {
                    // Store result
                    self.job_results.insert(job_id, Ok(result));

                    // Notify children this job is complete
                    for child_id in children {
                      if let Some((_, child)) = self.blocked_jobs.remove_if_mut(&child_id, |_, child| {
                        child.depends_on.remove(&job_id);
                        child.depends_on.is_empty()
                      }) {
                        // Child is no longer blocked. Add to work queue.
                        job_context.sender.send(child).unwrap();
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
                if idle_workers.load(Ordering::SeqCst) == num_workers && job_context.receiver.is_empty() {
                  break;
                }
              }
              Err(RecvTimeoutError::Disconnected) => break,
            }
          }
        });
      }
    });

    if !self.blocked_jobs.is_empty() {
      anyhow::bail!(
        "JobSystem finished but had [{}] jobs that weren't finished. [{:?}]",
        self.blocked_jobs.len(),
        self.blocked_jobs
      );
    }

    // Success!
    Ok(())
  }

  pub fn expect_result<T: JobResult>(&self, job_id: JobId) -> anyhow::Result<T> {
    if let Some((_, res)) = self.job_results.remove(&job_id) {
      let boxed_result = res?;
      boxed_result.downcast::<T>().map(|boxed| *boxed).map_err(|_| {
        anyhow::anyhow!("Job result for job id {} could not be cast to the expected type", job_id)
      })
    } else {
      let mut errors = Vec::new();
      for entry in self.job_results.iter() {
        if let Err(err) = entry.value() {
          errors.push(format!("Job id {}: {}", entry.key(), err));
        }
      }
      if errors.is_empty() {
        Err(anyhow::anyhow!("No job result found for job id {} and no job errors recorded", job_id))
      } else {
        Err(anyhow::anyhow!("Aggregated job errors: {}", errors.join("; ")))
      }
    }
  }

  pub fn any_errors(&self) -> bool {
    self.job_results.iter().any(|r| r.is_err())
  }

  pub fn get_errors(&mut self) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    // Collect the keys of entries that contain an error.
    let error_keys: Vec<JobId> =
      self.job_results.iter().filter(|entry| entry.value().is_err()).map(|entry| *entry.key()).collect();

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
    let jobsys: JobSystem = Default::default();
    let job = Job::new(
      jobsys.next_id(),
      "TrivialJob".to_owned(),
      Box::new(|_, _| JobFnResult::Success(Box::new(TrivialResult(42)))),
    );

    jobsys.run_to_completion(1, [job].into_iter())?;

    let result = jobsys.expect_result::<TrivialResult>(0)?;
    assert_eq!(result.0, 42);

    Ok(())
  }

  #[test]
  fn basic_dependency() -> anyhow::Result<()> {
    let jobsys: JobSystem = Default::default();

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
    b.depend_on(&mut a);

    // Run jobs
    // Note we pass job_b before job_a
    jobsys.run_to_completion(1, [b, a].into_iter())?;

    // Ensure both jobs successfully completed with the given value
    assert_eq!(jobsys.expect_result::<TrivialResult>(0).unwrap(), TrivialResult(42));
    assert_eq!(jobsys.expect_result::<TrivialResult>(1).unwrap(), TrivialResult(1337));
    assert_eq!(jobsys.any_errors(), false);

    Ok(())
  }

  #[test]
  fn basic_dynamic_dependency() -> anyhow::Result<()> {
    let mut jobsys: JobSystem = Default::default();

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

        // "This" job now depends on the new job
        job.depend_on(&mut dep_job);
        job.job_fn = Some(Box::new(|job, ctx| JobFnResult::Success(Box::new(TrivialResult(42)))));

        // Send the new jobs
        ctx.sender.send(dep_job).unwrap();

        // Defer this job
        JobFnResult::Deferred(job)
      }),
    );

    jobsys.run_to_completion(2, [a].into_iter())?;
    assert_eq!(jobsys.abort_flag.load(Ordering::SeqCst), false);
    let errors = jobsys.get_errors();
    assert_eq!(errors.len(), 0, "Errors: {:?}", errors);
    assert_eq!(jobsys.job_results.len(), 2);
    assert_eq!(jobsys.expect_result::<TrivialResult>(0).unwrap(), TrivialResult(42));
    assert_eq!(jobsys.expect_result::<TrivialResult>(1).unwrap(), TrivialResult(1337));

    Ok(())
  }
}
