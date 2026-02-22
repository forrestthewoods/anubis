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
use crate::progress::ProgressEvent;
use crate::util::format_duration;
use crate::{anubis, job_system, toolchain};
use crate::{anyhow_loc, anyhow_with_context, bail_loc, bail_with_context, timed_span};

// ----------------------------------------------------------------------------
// Declarations
// ----------------------------------------------------------------------------

// ID for jobs
pub type JobId = i64;

// Function that does the actual work of a job
pub type JobFn = dyn FnOnce(Job) -> anyhow::Result<JobOutcome> + Send + Sync + 'static;

// Trait to help with void* dynamic casts
pub trait JobArtifact: DowncastSync + Debug + Send + Sync + 'static {}
impl_downcast!(sync JobArtifact);

// Return value of a JobFn
pub enum JobOutcome {
    Deferred(JobDeferral),
    Success(Arc<dyn JobArtifact>),
}

/// Structured display metadata for a job.
/// Produced by rules at job-creation time so the progress display never needs
/// to reverse-engineer free-form description strings.
#[derive(Clone, Debug)]
pub struct JobDisplayInfo {
    /// Present participle for display: "Compiling", "Linking", "Archiving", etc.
    pub verb: &'static str,
    /// Short name for info level: "main.cpp", "simple_cpp"
    pub short_name: String,
    /// Verbose detail for debug level: "/full/path/to/main.cpp"
    pub detail: String,
}

impl JobDisplayInfo {
    /// Create a minimal display info from just a description string.
    /// Used by tests and internal jobs that don't need fancy display.
    pub fn from_desc(desc: &str) -> Self {
        JobDisplayInfo {
            verb: "Running",
            short_name: desc.to_string(),
            detail: desc.to_string(),
        }
    }
}

// Info for a job
pub struct Job {
    pub id: JobId,
    pub desc: String,
    pub display: JobDisplayInfo,
    pub ctx: Arc<JobContext>,
    pub job_fn: Option<Box<JobFn>>,
}

// Central hub for JobSystem
pub struct JobSystem {
    pub next_id: Arc<AtomicI64>,
    pub(crate) abort_flag: AtomicBool,
    pub(crate) blocked_jobs: DashMap<JobId, Job>,
    job_graph: Arc<Mutex<JobGraph>>,
    pub(crate) job_results: DashMap<JobId, anyhow::Result<Arc<dyn JobArtifact>>>,
    /// Maps continuation job IDs to original job IDs for result propagation.
    /// When a continuation job completes, its result is copied to the original job.
    result_propagation: DashMap<JobId, JobId>,
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
    pub continuation_job: Job,
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
    pub fn new(id: JobId, desc: String, display: JobDisplayInfo, ctx: Arc<JobContext>, job_fn: Box<JobFn>) -> Self {
        Job {
            id,
            desc,
            display,
            ctx,
            job_fn: Some(job_fn),
        }
    }

}

impl JobContext {
    pub fn get_next_id(&self) -> i64 {
        self.job_system.next_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn new_job(self: &Arc<JobContext>, desc: String, display: JobDisplayInfo, f: Box<JobFn>) -> Job {
        Job::new(self.get_next_id(), desc, display, self.clone(), f)
    }

    pub fn new_job_with_id(self: &Arc<JobContext>, id: i64, desc: String, display: JobDisplayInfo, f: Box<JobFn>) -> Job {
        assert!(id < self.job_system.next_id.load(Ordering::SeqCst));
        Job::new(id, desc, display, self.clone(), f)
    }

    pub fn with_mode(&self, mode: Arc<toolchain::Mode>) -> anyhow::Result<Self> {
        let cur_mode = self.mode.as_ref().ok_or_else(|| anyhow_loc!("JobContext::with_mode requires a toolchain"))?; 
        if Arc::ptr_eq(&cur_mode, &mode) {
            Ok(self.clone())
        } else {
            let cur_toolchain = self.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain"))?;
            let new_toolchain = self.anubis.get_toolchain(&*mode, &cur_toolchain.target)?;
            Ok(JobContext {
                anubis: self.anubis.clone(),
                job_system: self.job_system.clone(),
                mode: Some(mode),
                toolchain: Some(new_toolchain)
            })
        }
    }

}

impl dyn JobArtifact {
    pub fn cast<T: JobArtifact>(self: &Arc<dyn JobArtifact>) -> anyhow::Result<Arc<T>> {
        self.clone().downcast_arc::<T>().map_err(|v| {
            anyhow_loc!(
                "Could not cast JobResult to {}. Actual type was {}",
                std::any::type_name::<T>(),
                std::any::type_name_of_val(v.as_any())
            )
        })
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
            result_propagation: Default::default(),
            tx,
            rx,
        }
    }

    pub fn add_job(&self, job: Job) -> anyhow::Result<JobId> {
        tracing::trace!("Adding job [{}] [{}]", job.id, &job.desc);
        let job_id = job.id;
        self.tx.send(job)?;
        Ok(job_id)
    }

    pub fn add_job_with_deps(&self, job: Job, deps: &[JobId]) -> anyhow::Result<JobId> {
        if deps.is_empty() {
            return self.add_job(job);
        }
        
        tracing::trace!("Adding job [{}] [{}] with deps: {:?}", job.id, &job.desc, deps);

        let job_id = job.id;
        let job_desc = job.desc.clone();

        // Update graph and insert into blocked_jobs atomically while holding the lock.
        // This prevents a race condition where a completing dependency could try to
        // unblock this job before it's been inserted into blocked_jobs.
        let send_to_queue = {
            let mut job_graph = self.job_graph.lock().unwrap();

            let mut blocked = false;

            // Update blocked_by
            for &dep in deps {
                // See if job we're blocked by has already finished
                if let Some(dep_result) = self.job_results.get(&dep) {
                    match dep_result.value() {
                        Ok(_) => continue,
                        Err(e) => bail_loc!(
                            "Job [{}] can't be added because dep [{}] failed with [{}]",
                            job_desc,
                            dep,
                            e
                        ),
                    }
                }

                blocked = true;
                job_graph.blocks.entry(dep).or_default().insert(job_id);
                job_graph.blocked_by.entry(job_id).or_default().insert(dep);
            }

            // Insert into blocked_jobs BEFORE releasing the lock to prevent race condition
            if blocked {
                self.blocked_jobs.insert(job_id, job);
                None
            } else {
                Some(job)
            }
        };

        // Send job to work queue if not blocked (after releasing the lock to avoid
        // unnecessary contention on the channel send)
        if let Some(job) = send_to_queue {
            self.tx.send(job)?;
        }
        Ok(job_id)
    }

    pub fn run_to_completion(
        job_sys: Arc<JobSystem>,
        num_workers: usize,
        progress_tx: crossbeam::channel::Sender<ProgressEvent>,
    ) -> anyhow::Result<()> {
        tracing::debug!("Starting job system with {} workers", num_workers);

        let execution_start = std::time::Instant::now();
        let (tx, rx) = (job_sys.tx.clone(), job_sys.rx.clone());

        let worker_context = WorkerContext {
            sender: tx.clone(),
            receiver: rx.clone(),
        };

        let idle_workers = Arc::new(AtomicUsize::new(0));

        // Create N workers
        std::thread::scope(|scope| {
            for worker_id in 0..num_workers {
                let worker_context = worker_context.clone();
                let idle_workers = idle_workers.clone();
                let job_sys = job_sys.clone();
                let progress_tx = progress_tx.clone();

                scope.spawn(move || {
                    let _worker_span = tracing::info_span!("worker", id = worker_id).entered();
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
                                    let job_display = job.display.clone();
                                    let job_fn = job.job_fn.take().ok_or_else(|| {
                                        anyhow_loc!("Job [{}:{}] missing job fn", job.id, job.desc)
                                    })?;

                                    // Notify progress display that this worker started a job
                                    let _ = progress_tx.send(ProgressEvent::JobStarted {
                                        worker_id,
                                        job_id,
                                        display: job_display.clone(),
                                    });

                                    let job_start = std::time::Instant::now();
                                    let job_result = {
                                        let _job_span = tracing::info_span!("job", id = job_id, desc = %job_desc).entered();
                                        tracing::debug!("Running job: [{}] {}", job_id, job_desc);
                                        job_fn(job)
                                    };
                                    let job_duration = std::time::Instant::now() - job_start;

                                    match job_result {
                                        Ok(JobOutcome::Deferred(deferral)) => {
                                            let continuation_id = deferral.continuation_job.id;
                                            tracing::trace!(
                                                "Job [{}] [{}] deferred to continuation [{}] [{}], waiting for: {:?}",
                                                job_id, &job_desc,
                                                continuation_id, &deferral.continuation_job.desc,
                                                deferral.blocked_by
                                            );

                                            // Track that continuation's result should propagate to original job
                                            job_sys.result_propagation.insert(continuation_id, job_id);

                                            // Notify progress: worker is now free (deferred job spawned children)
                                            let _ = progress_tx.send(ProgressEvent::WorkerIdle { worker_id });

                                            job_sys.add_job_with_deps(
                                                deferral.continuation_job,
                                                &deferral.blocked_by,
                                            )?;
                                        }
                                        Ok(JobOutcome::Success(result)) => {
                                            tracing::debug!("Job [{}] completed in [{}]: [{}]", job_id, format_duration(job_duration), &job_desc);

                                            // Notify progress display
                                            let _ = progress_tx.send(ProgressEvent::JobCompleted {
                                                worker_id,
                                                job_id,
                                                display: job_display.clone(),
                                                duration: job_duration,
                                            });

                                            // Store result for this job
                                            job_sys.job_results.insert(job_id, Ok(result.clone()));

                                            // Collect all job IDs that need to have their dependents unblocked
                                            // This includes the completing job and any jobs it propagates to
                                            let mut jobs_to_unblock = vec![job_id];

                                            // Propagate result through any chain of continuations
                                            // (handles multi-level deferrals: A -> B -> C)
                                            let mut current_id = job_id;
                                            while let Some((_, original_job_id)) =
                                                job_sys.result_propagation.remove(&current_id)
                                            {
                                                tracing::trace!(
                                                    "Propagating result from continuation [{}] to original job [{}]",
                                                    current_id, original_job_id
                                                );
                                                job_sys.job_results.insert(original_job_id, Ok(result.clone()));
                                                jobs_to_unblock.push(original_job_id);
                                                current_id = original_job_id;
                                            }

                                            // Notify blocked_jobs that all these jobs are complete
                                            let mut graph = job_sys.job_graph.lock().unwrap();
                                            for finished_job in jobs_to_unblock {
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
                                        Err(e) => {
                                            tracing::error!("Job failed: [{}] [{}]: {}", job_id, &job_desc, e);

                                            // Notify progress display
                                            let _ = progress_tx.send(ProgressEvent::JobFailed {
                                                worker_id,
                                                job_id,
                                                display: job_display.clone(),
                                                error_output: e.to_string(),
                                            });

                                            // Store error
                                            let s = e.to_string();
                                            let job_result: anyhow::Result<Arc<dyn JobArtifact>> = anyhow::Result::Err(e).context(format!(
                                                "Job Failed:\n    Desc: {}\n    Err:{}",
                                                job_desc, s
                                            ));
                                            job_sys.job_results.insert(job_id, job_result);

                                            // Propagate error through any chain of continuations
                                            // (handles multi-level deferrals: A -> B -> C)
                                            let mut current_id = job_id;
                                            while let Some((_, original_job_id)) =
                                                job_sys.result_propagation.remove(&current_id)
                                            {
                                                tracing::error!(
                                                    "Propagating error from continuation [{}] to original job [{}]",
                                                    current_id, original_job_id
                                                );
                                                job_sys.job_results.insert(
                                                    original_job_id,
                                                    Err(anyhow_loc!(
                                                        "Original job [{}] failed because continuation job [{}] failed",
                                                        original_job_id, job_id
                                                    ))
                                                );
                                                current_id = original_job_id;
                                            }

                                            // Abort everything
                                            job_sys.abort_flag.store(true, Ordering::SeqCst);
                                        }
                                    }
                                }
                                Err(RecvTimeoutError::Timeout) => {
                                    if !idle {
                                        idle = true;
                                        idle_workers.fetch_add(1, Ordering::SeqCst);

                                        // Notify progress display that this worker is idle
                                        let _ = progress_tx.send(ProgressEvent::WorkerIdle { worker_id });
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
                        tracing::error!("JobSystem worker failed: [{}]", e);
                        job_sys.abort_flag.store(true, Ordering::SeqCst);
                    }
                });
            }
        });

        // Calculate execution time for reporting
        let execution_duration = execution_start.elapsed();
        let formatted_time = format_duration(execution_duration);
        let total_jobs = job_sys.job_results.len();

        // Check for any errors
        if job_sys.abort_flag.load(Ordering::SeqCst) {
            bail_loc!("JobSystem failed with errors after {}.", formatted_time);
        }

        // Sanity check: ensure all jobs actually completed
        if !job_sys.blocked_jobs.is_empty() {
            bail_loc!(
                "JobSystem finished after {formatted_time} but had [{}] jobs that weren't finished. [{:?}]",
                job_sys.blocked_jobs.len(),
                job_sys.blocked_jobs
            );
        }

        // Success!
        tracing::info!("Job system execution completed successfully in {formatted_time}");

        Ok(())
    }

    pub fn get_result(&self, job_id: JobId) -> ArcResult<dyn JobArtifact> {
        if let Some(kvp) = self.job_results.get(&job_id) {
            let arc_result = kvp.as_ref().map_err(|e| anyhow_loc!("{}", e))?.clone();
            Ok(arc_result)
        } else {
            let mut errors = Vec::new();
            for entry in self.job_results.iter() {
                if let Err(err) = entry.value() {
                    errors.push(format!("Job id {}: {err}", entry.key()));
                }
            }
            if errors.is_empty() {
                Err(anyhow_loc!(
                    "No job result found for job id {job_id} and no job errors recorded",
                ))
            } else {
                Err(anyhow_loc!("Aggregated job errors: {}", errors.join("; ")))
            }
        }
    }

    pub fn expect_result<T: JobArtifact>(&self, job_id: JobId) -> ArcResult<T> {
        let t_name = std::any::type_name::<T>();

        if let Some(kvp) = self.job_results.get(&job_id) {
            let arc_result = kvp.as_ref().map_err(|e| anyhow_loc!("{}", e))?.clone();
            arc_result.downcast_arc::<T>().map(|v| v.clone()).map_err(|v| {
                anyhow_loc!(
                    "Job result for job id {job_id} could not be cast to {t_name}. Actual type was {}",
                    std::any::type_name_of_val(v.as_any())
                )
            })
        } else {
            let mut errors = Vec::new();
            for entry in self.job_results.iter() {
                if let Err(err) = entry.value() {
                    errors.push(format!("Job id {}: {err}", entry.key()));
                }
            }
            if errors.is_empty() {
                Err(anyhow_loc!(
                    "No job result found for job id {job_id} and no job errors recorded",
                ))
            } else {
                Err(anyhow_loc!("Aggregated job errors: {}", errors.join("; ")))
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
                bail_loc!("Job [{}:{}] had no job fn", job.id, job.desc);
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

    pub(crate) fn any_errors(&self) -> bool {
        self.job_results.iter().any(|r| r.is_err())
    }
}
