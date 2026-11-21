//! Scheduling around `Rule::blocks_dependents` deps (#213): a rule's build
//! job waits for the blocking deps below it, its other deps don't, and the
//! blocking chain runs urgent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use serde::Deserialize;

use crate::anubis::*;
use crate::job_system::*;
use crate::progress::{DisplayMode, ProgressDisplay};

/// What the test rules saw, in the order they saw it.
#[derive(Default)]
struct Log {
    events: Mutex<Vec<String>>,
    sibling_started: AtomicBool,
    gate_done: AtomicBool,
}

static LOG: std::sync::LazyLock<Log> = std::sync::LazyLock::new(Log::default);

fn log(event: String) {
    LOG.events.lock().unwrap().push(event);
}

/// A node in the test graph. `blocks` makes it a blocking dep. The node
/// named `gate` waits (up to a timeout) for `sibling` to start, which only
/// happens if `sibling` isn't held back behind `gate`.
#[derive(Debug, Deserialize)]
struct Node {
    name: String,
    #[serde(default)]
    deps: Vec<AnubisTarget>,
    #[serde(default)]
    blocks: bool,
    #[serde(skip_deserializing)]
    target: AnubisTarget,
}

#[derive(Debug)]
struct Done;
impl JobArtifact for Done {}

fn display(target: &AnubisTarget) -> JobDisplayInfo {
    JobDisplayInfo { verb: "Test", short_name: target.target_name().to_owned(), detail: target.target_path().to_owned() }
}

fn run_node(name: &str, job: &Job) -> anyhow::Result<()> {
    log(format!("start {name} urgent={}", job.ctx.urgent));
    match name {
        "sibling" => LOG.sibling_started.store(true, Ordering::SeqCst),
        "gate" => {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !LOG.sibling_started.load(Ordering::SeqCst) {
                anyhow::ensure!(Instant::now() < deadline, "sibling never started while gate was running");
                std::thread::sleep(Duration::from_millis(5));
            }
            LOG.gate_done.store(true, Ordering::SeqCst);
        }
        "root" | "lib" => anyhow::ensure!(LOG.gate_done.load(Ordering::SeqCst), "{name} started before gate finished"),
        _ => {}
    }
    Ok(())
}

impl Rule for Node {
    fn name(&self) -> String { self.name.clone() }
    fn target(&self) -> AnubisTarget { self.target.clone() }
    fn blocks_dependents(&self) -> bool { self.blocks }

    fn build(&self, _: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let name = self.name.clone();
        let deps = self.deps.clone();
        Ok(ctx.new_job(name.clone(), display(&self.target), Box::new(move |job| {
            run_node(&name, &job)?;
            let ids = deps.iter().map(|d| job.ctx.anubis.build_rule(d, &job.ctx)).collect::<anyhow::Result<Vec<_>>>()?;
            if ids.is_empty() {
                return Ok(JobOutcome::Success(Arc::new(Done)));
            }
            let continuation_job = job.ctx.new_job(format!("{name} done"), job.display.clone(), Box::new(|_| Ok(JobOutcome::Success(Arc::new(Done)))));
            Ok(JobOutcome::Deferred(JobDeferral { blocked_by: ids, continuation_job }))
        })))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        self.deps.iter().map(|d| ctx.anubis.preload_rule(d.clone(), &ctx)).collect()
    }
}

impl crate::papyrus::PapyrusObjectType for Node {
    fn name() -> &'static str { "node" }
}

#[test]
fn blocked_rule_starts_its_other_deps_at_once_and_runs_the_blocking_chain_urgent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
    let write = |rel: &str, text: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(".anubis_root", "anubis_root()\n");
    write("mode/ANUBIS", r#"mode(name = "a", vars = { target_platform = "linux" })"#);
    write("toolchains/ANUBIS", r#"toolchain(name = "default", build_mode = Target("//mode:a"))"#);
    // root -> lib -> gate (blocking) -> gen; root -> sibling.
    write("pkg/ANUBIS", r#"
        node(name = "root", deps = Targets([":lib", ":sibling"]))
        node(name = "lib", deps = Targets([":gate"]))
        node(name = "gate", blocks = true, deps = Targets([":gen"]))
        node(name = "gen")
        node(name = "sibling")
    "#);

    let anubis = Anubis::new(root, false, &AnubisConfig {}).unwrap();
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("node".into()),
        parse_rule: |t, v| {
            let mut rule = Node::deserialize(crate::papyrus_serde::ValueDeserializer::new(v))?;
            rule.target = t;
            Ok(Arc::new(rule))
        },
    }).unwrap();
    let anubis = Arc::new(anubis);

    let progress = ProgressDisplay::new(DisplayMode::Quiet, 2, false);
    build_targets(
        anubis,
        &[AnubisTarget::new("//mode:a").unwrap()],
        &AnubisTarget::new(DEFAULT_TOOLCHAIN).unwrap(),
        &[AnubisTarget::new("//pkg:root").unwrap()],
        2,
        progress.sender(),
    )
    .unwrap();

    let events = LOG.events.lock().unwrap().clone();
    for expected in ["start gate urgent=true", "start gen urgent=true", "start sibling urgent=false", "start root urgent=false"] {
        assert!(events.iter().any(|e| e == expected), "missing [{expected}] in {events:?}");
    }
}

#[test]
fn urgent_jobs_keep_top_priority() {
    let job_system = Arc::new(JobSystem::new());
    let ctx = Arc::new(crate::job_system_tests::make_test_job_context(job_system));
    let job = |ctx: &Arc<JobContext>| ctx.new_job("j".into(), JobDisplayInfo::from_desc("j"), Box::new(|_| Ok(JobOutcome::Success(Arc::new(Done)))));
    assert_eq!(job(&ctx).with_priority(5).priority, 5);
    let urgent = Arc::new(ctx.as_urgent());
    assert_eq!(job(&urgent).priority, Job::PRIORITY_URGENT);
    assert_eq!(job(&urgent).with_priority(5).priority, Job::PRIORITY_URGENT);
}
