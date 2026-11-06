//! Building without a mode: a modeless rule builds its deps in modes it picks.

use std::sync::Arc;

use camino::Utf8PathBuf;
use serde::Deserialize;

use crate::anubis::*;
use crate::job_system::*;
use crate::progress::{DisplayMode, ProgressDisplay};

/// Reports the mode it was built in.
#[derive(Debug, Deserialize)]
struct ModeProbe {
    name: String,
    #[serde(skip_deserializing)]
    target: AnubisTarget,
}

/// Modeless: builds `dep` in each of `modes`, like a package of several builds.
#[derive(Debug, Deserialize)]
struct Bundle {
    name: String,
    dep: AnubisTarget,
    modes: Vec<AnubisTarget>,
    #[serde(skip_deserializing)]
    target: AnubisTarget,
}

#[derive(Debug)]
struct ModeNames(Vec<String>);
impl JobArtifact for ModeNames {}

fn mode_name(ctx: &JobContext) -> String {
    ctx.mode.as_ref().map_or("modeless".to_owned(), |m| m.name.clone())
}

fn display(target: &AnubisTarget) -> JobDisplayInfo {
    JobDisplayInfo { verb: "Test", short_name: target.target_name().to_owned(), detail: target.target_path().to_owned() }
}

impl Rule for ModeProbe {
    fn name(&self) -> String { self.name.clone() }
    fn target(&self) -> AnubisTarget { self.target.clone() }

    fn build(&self, _: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        Ok(ctx.new_job("probe".into(), display(&self.target), Box::new(|job| {
            Ok(JobOutcome::Success(Arc::new(ModeNames(vec![mode_name(&job.ctx)]))))
        })))
    }

    fn preload(&self, _: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> { Ok(vec![]) }
}

impl Rule for Bundle {
    fn name(&self) -> String { self.name.clone() }
    fn target(&self) -> AnubisTarget { self.target.clone() }
    fn needs_mode(&self) -> bool { false }

    fn build(&self, _: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let dep = self.dep.clone();
        let modes = self.modes.clone();
        Ok(ctx.new_job("bundle".into(), display(&self.target), Box::new(move |job| {
            let mut dep_ids = Vec::new();
            for mode in &modes {
                let mode = job.ctx.anubis.get_mode(mode)?;
                let dep_ctx = Arc::new(job.ctx.with_mode(mode)?);
                dep_ids.push(job.ctx.anubis.build_rule(&dep, &dep_ctx)?);
            }
            let ids = dep_ids.clone();
            let continuation_job = job.ctx.new_job("bundle done".into(), display(&dep), Box::new(move |job| {
                let mut names = vec![mode_name(&job.ctx)];
                for id in &ids {
                    names.extend(job.ctx.job_system.get_result(*id)?.cast::<ModeNames>()?.0.iter().cloned());
                }
                Ok(JobOutcome::Success(Arc::new(ModeNames(names))))
            }));
            Ok(JobOutcome::Deferred(JobDeferral { blocked_by: dep_ids, continuation_job }))
        })))
    }

    fn preload(&self, _: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> { Ok(vec![]) }
}

impl crate::papyrus::PapyrusObjectType for ModeProbe {
    fn name() -> &'static str { "mode_probe" }
}
impl crate::papyrus::PapyrusObjectType for Bundle {
    fn name() -> &'static str { "bundle" }
}

fn parse<T: Rule + for<'de> Deserialize<'de>>(
    target: AnubisTarget,
    v: &crate::papyrus::Value,
    set_target: fn(&mut T, AnubisTarget),
) -> anyhow::Result<Arc<dyn Rule>> {
    let mut rule = T::deserialize(crate::papyrus_serde::ValueDeserializer::new(v))?;
    set_target(&mut rule, target);
    Ok(Arc::new(rule))
}

fn setup() -> (tempfile::TempDir, Arc<Anubis>) {
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
    let write = |rel: &str, text: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(".anubis_root", "anubis_root()\n");
    write("mode/ANUBIS", r#"
        mode(name = "a", vars = { target_platform = "linux" })
        mode(name = "b", vars = { target_platform = "windows" })
    "#);
    write("toolchains/ANUBIS", r#"toolchain(name = "default", build_mode = Target("//mode:a"))"#);
    write("pkg/ANUBIS", r#"
        mode_probe(name = "probe")
        mode_probe(name = "per_platform", unused = select((target_platform) => { (linux) = "x", default = "y" }))
        bundle(name = "both", dep = Target(":probe"), modes = Targets(["//mode:a", "//mode:b"]))
    "#);

    let anubis = Anubis::new(root, false, &AnubisConfig {}).unwrap();
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("mode_probe".into()),
        parse_rule: |t, v| parse::<ModeProbe>(t, v, |r, t| r.target = t),
    }).unwrap();
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("bundle".into()),
        parse_rule: |t, v| parse::<Bundle>(t, v, |r, t| r.target = t),
    }).unwrap();
    (tmp, Arc::new(anubis))
}

fn build(anubis: &Arc<Anubis>, modes: &[&str], target: &str) -> anyhow::Result<Vec<String>> {
    let modes: Vec<_> = modes.iter().map(|m| AnubisTarget::new(m).unwrap()).collect();
    let toolchain = AnubisTarget::new(DEFAULT_TOOLCHAIN).unwrap();
    let progress = ProgressDisplay::new(DisplayMode::Quiet, 2, false);
    let artifacts = build_targets(anubis.clone(), &modes, &toolchain, &[AnubisTarget::new(target)?], 2, progress.sender())?;
    Ok(artifacts.iter().flat_map(|a| a.cast::<ModeNames>().unwrap().0.clone()).collect())
}

#[test]
fn modeless_rule_builds_its_deps_in_the_modes_it_picks() {
    let (_tmp, anubis) = setup();
    assert_eq!(build(&anubis, &[], "//pkg:both").unwrap(), ["modeless", "a", "b"]);
}

#[test]
fn modeless_build_of_a_rule_that_needs_a_mode_fails_clearly() {
    let (_tmp, anubis) = setup();
    let job_system = Arc::new(JobSystem::new());
    let ctx = Arc::new(JobContext { anubis: anubis.clone(), job_system: job_system.clone(), mode: None, toolchain: None });
    let id = anubis.preload_rule(AnubisTarget::new("//pkg:probe").unwrap(), &ctx).unwrap();
    let progress = ProgressDisplay::new(DisplayMode::Quiet, 1, false);
    assert!(JobSystem::run_to_completion(job_system.clone(), 1, progress.sender()).is_err());
    let err = job_system.get_result(id).unwrap_err();
    assert!(format!("{err:#}").contains("[//pkg:probe] needs a mode"), "{err:#}");
}

#[test]
fn rules_that_need_no_mode_still_build_with_one() {
    let (_tmp, anubis) = setup();
    assert_eq!(build(&anubis, &["//mode:b"], "//pkg:probe").unwrap(), ["b"]);
}
