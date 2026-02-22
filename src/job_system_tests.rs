//! Tests for job_system.rs

use crate::bail_loc;
use crate::function_name;
use crate::anubis::*;
use crate::fs_tree_hasher::*;
use crate::job_system::*;
use crate::progress::ProgressEvent;
use camino::{Utf8Path, Utf8PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Create a dummy progress sender for tests (events are silently discarded).
fn dummy_progress_tx() -> crossbeam::channel::Sender<ProgressEvent> {
    let (tx, _rx) = crossbeam::channel::unbounded();
    tx
}

/// Create a Job with a minimal display info derived from the description.
fn make_test_job(id: JobId, desc: String, ctx: Arc<JobContext>, job_fn: Box<JobFn>) -> Job {
    let display = JobDisplayInfo::from_desc(&desc);
    Job::new(id, desc, display, ctx, job_fn)
}

/// Create a Job via JobContext with minimal display info.
fn make_ctx_job(ctx: &Arc<JobContext>, desc: String, f: Box<JobFn>) -> Job {
    let display = JobDisplayInfo::from_desc(&desc);
    ctx.new_job(desc, display, f)
}

pub fn make_test_job_context(job_system: Arc<JobSystem>) -> JobContext {
    JobContext {
        anubis: make_test_anubis(),
        job_system,
        mode: None,
        toolchain: None,
    }
}

pub fn make_test_anubis() -> Arc<Anubis> {
    Arc::new(Anubis {
        root: Utf8PathBuf::new(),
        verbose_tools: false,
        rule_typeinfos: Default::default(),
        dir_exists_cache: Default::default(),
        raw_config_cache: Default::default(),
        resolved_config_cache: Default::default(),
        mode_cache: Default::default(),
        toolchain_cache: Default::default(),
        rule_cache: Default::default(),
        impure_transitive_deps_cache: Default::default(),
        job_cache: Default::default(),
        fs_tree_hasher: FsTreeHasher::new(HashMode::Fast)
            .expect("FsTreeHasher::new failed in test"),
    })
}

#[derive(Debug, Eq, PartialEq)]
pub struct TrivialResult(pub i64);
impl JobArtifact for TrivialResult {}

#[test]
fn trivial_job() -> anyhow::Result<()> {
    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let job = make_test_job(
        ctx.get_next_id(),
        "TrivialJob".to_owned(),
        ctx,
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(42))))),
    );
    jobsys.add_job(job)?;

    JobSystem::run_to_completion(jobsys.clone(), 1, dummy_progress_tx())?;

    let result = jobsys.expect_result::<TrivialResult>(0)?;
    assert_eq!(result.0, 42);

    Ok(())
}

#[test]
fn basic_dependency() -> anyhow::Result<()> {
    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    let mut flag = Arc::new(AtomicBool::new(false));

    // Create job A
    let a_flag = flag.clone();
    let mut a = make_test_job(
        ctx.get_next_id(),
        "job_a".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            a_flag.store(true, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(42))))
        }),
    );

    // Create job B
    let b_flag = flag.clone();
    let mut b = make_test_job(
        ctx.get_next_id(),
        "job_b".to_owned(),
        ctx,
        Box::new(move |_| {
            if b_flag.load(Ordering::SeqCst) {
                Ok(JobOutcome::Success(Arc::new(TrivialResult(1337))))
            } else {
                bail_loc!("Job B expected flag to be set by Job A")
            }
        }),
    );

    // Add jobs
    let a_id = a.id;
    jobsys.add_job(a)?;
    jobsys.add_job_with_deps(b, &vec![a_id])?;

    // Run jobs
    // Note we pass job_b before job_a
    JobSystem::run_to_completion(jobsys.clone(), 1, dummy_progress_tx())?;

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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let work_counter = Arc::new(AtomicUsize::new(0));

    // Create jobs with varying work loads
    for i in 0..12 {
        let counter = work_counter.clone();
        let work_ms = (i % 3) * 10; // 0ms, 10ms, 20ms work simulation

        let job = make_test_job(
            ctx.get_next_id(),
            format!("work_job_{}", i),
            ctx.clone(),
            Box::new(move |_| {
                // Simulate work
                if work_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(work_ms));
                }
                let work_done = counter.fetch_add(1, Ordering::SeqCst);
                Ok(JobOutcome::Success(Arc::new(TrivialResult(work_done as i64))))
            }),
        );
        jobsys.add_job(job)?;
    }

    let start_time = std::time::Instant::now();
    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?; // Use 4 workers
    let elapsed = start_time.elapsed();

    // With 4 workers, should complete faster than single worker
    assert!(
        elapsed < std::time::Duration::from_millis(300),
        "Multi-worker system should complete work efficiently"
    );

    // Verify all work was completed
    assert_eq!(work_counter.load(Ordering::SeqCst), 12);

    // Verify all jobs have unique work completion order
    let mut work_orders: Vec<i64> =
        (0..12).map(|i| jobsys.expect_result::<TrivialResult>(i).unwrap().0).collect();
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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let execution_order = Arc::new(AtomicU8::new(0));

    // Job A (executes last)
    let order_a = execution_order.clone();
    let job_a = make_test_job(
        ctx.get_next_id(),
        "job_a".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            let order = order_a.fetch_add(1, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(order as i64))))
        }),
    );

    // Job B (depends on A)
    let order_b = execution_order.clone();
    let job_b = make_test_job(
        ctx.get_next_id(),
        "job_b".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            let order = order_b.fetch_add(1, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(order as i64))))
        }),
    );

    // Job C (depends on B)
    let order_c = execution_order.clone();
    let job_c = make_test_job(
        ctx.get_next_id(),
        "job_c".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            let order = order_c.fetch_add(1, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(order as i64))))
        }),
    );

    // Job D (depends on C)
    let order_d = execution_order.clone();
    let job_d = make_test_job(
        ctx.get_next_id(),
        "job_d".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            let order = order_d.fetch_add(1, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(order as i64))))
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

    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?;

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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let execution_flags = Arc::new((
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    ));

    // Job A (foundation)
    let flags_a = execution_flags.clone();
    let job_a = make_test_job(
        ctx.get_next_id(),
        "job_a".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            flags_a.0.store(true, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(1))))
        }),
    );

    // Job B (depends on A)
    let flags_b = execution_flags.clone();
    let job_b = make_test_job(
        ctx.get_next_id(),
        "job_b".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            // Verify A has executed
            assert!(
                flags_b.0.load(Ordering::SeqCst),
                "Job A should have executed before B"
            );
            flags_b.1.store(true, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(2))))
        }),
    );

    // Job C (depends on A)
    let flags_c = execution_flags.clone();
    let job_c = make_test_job(
        ctx.get_next_id(),
        "job_c".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            // Verify A has executed
            assert!(
                flags_c.0.load(Ordering::SeqCst),
                "Job A should have executed before C"
            );
            flags_c.2.store(true, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(3))))
        }),
    );

    // Job D (depends on both B and C)
    let flags_d = execution_flags.clone();
    let job_d = make_test_job(
        ctx.get_next_id(),
        "job_d".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            // Verify both B and C have executed
            assert!(
                flags_d.1.load(Ordering::SeqCst),
                "Job B should have executed before D"
            );
            assert!(
                flags_d.2.load(Ordering::SeqCst),
                "Job C should have executed before D"
            );
            Ok(JobOutcome::Success(Arc::new(TrivialResult(4))))
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

    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?;

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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let should_not_execute = Arc::new(AtomicBool::new(false));

    // Job A (will fail)
    let job_a = make_test_job(
        ctx.get_next_id(),
        "failing_job".to_owned(),
        ctx.clone(),
        Box::new(|_| bail_loc!("Intentional test failure")),
    );

    // Job B (should not execute due to A's failure)
    let flag = should_not_execute.clone();
    let job_b = make_test_job(
        ctx.get_next_id(),
        "dependent_job".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            flag.store(true, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(42))))
        }),
    );

    let a_id = job_a.id;
    let b_id = job_b.id;

    jobsys.add_job(job_a)?;
    jobsys.add_job_with_deps(job_b, &[a_id])?;

    // This should fail due to job A's error
    let result = JobSystem::run_to_completion(jobsys.clone(), 2, dummy_progress_tx());
    assert!(
        result.is_err(),
        "JobSystem should have failed due to job A's error"
    );

    // Verify job B never executed
    assert!(
        !should_not_execute.load(Ordering::SeqCst),
        "Dependent job should not have executed"
    );

    // Verify abort flag is set
    assert!(
        jobsys.abort_flag.load(Ordering::SeqCst),
        "Abort flag should be set"
    );

    // Verify we have error results
    assert!(jobsys.get_result(a_id).is_err(), "Job A should have error result");
    assert!(jobsys.get_result(b_id).is_err(), "Job B should have no result");

    Ok(())
}

// Test concurrent job execution with no dependencies
#[test]
fn concurrent_independent_jobs() -> anyhow::Result<()> {
    // Test verifies that independent jobs can execute concurrently
    // Uses timing to ensure jobs run in parallel, not sequentially

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let start_time = std::time::Instant::now();
    let barrier = Arc::new(std::sync::Barrier::new(3)); // 3 jobs will wait for each other

    // Create 3 independent jobs that synchronize at a barrier
    // If they run concurrently, they'll all reach the barrier quickly
    // If they run sequentially, the test will timeout
    for i in 0..3 {
        let barrier_clone = barrier.clone();
        let job = make_test_job(
            ctx.get_next_id(),
            format!("concurrent_job_{}", i),
            ctx.clone(),
            Box::new(move |_| {
                // All jobs wait at barrier - proves they're running concurrently
                barrier_clone.wait();
                Ok(JobOutcome::Success(Arc::new(TrivialResult(i as i64))))
            }),
        );
        jobsys.add_job(job)?;
    }

    JobSystem::run_to_completion(jobsys.clone(), 3, dummy_progress_tx())?;

    // Verify all completed within reasonable time (concurrent execution)
    let elapsed = start_time.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(1000),
        "Jobs should complete quickly if running concurrently"
    );

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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Job A (will complete first)
    let job_a = make_test_job(
        ctx.get_next_id(),
        "completed_job".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(42))))),
    );

    let a_id = job_a.id;
    jobsys.add_job(job_a)?;

    // Run just job A to completion
    JobSystem::run_to_completion(jobsys.clone(), 1, dummy_progress_tx())?;

    // Verify A completed
    assert_eq!(jobsys.expect_result::<TrivialResult>(a_id)?.0, 42);

    // Now add job B that depends on the already-completed job A
    let job_b = make_test_job(
        ctx.get_next_id(),
        "dependent_on_completed".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(1337))))),
    );

    let b_id = job_b.id;
    jobsys.add_job_with_deps(job_b, &[a_id])?;

    // Run again - job B should execute immediately since A is done
    JobSystem::run_to_completion(jobsys.clone(), 1, dummy_progress_tx())?;

    // Verify B completed
    assert_eq!(jobsys.expect_result::<TrivialResult>(b_id)?.0, 1337);

    Ok(())
}

// Test adding dependency on failed job should fail
#[test]
fn dependency_on_failed_job() -> anyhow::Result<()> {
    // Test verifies that trying to add a dependency on a job that already failed
    // results in an error when adding the dependent job

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Job A (will fail)
    let job_a = make_test_job(
        ctx.get_next_id(),
        "failing_job".to_owned(),
        ctx.clone(),
        Box::new(|_| bail_loc!("Intentional failure")),
    );

    let a_id = job_a.id;
    jobsys.add_job(job_a)?;

    // Run job A to failure
    let result = JobSystem::run_to_completion(jobsys.clone(), 1, dummy_progress_tx());
    assert!(result.is_err(), "Job A should have failed");

    // Now try to add job B that depends on the failed job A
    let job_b = make_test_job(
        ctx.get_next_id(),
        "dependent_on_failed".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(42))))),
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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Independent jobs (will run in parallel)
    let indep_1 = make_test_job(
        ctx.get_next_id(),
        "independent_1".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(100))))),
    );

    let indep_2 = make_test_job(
        ctx.get_next_id(),
        "independent_2".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(200))))),
    );

    // Chain: base -> middle -> final
    let base_job = make_test_job(
        ctx.get_next_id(),
        "chain_base".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(5))))),
    );

    let middle_job = make_test_job(
        ctx.get_next_id(),
        "chain_middle".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(10))))),
    );

    let final_job = make_test_job(
        ctx.get_next_id(),
        "chain_final".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(15))))),
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

    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?;

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

    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?;

    let elapsed = start_time.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(1000),
        "Empty job system should complete very quickly"
    );

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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Create a chain of 10 dependent jobs
    let mut prev_job_id = None;
    for i in 0..10 {
        let job = make_test_job(
            ctx.get_next_id(),
            format!("chain_job_{}", i),
            ctx.clone(),
            Box::new(move |_| Ok(JobOutcome::Success(Arc::new(TrivialResult(i))))),
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
    JobSystem::run_to_completion(jobsys.clone(), 1, dummy_progress_tx())?;

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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Job that takes a short time to complete
    let job = make_test_job(
        ctx.get_next_id(),
        "slow_job".to_owned(),
        ctx.clone(),
        Box::new(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(JobOutcome::Success(Arc::new(TrivialResult(42))))
        }),
    );

    jobsys.add_job(job)?;

    let start_time = std::time::Instant::now();
    JobSystem::run_to_completion(jobsys.clone(), 3, dummy_progress_tx())?;
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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    let num_jobs = 100;

    // Create many independent jobs
    for i in 0..num_jobs {
        let job = make_test_job(
            ctx.get_next_id(),
            format!("bulk_job_{}", i),
            ctx.clone(),
            Box::new(move |_| Ok(JobOutcome::Success(Arc::new(TrivialResult(i))))),
        );
        jobsys.add_job(job)?;
    }

    let start_time = std::time::Instant::now();
    JobSystem::run_to_completion(jobsys.clone(), 8, dummy_progress_tx())?; // Use 8 workers
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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Test 1: Dependency on non-existent job (this should block forever and be detected)
    let job_missing_dep = make_test_job(
        ctx.get_next_id(),
        "depends_on_missing".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(1))))),
    );

    // Add job that depends on job ID 999 which doesn't exist
    jobsys.add_job_with_deps(job_missing_dep, &[999])?;

    // This should fail because job 999 doesn't exist and will never complete
    let result = JobSystem::run_to_completion(jobsys.clone(), 1, dummy_progress_tx());
    assert!(result.is_err(), "Should fail due to missing dependency");

    // Test 2: Self-dependency (job depends on itself)
    let jobsys2: Arc<JobSystem> = JobSystem::new().into();
    let self_dep_job = make_test_job(
        ctx.get_next_id(),
        "self_dependent".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(2))))),
    );

    let self_id = self_dep_job.id;
    jobsys2.add_job_with_deps(self_dep_job, &[self_id])?;

    // This should also fail due to self-dependency deadlock
    let result2 = JobSystem::run_to_completion(jobsys2, 1, dummy_progress_tx());
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
    impl JobArtifact for CustomResult {}

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Job that returns custom result type
    let job = make_test_job(
        ctx.get_next_id(),
        "custom_result_job".to_owned(),
        ctx.clone(),
        Box::new(|_| {
            Ok(JobOutcome::Success(Arc::new(CustomResult {
                value: "test".to_string(),
                number: 42,
            })))
        }),
    );

    let job_id = job.id;
    jobsys.add_job(job)?;

    JobSystem::run_to_completion(jobsys.clone(), 1, dummy_progress_tx())?;

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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let shared_counter = Arc::new(AtomicI64::new(0));

    // Multiple jobs that access shared state through context
    for i in 0..5 {
        let counter = shared_counter.clone();
        let job = make_test_job(
            ctx.get_next_id(),
            format!("shared_context_job_{}", i),
            ctx.clone(),
            Box::new(move |_| {
                let value = counter.fetch_add(1, Ordering::SeqCst);
                Ok(JobOutcome::Success(Arc::new(TrivialResult(value))))
            }),
        );
        jobsys.add_job(job)?;
    }

    JobSystem::run_to_completion(jobsys.clone(), 3, dummy_progress_tx())?;

    // Verify all jobs ran and accessed shared state
    let final_count = shared_counter.load(Ordering::SeqCst);
    assert_eq!(final_count, 5);

    // Each job should have gotten a unique value from the counter
    let mut values: Vec<i64> = (0..5).map(|i| jobsys.expect_result::<TrivialResult>(i).unwrap().0).collect();
    values.sort();
    assert_eq!(values, vec![0, 1, 2, 3, 4]);

    Ok(())
}

// Test aborting during job execution
#[test]
fn abort_during_execution() -> anyhow::Result<()> {
    // Test verifies that abort flag properly stops job execution

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let execution_count = Arc::new(AtomicUsize::new(0));

    // Create jobs that check abort flag
    for i in 0..10 {
        let count = execution_count.clone();
        let job = make_test_job(
            ctx.get_next_id(),
            format!("abortable_job_{}", i),
            ctx.clone(),
            Box::new(move |job| {
                count.fetch_add(1, Ordering::SeqCst);
                if job.id == 2 {
                    // Job 2 will fail and trigger abort
                    bail_loc!("Triggering abort")
                } else {
                    // Other jobs might not get to run due to abort
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    Ok(JobOutcome::Success(Arc::new(TrivialResult(job.id))))
                }
            }),
        );
        jobsys.add_job(job)?;
    }

    let result = JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx());
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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Spawn multiple threads that add jobs rapidly
    let handles: Vec<_> = (0..4)
        .map(|thread_id| {
            let ctx = ctx.clone();
            let jobsys = jobsys.clone();

            std::thread::spawn(move || -> anyhow::Result<()> {
                for i in 0..25 {
                    let job = make_test_job(
                        ctx.get_next_id(),
                        format!("rapid_job_t{}_i{}", thread_id, i),
                        ctx.clone(),
                        Box::new(move |_| {
                            // Small amount of work to prevent immediate completion
                            std::thread::sleep(std::time::Duration::from_micros(100));
                            Ok(JobOutcome::Success(Arc::new(TrivialResult(
                                (thread_id * 100 + i) as i64,
                            ))))
                        }),
                    );
                    jobsys.add_job(job)?;
                }
                Ok(())
            })
        })
        .collect();

    // Wait for all threads to add their jobs
    for handle in handles {
        handle.join().unwrap()?;
    }

    let start_time = std::time::Instant::now();
    JobSystem::run_to_completion(jobsys.clone(), 8, dummy_progress_tx())?;
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
    impl JobArtifact for LargeResult {}

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Create jobs that produce large results
    for i in 0..10 {
        let job = make_test_job(
            ctx.get_next_id(),
            format!("large_result_job_{}", i),
            ctx.clone(),
            Box::new(move |_| {
                // Create a 1MB result
                let large_data = vec![i as u8; 1024 * 1024];
                Ok(JobOutcome::Success(Arc::new(LargeResult {
                    data: large_data,
                    id: i as usize,
                })))
            }),
        );
        jobsys.add_job(job)?;
    }

    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?;

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

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    let mut job = make_test_job(
        ctx.get_next_id(),
        "job_without_fn".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(42))))),
    );

    // Remove the job function to simulate the error case
    job.job_fn = None;

    jobsys.add_job(job)?;

    // This should fail during execution when the job has no function
    let result = JobSystem::run_to_completion(jobsys, 1, dummy_progress_tx());
    assert!(result.is_err(), "Should fail due to missing job function");

    Ok(())
}

// Test graceful shutdown behavior
#[test]
fn graceful_shutdown() -> anyhow::Result<()> {
    // Test verifies that the system shuts down gracefully when all work is done
    // This tests the idle detection and completion logic

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Add a few simple jobs
    for i in 0..5 {
        let job = make_test_job(
            ctx.get_next_id(),
            format!("shutdown_test_job_{}", i),
            ctx.clone(),
            Box::new(move |_| {
                std::thread::sleep(std::time::Duration::from_millis(10));
                Ok(JobOutcome::Success(Arc::new(TrivialResult(i as i64))))
            }),
        );
        jobsys.add_job(job)?;
    }

    let start_time = std::time::Instant::now();
    JobSystem::run_to_completion(jobsys.clone(), 3, dummy_progress_tx())?;
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

// Test jobs that create and add new jobs to the system
#[test]
fn jobs_creating_jobs() -> anyhow::Result<()> {
    // Test verifies that a job can create and add new jobs to the system
    // This mirrors the pattern in cc_rules.rs where build_cpp_binary creates compile jobs

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let completion_order = Arc::new(Mutex::new(Vec::new()));

    // Parent job that creates child jobs
    let order_parent = completion_order.clone();
    let parent_job = make_test_job(
        ctx.get_next_id(),
        "parent_job_creates_children".to_owned(),
        ctx.clone(),
        Box::new(move |job| {
            // Create 3 child jobs
            for i in 0..3 {
                let order_child = order_parent.clone();
                let child_job = make_test_job(
                    job.ctx.get_next_id(),
                    format!("child_job_{}", i),
                    job.ctx.clone(),
                    Box::new(move |_| {
                        // Record completion order
                        order_child.lock().unwrap().push(format!("child_{}", i));
                        Ok(JobOutcome::Success(Arc::new(TrivialResult(i as i64))))
                    }),
                );

                // Add child job to the system
                if let Err(e) = job.ctx.job_system.add_job(child_job) {
                    bail_loc!("Failed to add child job: {}", e);
                }
            }

            // Parent completes after creating children
            order_parent.lock().unwrap().push("parent".to_string());
            Ok(JobOutcome::Success(Arc::new(TrivialResult(999))))
        }),
    );

    let parent_id = parent_job.id;
    jobsys.add_job(parent_job)?;

    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?;

    // Verify parent job completed
    assert_eq!(jobsys.expect_result::<TrivialResult>(parent_id)?.0, 999);

    // Verify all child jobs completed - they should all be done by now
    // since run_to_completion should wait for all jobs to finish
    let mut found_children = 0;
    for entry in jobsys.job_results.iter() {
        let job_id = *entry.key();
        let result = entry.value();
        if job_id != parent_id {
            let result = result.as_ref().unwrap();
            let trivial_result = result.downcast_ref::<TrivialResult>().unwrap();
            assert!(trivial_result.0 < 3, "Child job should have value 0-2");
            found_children += 1;
        }
    }
    assert_eq!(found_children, 3, "Should have found 3 child jobs");

    // Verify completion order - parent should complete first (since it creates children)
    let order = completion_order.lock().unwrap();
    assert_eq!(order[0], "parent", "Parent should complete first");
    assert!(order.len() == 4, "Should have 4 completion events");

    Ok(())
}

// Test jobs creating jobs with dependencies
#[test]
fn jobs_creating_dependent_jobs() -> anyhow::Result<()> {
    // Test verifies that a job can create jobs with dependencies between them
    // This tests more complex job creation patterns

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let execution_order = Arc::new(AtomicUsize::new(0));

    // Parent job that creates a chain of dependent jobs
    let order_parent = execution_order.clone();
    let parent_job = make_test_job(
        ctx.get_next_id(),
        "parent_creates_chain".to_owned(),
        ctx.clone(),
        Box::new(move |job| {
            let mut job_ids = Vec::new();

            // Create chain: job_0 -> job_1 -> job_2
            for i in 0..3 {
                let order_child = order_parent.clone();
                let child_job = make_test_job(
                    job.ctx.get_next_id(),
                    format!("chain_job_{}", i),
                    job.ctx.clone(),
                    Box::new(move |_| {
                        let order = order_child.fetch_add(1, Ordering::SeqCst);
                        Ok(JobOutcome::Success(Arc::new(TrivialResult(order as i64))))
                    }),
                );

                let child_id = child_job.id;
                job_ids.push(child_id);

                // Add with dependency on previous job (except first)
                if i == 0 {
                    if let Err(e) = job.ctx.job_system.add_job(child_job) {
                        bail_loc!("Failed to add job: {}", e)
                    }
                } else {
                    let prev_id = job_ids[i - 1];
                    if let Err(e) = job.ctx.job_system.add_job_with_deps(child_job, &[prev_id]) {
                        bail_loc!("Failed to add dependent job: {}", e)
                    }
                }
            }

            // Create continuation job
            let continuation_job = make_ctx_job(&job.ctx,
                format!("{} (continuation)", job.desc),
                Box::new(move |_job: Job| -> anyhow::Result<JobOutcome> {
                    Ok(JobOutcome::Success(Arc::new(TrivialResult(777))))
                }),
            );

            // Defer until all child jobs complete
            Ok(JobOutcome::Deferred(JobDeferral {
                blocked_by: job_ids,
                continuation_job,
            }))
        }),
    );

    let parent_id = parent_job.id;
    jobsys.add_job(parent_job)?;

    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?;

    // Verify parent job completed
    assert_eq!(jobsys.expect_result::<TrivialResult>(parent_id)?.0, 777);

    // Verify child jobs executed in order
    // Filter out parent job and continuation job (which has value 777)
    let mut child_results: Vec<(JobId, i64)> = jobsys
        .job_results
        .iter()
        .filter(|entry| *entry.key() != parent_id)
        .map(|entry| {
            let job_id = *entry.key();
            let result = entry.value().as_ref().unwrap();
            let trivial_result = result.downcast_ref::<TrivialResult>().unwrap();
            (job_id, trivial_result.0)
        })
        .filter(|(_, value)| *value != 777) // Filter out the continuation job
        .collect();

    child_results.sort_by_key(|(_, order)| *order);
    assert_eq!(child_results.len(), 3);
    assert_eq!(child_results[0].1, 0); // First job executed
    assert_eq!(child_results[1].1, 1); // Second job executed
    assert_eq!(child_results[2].1, 2); // Third job executed

    Ok(())
}

// Test job deferral mechanism
#[test]
fn job_deferral_basic() -> anyhow::Result<()> {
    // Test verifies basic job deferral functionality
    // A job defers itself until a dependency completes

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let execution_order = Arc::new(AtomicUsize::new(0));

    // Dependency job
    let order_dep = execution_order.clone();
    let dep_job = make_test_job(
        ctx.get_next_id(),
        "dependency_job".to_owned(),
        ctx.clone(),
        Box::new(move |_| {
            let order = order_dep.fetch_add(1, Ordering::SeqCst);
            Ok(JobOutcome::Success(Arc::new(TrivialResult(order as i64))))
        }),
    );

    let dep_id = dep_job.id;
    jobsys.add_job(dep_job)?;

    // Job that defers itself
    let order_main = execution_order.clone();
    let main_job = make_test_job(
        ctx.get_next_id(),
        "deferring_job".to_owned(),
        ctx.clone(),
        Box::new(move |job| {
            // Create continuation job
            let order_deferred = order_main.clone();
            let continuation_job = make_ctx_job(&job.ctx,
                format!("{} (continuation)", job.desc),
                Box::new(move |_job: Job| -> anyhow::Result<JobOutcome> {
                    let order = order_deferred.fetch_add(1, Ordering::SeqCst);
                    Ok(JobOutcome::Success(Arc::new(TrivialResult(order as i64))))
                }),
            );

            // Defer the job until dep_job completes
            Ok(JobOutcome::Deferred(JobDeferral {
                blocked_by: vec![dep_id],
                continuation_job,
            }))
        }),
    );

    let main_id = main_job.id;
    jobsys.add_job(main_job)?;

    JobSystem::run_to_completion(jobsys.clone(), 2, dummy_progress_tx())?;

    // Verify dependency job executed first
    assert_eq!(jobsys.expect_result::<TrivialResult>(dep_id)?.0, 0);

    // Verify main job executed second with deferred function
    // The main job should have executed after the dependency with value 1
    assert_eq!(
        jobsys.expect_result::<TrivialResult>(main_id)?.0,
        1,
        "Main job should execute after dependency"
    );

    Ok(())
}

// Test job deferral with multiple dependencies
#[test]
fn job_deferral_multiple_dependencies() -> anyhow::Result<()> {
    // Test verifies job deferral with multiple dependencies
    // This mirrors the cc_rules.rs pattern where linking waits for all compilation jobs

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));
    let completion_flags = Arc::new((
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    ));

    // Create 3 dependency jobs
    let mut dep_ids = Vec::new();
    for i in 0..3 {
        let flags = completion_flags.clone();
        let dep_job = make_test_job(
            ctx.get_next_id(),
            format!("dependency_{}", i),
            ctx.clone(),
            Box::new(move |_| {
                // Simulate different completion times
                std::thread::sleep(std::time::Duration::from_millis(i * 10));

                match i {
                    0 => flags.0.store(true, Ordering::SeqCst),
                    1 => flags.1.store(true, Ordering::SeqCst),
                    2 => flags.2.store(true, Ordering::SeqCst),
                    _ => {}
                }

                Ok(JobOutcome::Success(Arc::new(TrivialResult(i as i64))))
            }),
        );

        dep_ids.push(dep_job.id);
        jobsys.add_job(dep_job)?;
    }

    // Job that defers until all dependencies complete
    let flags_main = completion_flags.clone();
    let dep_ids_clone = dep_ids.clone();
    let main_job = make_test_job(
        ctx.get_next_id(),
        "main_job_defers".to_owned(),
        ctx.clone(),
        Box::new(move |job| {
            // Create continuation job
            let flags_deferred = flags_main.clone();
            let continuation_job = make_ctx_job(&job.ctx,
                format!("{} (continuation)", job.desc),
                Box::new(move |_job: Job| -> anyhow::Result<JobOutcome> {
                    // Verify all dependencies completed
                    if flags_deferred.0.load(Ordering::SeqCst)
                        && flags_deferred.1.load(Ordering::SeqCst)
                        && flags_deferred.2.load(Ordering::SeqCst)
                    {
                        Ok(JobOutcome::Success(Arc::new(TrivialResult(999))))
                    } else {
                        bail_loc!("Not all dependencies completed")
                    }
                }),
            );

            // Defer until all dependencies complete
            Ok(JobOutcome::Deferred(JobDeferral {
                blocked_by: dep_ids_clone.clone(),
                continuation_job,
            }))
        }),
    );

    let main_id = main_job.id;
    jobsys.add_job(main_job)?;

    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?;

    // Verify all dependency jobs completed
    for (i, dep_id) in dep_ids.iter().enumerate() {
        assert_eq!(jobsys.expect_result::<TrivialResult>(*dep_id)?.0, i as i64);
    }

    // Verify main job completed successfully with deferred function
    assert_eq!(
        jobsys.expect_result::<TrivialResult>(main_id)?.0,
        999,
        "Main job should complete successfully after dependencies"
    );

    Ok(())
}

// Test job deferral with job modification (like cc_rules.rs)
#[test]
fn job_deferral_with_modification() -> anyhow::Result<()> {
    // Test verifies job deferral where the job modifies itself before deferring
    // This mirrors cc_rules.rs where the job changes its function to be the link job

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Create a preparation job
    let prep_job = make_test_job(
        ctx.get_next_id(),
        "preparation_job".to_owned(),
        ctx.clone(),
        Box::new(|_| Ok(JobOutcome::Success(Arc::new(TrivialResult(42))))),
    );

    let prep_id = prep_job.id;
    jobsys.add_job(prep_job)?;

    // Main job that creates a continuation job and then defers
    let main_job = make_test_job(
        ctx.get_next_id(),
        "self_modifying_job".to_owned(),
        ctx.clone(),
        Box::new(move |job| {
            // Create continuation job (like cc_rules.rs link job)
            let continuation_job = make_ctx_job(&job.ctx,
                format!("{} (continuation)", job.desc),
                Box::new(move |cont_job: Job| -> anyhow::Result<JobOutcome> {
                    // This is the continuation function that runs after deferral
                    // Verify the preparation job completed using the job's context
                    match cont_job.ctx.job_system.expect_result::<TrivialResult>(prep_id) {
                        Ok(result) => {
                            if result.0 == 42 {
                                Ok(JobOutcome::Success(Arc::new(TrivialResult(1337))))
                            } else {
                                bail_loc!("Unexpected prep result: {}", result.0)
                            }
                        }
                        Err(e) => bail_loc!("Failed to get prep result: {}", e),
                    }
                }),
            );

            // Defer until preparation completes
            Ok(JobOutcome::Deferred(JobDeferral {
                blocked_by: vec![prep_id],
                continuation_job,
            }))
        }),
    );

    let main_id = main_job.id;
    jobsys.add_job(main_job)?;

    JobSystem::run_to_completion(jobsys.clone(), 2, dummy_progress_tx())?;

    // Verify preparation job completed
    assert_eq!(jobsys.expect_result::<TrivialResult>(prep_id)?.0, 42);

    // Verify main job completed with modified function result
    assert_eq!(jobsys.expect_result::<TrivialResult>(main_id)?.0, 1337);

    Ok(())
}

// Test complex job creation and deferral pattern (like cc_rules.rs)
#[test]
fn complex_job_creation_and_deferral() -> anyhow::Result<()> {
    // Test verifies the full pattern from cc_rules.rs:
    // 1. Main job creates multiple child jobs
    // 2. Main job modifies itself to be a "link" job
    // 3. Main job defers until all child jobs complete
    // 4. Modified job executes with results from child jobs

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Main job that creates children and defers (mirrors build_cpp_binary)
    let main_job = make_test_job(
        ctx.get_next_id(),
        "main_build_job".to_owned(),
        ctx.clone(),
        Box::new(move |job| {
            let mut child_job_ids = Vec::new();

            // Create multiple "compilation" jobs
            for i in 0..3 {
                let compile_job = make_test_job(
                    job.ctx.get_next_id(),
                    format!("compile_job_{}", i),
                    job.ctx.clone(),
                    Box::new(move |_| {
                        // Simulate compilation by producing a file result
                        Ok(JobOutcome::Success(Arc::new(TrivialResult(i * 10))))
                    }),
                );

                child_job_ids.push(compile_job.id);

                // Add compilation job to system
                if let Err(e) = job.ctx.job_system.add_job(compile_job) {
                    bail_loc!("Failed to add compile job: {}", e)
                }
            }

            // Create continuation "link" job that uses results from compilation jobs
            let link_job_ids = child_job_ids.clone();
            let continuation_job = make_ctx_job(&job.ctx,
                format!("{} (link)", job.desc),
                Box::new(move |link_job: Job| -> anyhow::Result<JobOutcome> {
                    // Collect results from all compilation jobs
                    let mut total = 0;
                    for compile_job_id in &link_job_ids {
                        match link_job.ctx.job_system.get_result(*compile_job_id) {
                            Ok(result) => {
                                if let Some(trivial_result) = result.downcast_ref::<TrivialResult>() {
                                    total += trivial_result.0;
                                } else {
                                    bail_loc!("Failed to downcast compile result")
                                }
                            }
                            Err(e) => {
                                bail_loc!("Compile job failed: {}", e)
                            }
                        }
                    }

                    // "Link" the results together
                    Ok(JobOutcome::Success(Arc::new(TrivialResult(total))))
                }),
            );

            // Defer until all compilation jobs complete
            Ok(JobOutcome::Deferred(JobDeferral {
                blocked_by: child_job_ids,
                continuation_job,
            }))
        }),
    );

    let main_id = main_job.id;
    jobsys.add_job(main_job)?;

    JobSystem::run_to_completion(jobsys.clone(), 4, dummy_progress_tx())?;

    // Verify main job completed with linked result
    // Should be 0*10 + 1*10 + 2*10 = 0 + 10 + 20 = 30
    assert_eq!(jobsys.expect_result::<TrivialResult>(main_id)?.0, 30);

    // Verify all compilation jobs completed
    let mut found_compile_jobs = 0;
    for entry in jobsys.job_results.iter() {
        let job_id = *entry.key();
        let result = entry.value();
        if job_id != main_id {
            let result = result.as_ref().unwrap();
            let trivial_result = result.downcast_ref::<TrivialResult>().unwrap();

            // Compile job results should be 0, 10, or 20
            if trivial_result.0 == 0 || trivial_result.0 == 10 || trivial_result.0 == 20 {
                found_compile_jobs += 1;
            }
        }
    }
    assert_eq!(found_compile_jobs, 3, "Should have found 3 compile jobs");

    Ok(())
}

// Test job deferral error handling
#[test]
fn job_deferral_error_handling() -> anyhow::Result<()> {
    // Test verifies error handling in job deferral scenarios

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Dependency job that will fail
    let failing_dep = make_test_job(
        ctx.get_next_id(),
        "failing_dependency".to_owned(),
        ctx.clone(),
        Box::new(|_| bail_loc!("Dependency failed")),
    );

    let failing_id = failing_dep.id;
    jobsys.add_job(failing_dep)?;

    // Job that defers on the failing dependency
    let main_job = make_test_job(
        ctx.get_next_id(),
        "job_defers_on_failure".to_owned(),
        ctx.clone(),
        Box::new(move |job| {
            // Create continuation job
            let continuation_job = make_ctx_job(&job.ctx,
                format!("{} (continuation)", job.desc),
                Box::new(move |_job: Job| -> anyhow::Result<JobOutcome> {
                    Ok(JobOutcome::Success(Arc::new(TrivialResult(999))))
                }),
            );

            // Defer on the failing job
            Ok(JobOutcome::Deferred(JobDeferral {
                blocked_by: vec![failing_id],
                continuation_job,
            }))
        }),
    );

    let main_id = main_job.id;
    jobsys.add_job(main_job)?;

    // This should fail due to the failing dependency
    let result = JobSystem::run_to_completion(jobsys.clone(), 2, dummy_progress_tx());
    assert!(result.is_err(), "Should fail due to failing dependency");

    // Verify the failing job has an error result
    assert!(jobsys.get_result(failing_id).is_err());

    // Verify main job never executed with deferred function
    // Since the failing dependency prevents the deferred job from running,
    // the main job should not have a result
    assert!(
        jobsys.get_result(main_id).is_err(),
        "Main job should not have executed its deferred function"
    );

    Ok(())
}

// Test multi-level continuation jobs (continuation that defers again)
#[test]
fn job_deferral_multi_level() -> anyhow::Result<()> {
    // Test verifies that continuation jobs can themselves defer
    // Chain: Job A defers to B, B defers to C, C completes
    // Result should propagate back through: C -> B -> A

    let jobsys: Arc<JobSystem> = JobSystem::new().into();
    let ctx: Arc<JobContext> = Arc::new(make_test_job_context(jobsys.clone()));

    // Job A: defers to continuation B
    let main_job = make_test_job(
        ctx.get_next_id(),
        "job_a".to_owned(),
        ctx.clone(),
        Box::new(move |job| {
            // Create continuation B which will itself defer
            let continuation_b = make_ctx_job(&job.ctx,
                "job_b (continuation of A)".to_owned(),
                Box::new(move |job_b| {
                    // B defers to continuation C
                    let continuation_c = make_ctx_job(&job_b.ctx,
                        "job_c (continuation of B)".to_owned(),
                        Box::new(move |_job_c| {
                            // C completes with final result
                            Ok(JobOutcome::Success(Arc::new(TrivialResult(42))))
                        }),
                    );

                    Ok(JobOutcome::Deferred(JobDeferral {
                        blocked_by: vec![],
                        continuation_job: continuation_c,
                    }))
                }),
            );

            Ok(JobOutcome::Deferred(JobDeferral {
                blocked_by: vec![],
                continuation_job: continuation_b,
            }))
        }),
    );

    let main_id = main_job.id;
    jobsys.add_job(main_job)?;

    JobSystem::run_to_completion(jobsys.clone(), 2, dummy_progress_tx())?;

    // The original job A should have the result from C propagated back
    let result = jobsys.expect_result::<TrivialResult>(main_id)?;
    assert_eq!(
        result.0, 42,
        "Result should propagate through all continuation levels"
    );

    Ok(())
}
