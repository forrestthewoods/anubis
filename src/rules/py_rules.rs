//! `py_package` and `py_env`: Python packages whose native code Anubis builds.
//!
//! Experimental (`python` branch). uv owns the Python side: `pyproject.toml`,
//! `uv.lock`, `.venv`. Anubis owns the native side, described here, in
//! ANUBIS files. Anubis never reads a `pyproject.toml`.
//!
//! ```text
//! py_package(
//!     name = "wheel",
//!     native_dir = "hello_pkg/_native",     # where the files go in site-packages
//!     targets = Targets(["//samples/basic/python_ext:hello"]),
//!     deps = Targets(["//other:wheel"]),    # optional: packages this one needs
//!     mode_vars = { precision = "double" }, # optional: mode override
//! )
//! ```
//!
//! A package's files are its targets' outputs (shared libraries, extension
//! modules, executables, their runtime shared libraries and `resources`),
//! minus files with the same name that a `deps` package ships (transitively):
//! a shared library two packages link is shipped once, by the lower one,
//! which Python imports first.
//!
//! `mode_vars` overrides mode variables for this package's targets: they are
//! built in a mode derived from the one asked for (`linux_dev` ->
//! `linux_dev+precision=double`, with its own output directory), so a
//! double-precision package and its single-precision sibling build side by
//! side in one `anubis build` and live in one venv. `deps` packages are
//! built in the mode asked for, with their own overrides.
//!
//! The wheel's tag (`cp312-cp312-manylinux_2_28_x86_64`) is the toolchain's
//! `PyToolchain.wheel_tag`. The hatch-anubis build hook names one
//! `py_package` (`target = "//x:wheel"`) and gets files, destinations and
//! tag from `anubis build --outputs-json`.
//!
//! ```text
//! py_env(
//!     name = "py",
//!     project = RelPath("."),           # dir with the uv root pyproject.toml
//!     packages = Targets([":wheel"]),   # py_packages to put in the venv
//!     sync_args = ["--all-packages"],   # extra `uv sync` arguments
//! )
//! ```
//!
//! `anubis run -m //mode:linux_dev -t //:py -- python examples/foo.py`
//!
//! 1. Build: every package, in the mode (plus each package's overrides).
//! 2. Sync: `uv sync`, with `ANUBIS_SKIP_NATIVE=1` so editable installs of
//!    members carry no native code (see `python/hatch-anubis`).
//! 3. Copy: each package's files into `<venv site-packages>/<native_dir>/`,
//!    only files that changed, each written to a temp name and renamed into
//!    place (overwriting a loaded `.so` crashes the process using it). A
//!    `MANIFEST` lists what Anubis put there, so files from earlier runs that
//!    are no longer outputs are deleted; `MODE` (written last) names the mode.
//! 4. Run: `uv run --no-sync -- <args>` with the user's environment.
//!
//! Steps 2-4 happen in `anubis run` (see [`run_py_env`]); building the target
//! alone only builds the native code.

use crate::anubis::{self, AnubisTarget};
use crate::job_system::*;
use crate::rules::cc_rules::{CcBuildOutput, CompileExeArtifact};
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{anyhow_loc, bail_loc, function_name};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

// ----------------------------------------------------------------------------
// py_package
// ----------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PyPackage {
    pub name: String,
    /// Directory inside site-packages (and the wheel) for the native files.
    pub native_dir: String,
    /// Targets whose outputs the package ships.
    pub targets: Vec<AnubisTarget>,
    /// Other `py_package`s this one needs; files they ship are left out.
    #[serde(default)]
    pub deps: Vec<AnubisTarget>,
    /// Mode variables to override for `targets`.
    #[serde(default)]
    pub mode_vars: BTreeMap<String, String>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

/// A built `py_package`: its native files and where they go.
#[derive(Debug, Clone)]
pub struct Payload {
    pub native_dir: String,
    /// (source, path inside `native_dir`): a file name, or a `resources`
    /// path relative to the executable such as `assets/cube.obj`.
    pub files: Vec<(Utf8PathBuf, String)>,
    /// Names of the files this package and its deps ship.
    pub shipped: BTreeSet<String>,
    /// The mode the targets were built in (after `mode_vars`).
    pub mode: AnubisTarget,
    /// Wheel tag, e.g. `cp312-cp312-win_amd64`.
    pub wheel_tag: String,
}

impl JobArtifact for Payload {}

impl crate::papyrus::PapyrusObjectType for PyPackage {
    fn name() -> &'static str {
        "py_package"
    }
}

impl PyPackage {
    /// `ctx` with this package's `mode_vars` applied.
    fn targets_ctx(&self, ctx: &Arc<JobContext>) -> anyhow::Result<Arc<JobContext>> {
        if self.mode_vars.is_empty() {
            return Ok(ctx.clone());
        }
        let base = ctx.mode.as_ref().ok_or_else(|| anyhow_loc!("py_package needs a mode"))?;
        let mode = ctx.anubis.derive_mode(base, &self.mode_vars)?;
        Ok(Arc::new(ctx.with_mode(mode)?))
    }
}

impl anubis::Rule for PyPackage {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let rule = arc_self
            .clone()
            .downcast_arc::<PyPackage>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to PyPackage", arc_self))?;
        Ok(ctx.new_job(
            format!("Build PyPackage: {}", self.target),
            JobDisplayInfo {
                verb: "Building",
                short_name: self.name.clone(),
                detail: format!("py_package {}", self.target),
            },
            Box::new(move |job| build_py_package(rule.clone(), job)),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        let targets_ctx = self.targets_ctx(&ctx)?;
        let mut ids = Vec::new();
        for t in &self.targets {
            ids.push(ctx.anubis.preload_rule(t.clone(), &targets_ctx)?);
        }
        for d in &self.deps {
            ids.push(ctx.anubis.preload_rule(d.clone(), &ctx)?);
        }
        Ok(ids)
    }
}

fn parse_py_package(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut rule = PyPackage::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    rule.native_dir = rule.native_dir.trim_matches('/').to_owned();
    if rule.native_dir.is_empty() || rule.native_dir.split('/').any(|c| c.is_empty() || c == ".." || c == ".") {
        bail_loc!("py_package {}: native_dir {:?} must be a relative path inside site-packages", t.target_path(), rule.native_dir);
    }
    if rule.targets.is_empty() {
        bail_loc!("py_package {}: `targets` is empty", t.target_path());
    }
    rule.target = t;
    Ok(Arc::new(rule))
}

fn build_py_package(rule: Arc<PyPackage>, job: Job) -> anyhow::Result<JobOutcome> {
    let targets_ctx = rule.targets_ctx(&job.ctx)?;
    let build = |targets: &[AnubisTarget], ctx: &Arc<JobContext>| {
        targets.iter().map(|t| ctx.anubis.build_rule(t, ctx)).collect::<anyhow::Result<Vec<_>>>()
    };
    let target_ids = build(&rule.targets, &targets_ctx)?;
    let dep_ids = build(&rule.deps, &job.ctx)?;
    let blocked_by: Vec<JobId> = target_ids.iter().chain(&dep_ids).copied().collect();

    let mode = targets_ctx.mode.as_ref().ok_or_else(|| anyhow_loc!("py_package needs a mode"))?.target.clone();
    let wheel_tag = targets_ctx
        .toolchain
        .as_ref()
        .ok_or_else(|| anyhow_loc!("py_package needs a toolchain"))?
        .python()?
        .wheel_tag
        .clone();
    if wheel_tag.is_empty() {
        bail_loc!("the toolchain's PyToolchain has no `wheel_tag` for mode {}", mode.target_path());
    }

    let rule2 = rule.clone();
    let finish = move |final_job: Job| -> anyhow::Result<JobOutcome> {
        let rule = &rule2;
        let by_name = |f: &Utf8PathBuf| -> anyhow::Result<(Utf8PathBuf, String)> {
            Ok((f.clone(), crate::file_name!(f)?.to_owned()))
        };
        let mut shipped = BTreeSet::new();
        for (id, dep) in dep_ids.iter().zip(&rule.deps) {
            let artifact = final_job.ctx.job_system.get_result(*id)?;
            let payload = artifact.downcast_arc::<Payload>().map_err(|_| {
                anyhow_loc!("py_package {}: dep {} is not a py_package", rule.target.target_path(), dep.target_path())
            })?;
            shipped.extend(payload.shipped.iter().cloned());
        }
        let mut found: Vec<(Utf8PathBuf, String)> = Vec::new();
        for (id, target) in target_ids.iter().zip(&rule.targets) {
            let artifact = final_job.ctx.job_system.get_result(*id)?;
            if let Ok(out) = artifact.clone().downcast_arc::<CcBuildOutput>() {
                for f in &out.runtime_files {
                    found.push(by_name(f)?);
                }
            } else if let Ok(exe) = artifact.downcast_arc::<CompileExeArtifact>() {
                found.push(by_name(&exe.output_file)?);
                for f in &exe.runtime_files {
                    found.push(by_name(f)?);
                }
                found.extend(exe.resources.iter().cloned());
            } else {
                bail_loc!(
                    "py_package {}: {} produced no shared library or executable",
                    rule.target.target_path(),
                    target.target_path()
                );
            }
        }
        // Several targets list the same runtime library: keep one. Two
        // different files with one name can't share a directory.
        let mut files: Vec<(Utf8PathBuf, String)> = Vec::new();
        for (src, dest) in found {
            if shipped.contains(&dest) {
                continue;
            }
            match files.iter().find(|(_, d)| *d == dest) {
                Some((other, _)) if *other == src => {}
                Some((other, _)) => bail_loc!(
                    "py_package {}: two files named {dest}: {other} and {src}",
                    rule.target.target_path()
                ),
                None => files.push((src, dest)),
            }
        }
        shipped.extend(files.iter().map(|(_, d)| d.clone()));
        Ok(JobOutcome::Success(Arc::new(Payload {
            native_dir: rule.native_dir.clone(),
            files,
            shipped,
            mode: mode.clone(),
            wheel_tag: wheel_tag.clone(),
        })))
    };
    let display = JobDisplayInfo {
        verb: "Collecting",
        short_name: rule.name.clone(),
        detail: format!("py_package {}", rule.target),
    };
    let continuation_job = job.ctx.new_job(format!("{} (collect)", job.desc), display, Box::new(finish));
    Ok(JobOutcome::Deferred(JobDeferral { blocked_by, continuation_job }))
}

// ----------------------------------------------------------------------------
// py_env
// ----------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PyEnv {
    pub name: String,
    /// Directory holding the uv project's (or workspace's) root `pyproject.toml`.
    pub project: Utf8PathBuf,
    /// `py_package`s whose native files go in the venv.
    #[serde(default)]
    pub packages: Vec<AnubisTarget>,
    /// Extra arguments for `uv sync`, e.g. `["--all-packages"]` or `["--extra", "gui"]`.
    #[serde(default)]
    pub sync_args: Vec<String>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

/// What building a `py_env` produces; `anubis run` turns it into a venv.
#[derive(Debug)]
pub struct PyEnvArtifact {
    pub project: Utf8PathBuf,
    pub sync_args: Vec<String>,
    pub payloads: Vec<Arc<Payload>>,
}

impl JobArtifact for PyEnvArtifact {}

impl crate::papyrus::PapyrusObjectType for PyEnv {
    fn name() -> &'static str {
        "py_env"
    }
}

impl anubis::Rule for PyEnv {
    fn is_runnable(&self) -> bool {
        true
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let rule = arc_self
            .clone()
            .downcast_arc::<PyEnv>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to PyEnv", arc_self))?;
        Ok(ctx.new_job(
            format!("Build PyEnv: {}", self.target),
            JobDisplayInfo {
                verb: "Building",
                short_name: self.name.clone(),
                detail: format!("py_env {}", self.target),
            },
            Box::new(move |job| build_py_env(rule.clone(), job)),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        self.packages.iter().cloned().map(|t| ctx.anubis.preload_rule(t, &ctx)).collect()
    }
}

fn parse_py_env(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut rule = PyEnv::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    rule.target = t;
    Ok(Arc::new(rule))
}

fn build_py_env(rule: Arc<PyEnv>, job: Job) -> anyhow::Result<JobOutcome> {
    let blocked_by = rule
        .packages
        .iter()
        .map(|t| job.ctx.anubis.build_rule(t, &job.ctx))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let ids = blocked_by.clone();
    let rule2 = rule.clone();
    let finish = move |final_job: Job| -> anyhow::Result<JobOutcome> {
        let mut payloads = Vec::new();
        for (id, package) in ids.iter().zip(&rule2.packages) {
            let payload = final_job.ctx.job_system.get_result(*id)?.downcast_arc::<Payload>().map_err(|_| {
                anyhow_loc!("py_env {}: {} is not a py_package", rule2.target.target_path(), package.target_path())
            })?;
            if let Some(other) = payloads.iter().find(|p: &&Arc<Payload>| p.native_dir == payload.native_dir) {
                bail_loc!(
                    "py_env {}: two packages use native_dir {} ({package} is one)",
                    rule2.target.target_path(),
                    other.native_dir
                );
            }
            payloads.push(payload);
        }
        Ok(JobOutcome::Success(Arc::new(PyEnvArtifact {
            project: rule2.project.clone(),
            sync_args: rule2.sync_args.clone(),
            payloads,
        })))
    };
    let display = JobDisplayInfo {
        verb: "Collecting",
        short_name: rule.name.clone(),
        detail: format!("py_env {}", rule.target),
    };
    let continuation_job = job.ctx.new_job(format!("{} (collect)", job.desc), display, Box::new(finish));
    Ok(JobOutcome::Deferred(JobDeferral { blocked_by, continuation_job }))
}

// ----------------------------------------------------------------------------
// anubis run
// ----------------------------------------------------------------------------

/// Steps 2-4 of `anubis run` for a `py_env`: sync, copy natives, run.
/// `user_env` is the environment `anubis run` was started with.
pub fn run_py_env(
    artifact: &PyEnvArtifact,
    python: &crate::toolchain::PyToolchain,
    mode: &AnubisTarget,
    user_env: &[(std::ffi::OsString, std::ffi::OsString)],
    args: &[String],
) -> anyhow::Result<i32> {
    let uv = &python.uv;
    if !uv.exists() {
        bail_loc!("uv not found at {uv}; run `anubis install-toolchains`");
    }
    let base_env = || {
        let mut cmd = std::process::Command::new(uv.as_std_path());
        cmd.current_dir(&artifact.project).env_clear().envs(user_env.iter().map(|(k, v)| (k, v)));
        cmd
    };

    // Sync. Pin the interpreter only when there is no venv yet: forcing it
    // on an existing venv made with another 3.12 makes uv delete and
    // recreate the venv (and the native copies with it).
    let mut sync = base_env();
    sync.arg("sync").args(&artifact.sync_args);
    sync.env("ANUBIS_SKIP_NATIVE", "1").env("ANUBIS_MODE", mode.target_path());
    if let Ok(exe) = std::env::current_exe() {
        sync.env("ANUBIS_EXE", exe);
    }
    let has_venv = user_env.iter().any(|(k, _)| k == "UV_PROJECT_ENVIRONMENT") || artifact.project.join(".venv").exists();
    if !has_venv && python.interpreter.exists() {
        sync.arg("--python").arg(python.interpreter.as_std_path());
    }
    tracing::info!("uv sync in {}", artifact.project);
    let status = sync.status().with_context(|| format!("running {uv} sync"))?;
    if !status.success() {
        bail_loc!("`uv sync` failed in {} ({status})", artifact.project);
    }

    // Where the venv keeps packages: ask its interpreter (Lib/site-packages
    // on Windows, lib/python3.X/site-packages elsewhere).
    let out = base_env()
        .args(["run", "--no-sync", "python", "-c", "import sysconfig; print(sysconfig.get_paths()['platlib'])"])
        .output()
        .with_context(|| format!("running {uv} run"))?;
    if !out.status.success() {
        bail_loc!("could not find the venv's site-packages: {}", String::from_utf8_lossy(&out.stderr));
    }
    let platlib = Utf8PathBuf::from(String::from_utf8(out.stdout)?.trim().to_owned());

    install_payloads(&platlib, &artifact.payloads)?;

    // Run.
    let mut run = base_env();
    run.args(["run", "--no-sync", "--"]).args(args);
    exec_or_wait(run)
}

/// Copy each payload into `platlib/<native_dir>/` (see the module docs).
pub fn install_payloads(platlib: &Utf8Path, payloads: &[Arc<Payload>]) -> anyhow::Result<()> {
    std::fs::create_dir_all(platlib)?;
    let lock_path = platlib.join(".anubis-native.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {lock_path}"))?;
    lock.lock().with_context(|| format!("locking {lock_path}"))?;

    for payload in payloads {
        let dir = platlib.join(&payload.native_dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {dir}"))?;
        clean_old_files(&dir);

        let manifest_path = dir.join("MANIFEST");
        let old: Vec<String> = std::fs::read_to_string(&manifest_path)
            .map(|s| s.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default();

        let mut names: Vec<String> = Vec::new();
        for (src, name) in &payload.files {
            if names.contains(name) {
                bail_loc!("two native files named {name} for {}", payload.native_dir);
            }
            if name.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
                bail_loc!("bad native file path {name:?} for {}", payload.native_dir);
            }
            let dst = dir.join(name);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("creating {parent}"))?;
            }
            copy_if_changed(src, &dst)?;
            names.push(name.clone());
        }
        for stale in old.iter().filter(|o| !names.contains(o)) {
            let path = dir.join(stale);
            if path.exists() {
                remove_or_set_aside(&path)?;
            }
        }
        names.sort();
        write_atomic(&manifest_path, (names.join("\n") + "\n").as_bytes())?;
        write_atomic(&dir.join("MODE"), format!("{}\n", payload.mode.target_path()).as_bytes())?;
    }
    drop(lock);
    Ok(())
}

fn copy_if_changed(src: &Utf8Path, dst: &Utf8Path) -> anyhow::Result<()> {
    let new = std::fs::read(src).with_context(|| format!("reading {src}"))?;
    if std::fs::read(dst).ok().as_deref() == Some(new.as_slice()) {
        return Ok(());
    }
    write_atomic(dst, &new)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(src)?.permissions().mode();
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode | 0o200))?;
    }
    Ok(())
}

/// Write to a temp file next to `dst`, then rename it over `dst`. A process
/// that has the old file mapped keeps its (now unlinked) copy. Windows can't
/// replace a loaded DLL, but can rename it, so the old file moves aside first.
fn write_atomic(dst: &Utf8Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = Utf8PathBuf::from(format!("{dst}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {tmp}"))?;
    if std::fs::rename(&tmp, dst).is_ok() {
        return Ok(());
    }
    if dst.exists() {
        remove_or_set_aside(dst)?;
    }
    std::fs::rename(&tmp, dst).with_context(|| format!("renaming {tmp} to {dst}"))
}

fn remove_or_set_aside(path: &Utf8Path) -> anyhow::Result<()> {
    if std::fs::remove_file(path).is_ok() {
        return Ok(());
    }
    for n in 0..1000 {
        let aside = Utf8PathBuf::from(format!("{path}.old-{n}"));
        if !aside.exists() {
            return std::fs::rename(path, &aside).with_context(|| format!("moving {path} aside (is it in use?)"));
        }
    }
    bail_loc!("could not move {path} aside")
}

/// Best effort: delete files set aside (`*.old-N`) and temp files left by
/// earlier runs. Ones still loaded somewhere stay until next time.
fn clean_old_files(dir: &Utf8Path) {
    let Ok(entries) = dir.read_dir_utf8() else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.contains(".old-") || name.contains(".tmp-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(unix)]
fn exec_or_wait(mut cmd: std::process::Command) -> anyhow::Result<i32> {
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    Err(anyhow_loc!("could not exec uv: {err}"))
}

#[cfg(not(unix))]
fn exec_or_wait(mut cmd: std::process::Command) -> anyhow::Result<i32> {
    let status = cmd.status().context("running uv")?;
    Ok(status.code().unwrap_or(1))
}

pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("py_env".to_owned()),
        parse_rule: parse_py_env,
    })?;
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("py_package".to_owned()),
        parse_rule: parse_py_package,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> Utf8PathBuf {
        let dir = std::env::temp_dir().join(format!("anubis-py-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Utf8PathBuf::try_from(dir).unwrap()
    }

    #[test]
    fn install_payloads_copies_and_removes_stale() {
        let root = tmpdir("install");
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("a.so"), "a1").unwrap();
        std::fs::write(out.join("b.so"), "b1").unwrap();
        let platlib = root.join("site-packages");
        let mode = AnubisTarget::new("//mode:linux_dev").unwrap();

        let payload = |files: Vec<(Utf8PathBuf, String)>| {
            vec![Arc::new(Payload {
                native_dir: "pkg/_native".into(),
                files,
                shipped: Default::default(),
                mode: mode.clone(),
                wheel_tag: String::new(),
            })]
        };
        let p = |files: &[&str]| payload(files.iter().map(|f| (out.join(f), f.to_string())).collect());
        install_payloads(&platlib, &p(&["a.so", "b.so"])).unwrap();
        let native = platlib.join("pkg/_native");
        assert_eq!(std::fs::read_to_string(native.join("a.so")).unwrap(), "a1");
        assert_eq!(std::fs::read_to_string(native.join("MANIFEST")).unwrap(), "a.so\nb.so\n");
        assert_eq!(std::fs::read_to_string(native.join("MODE")).unwrap(), "//mode:linux_dev\n");

        // A file someone else put there survives; a stale output doesn't.
        std::fs::write(native.join("user.txt"), "keep").unwrap();
        std::fs::write(out.join("a.so"), "a2").unwrap();
        install_payloads(&platlib, &p(&["a.so"])).unwrap();
        assert_eq!(std::fs::read_to_string(native.join("a.so")).unwrap(), "a2");
        assert!(!native.join("b.so").exists());
        assert!(native.join("user.txt").exists());
        assert_eq!(std::fs::read_to_string(native.join("MANIFEST")).unwrap(), "a.so\n");

        // Resources keep their relative path.
        std::fs::create_dir_all(out.join("assets")).unwrap();
        std::fs::write(out.join("assets/cube.obj"), "cube").unwrap();
        install_payloads(&platlib, &p(&["a.so", "assets/cube.obj"])).unwrap();
        assert_eq!(std::fs::read_to_string(native.join("assets/cube.obj")).unwrap(), "cube");
        install_payloads(&platlib, &p(&["a.so"])).unwrap();
        assert!(!native.join("assets/cube.obj").exists());
        let bad = payload(vec![(out.join("a.so"), "../a.so".into())]);
        assert!(install_payloads(&platlib, &bad).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn derive_mode_overrides_vars_and_gets_its_own_name() {
        let root = tmpdir("derive");
        let anubis = Anubis::new(root.clone(), false, &Default::default()).unwrap();
        let base = Arc::new(crate::toolchain::Mode {
            name: "linux_dev".into(),
            vars: [("precision".to_owned(), "single".to_owned()), ("build_type".to_owned(), "debug".to_owned())].into(),
            target: AnubisTarget::new("//mode:linux_dev").unwrap(),
        });
        let vars = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        // Nothing changes: the same mode, so the same output directory.
        let same = anubis.derive_mode(&base, &vars(&[("precision", "single")])).unwrap();
        assert!(Arc::ptr_eq(&same, &base));

        let double = anubis.derive_mode(&base, &vars(&[("precision", "double"), ("build_type", "debug")])).unwrap();
        assert_eq!(double.target.target_path(), "//mode:linux_dev+precision=double");
        assert_eq!(double.vars["precision"], "double");
        assert_eq!(double.vars["build_type"], "debug");
        assert_ne!(anubis.out_dir(&Some(double.clone()), &base.target, "x"), anubis.out_dir(&Some(base.clone()), &base.target, "x"));
        // One mode object per name.
        let again = anubis.derive_mode(&base, &vars(&[("precision", "double")])).unwrap();
        assert!(Arc::ptr_eq(&again, &double));
        assert!(anubis.derive_mode(&base, &vars(&[("precision", "a/b")])).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
