//! `py_env`: run Python in a uv project whose native code Anubis builds.
//!
//! Experimental (`python` branch). uv owns the Python side: `pyproject.toml`,
//! `uv.lock`, `.venv`. Native code is declared where the wheels already
//! declare it, in each member's `[tool.hatch.build.hooks.anubis]` table
//! (`targets`, `native_dir`), which the hatch-anubis build hook reads too.
//!
//! ```text
//! py_env(
//!     name = "py",
//!     project = RelPath("."),           # dir with the root pyproject.toml
//!     sync_args = ["--all-packages"],   # extra `uv sync` arguments
//! )
//! ```
//!
//! `anubis run -m //mode:linux_dev -t //:py -- python examples/foo.py`
//!
//! 1. Build: every native target named by the project's members, in the mode.
//! 2. Sync: `uv sync`, with `ANUBIS_SKIP_NATIVE=1` so editable installs of
//!    members carry no native code (see `python/hatch-anubis`).
//! 3. Copy: each member's outputs into `<venv site-packages>/<native_dir>/`,
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
use std::sync::Arc;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PyEnv {
    pub name: String,
    /// Directory holding the uv project's (or workspace's) root `pyproject.toml`.
    pub project: Utf8PathBuf,
    /// Extra arguments for `uv sync`, e.g. `["--all-packages"]` or `["--extra", "gui"]`.
    #[serde(default)]
    pub sync_args: Vec<String>,

    /// Native payloads found in the members' pyproject files, read once at parse time.
    #[serde(skip_deserializing)]
    payloads: Vec<PayloadSpec>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

#[derive(Clone, Debug)]
struct PayloadSpec {
    /// Member whose pyproject declared it (for messages).
    pyproject: Utf8PathBuf,
    native_dir: String,
    targets: Vec<AnubisTarget>,
}

/// One member's built native files and where they go in site-packages.
#[derive(Debug, Clone)]
pub struct Payload {
    pub native_dir: String,
    pub files: Vec<Utf8PathBuf>,
}

/// What building a `py_env` produces; `anubis run` turns it into a venv.
#[derive(Debug)]
pub struct PyEnvArtifact {
    pub project: Utf8PathBuf,
    pub sync_args: Vec<String>,
    pub payloads: Vec<Payload>,
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
        self.payloads
            .iter()
            .flat_map(|p| p.targets.iter())
            .cloned()
            .map(|t| ctx.anubis.preload_rule(t, &ctx))
            .collect()
    }
}

fn parse_py_env(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut rule = PyEnv::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    rule.payloads = read_payload_specs(&rule.project).with_context(|| format!("py_env {}", t.target_path()))?;
    rule.target = t;
    Ok(Arc::new(rule))
}

/// The root pyproject and every `[tool.uv.workspace] members` glob, minus
/// `exclude`; each one with a `[tool.hatch.build.hooks.anubis]` table.
fn read_payload_specs(project: &Utf8Path) -> anyhow::Result<Vec<PayloadSpec>> {
    let root_pyproject = project.join("pyproject.toml");
    let root = read_toml(&root_pyproject)?;
    let workspace = root.get("tool").and_then(|t| t.get("uv")).and_then(|u| u.get("workspace"));
    let strings = |key: &str| -> Vec<String> {
        workspace
            .and_then(|w| w.get(key))
            .and_then(|m| m.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    let expand = |patterns: Vec<String>| -> anyhow::Result<Vec<Utf8PathBuf>> {
        let mut dirs = Vec::new();
        for pattern in patterns {
            let full = project.join(&pattern);
            for entry in glob::glob(full.as_str()).with_context(|| format!("bad member glob {pattern}"))? {
                let path = Utf8PathBuf::try_from(entry?)?;
                if path.join("pyproject.toml").is_file() {
                    dirs.push(path);
                }
            }
        }
        Ok(dirs)
    };
    let excluded = expand(strings("exclude"))?;
    let mut members = vec![project.to_owned()];
    members.extend(expand(strings("members"))?.into_iter().filter(|m| !excluded.contains(m)));
    members.dedup();

    let mut specs = Vec::new();
    for member in members {
        let pyproject = member.join("pyproject.toml");
        let doc = if member == project { root.clone() } else { read_toml(&pyproject)? };
        let Some(hook) = doc
            .get("tool")
            .and_then(|t| t.get("hatch"))
            .and_then(|h| h.get("build"))
            .and_then(|b| b.get("hooks"))
            .and_then(|h| h.get("anubis"))
        else {
            continue;
        };
        let native_dir = hook
            .get("native_dir")
            .and_then(|d| d.as_str())
            .ok_or_else(|| anyhow_loc!("{pyproject}: [tool.hatch.build.hooks.anubis] needs `native_dir`"))?;
        let targets = hook
            .get("targets")
            .and_then(|t| t.as_array())
            .ok_or_else(|| anyhow_loc!("{pyproject}: [tool.hatch.build.hooks.anubis] needs `targets`"))?
            .iter()
            .map(|t| {
                let s = t.as_str().ok_or_else(|| anyhow_loc!("{pyproject}: targets must be strings"))?;
                if !s.starts_with("//") {
                    bail_loc!("{pyproject}: target {s:?} must be absolute (//path:name)");
                }
                AnubisTarget::new(s)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let native_dir = native_dir.trim_matches('/').to_owned();
        if native_dir.is_empty() || native_dir.split('/').any(|c| c == ".." || c == ".") {
            bail_loc!("{pyproject}: native_dir {native_dir:?} must be a relative path inside site-packages");
        }
        specs.push(PayloadSpec { pyproject, native_dir, targets });
    }
    Ok(specs)
}

fn read_toml(path: &Utf8Path) -> anyhow::Result<toml::Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    toml::from_str(&text).with_context(|| format!("parsing {path}"))
}

fn build_py_env(rule: Arc<PyEnv>, job: Job) -> anyhow::Result<JobOutcome> {
    let mut groups: Vec<(PayloadSpec, Vec<JobId>)> = Vec::new();
    let mut blocked_by = Vec::new();
    for spec in &rule.payloads {
        let ids = spec
            .targets
            .iter()
            .map(|t| job.ctx.anubis.build_rule(t, &job.ctx))
            .collect::<anyhow::Result<Vec<_>>>()?;
        blocked_by.extend(ids.iter().copied());
        groups.push((spec.clone(), ids));
    }

    let rule2 = rule.clone();
    let finish = move |final_job: Job| -> anyhow::Result<JobOutcome> {
        let mut payloads = Vec::new();
        for (spec, ids) in &groups {
            let mut files: Vec<Utf8PathBuf> = Vec::new();
            for id in ids {
                let artifact = final_job.ctx.job_system.get_result(*id)?;
                if let Ok(out) = artifact.clone().downcast_arc::<CcBuildOutput>() {
                    files.extend(out.runtime_files.iter().cloned());
                } else if let Ok(exe) = artifact.downcast_arc::<CompileExeArtifact>() {
                    files.push(exe.output_file.clone());
                } else {
                    bail_loc!("{}: a target in `targets` produced no shared library or executable", spec.pyproject);
                }
            }
            files.dedup();
            payloads.push(Payload { native_dir: spec.native_dir.clone(), files });
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

    install_payloads(&platlib, &artifact.payloads, mode)?;

    // Run.
    let mut run = base_env();
    run.args(["run", "--no-sync", "--"]).args(args);
    exec_or_wait(run)
}

/// Copy each payload into `platlib/<native_dir>/` (see the module docs).
pub fn install_payloads(platlib: &Utf8Path, payloads: &[Payload], mode: &AnubisTarget) -> anyhow::Result<()> {
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
        for src in &payload.files {
            let name = crate::file_name!(src)?.to_owned();
            if names.contains(&name) {
                bail_loc!("two native files named {name} for {}", payload.native_dir);
            }
            copy_if_changed(src, &dir.join(&name))?;
            names.push(name);
        }
        for stale in old.iter().filter(|o| !names.contains(o)) {
            let path = dir.join(stale);
            if path.exists() {
                remove_or_set_aside(&path)?;
            }
        }
        names.sort();
        write_atomic(&manifest_path, (names.join("\n") + "\n").as_bytes())?;
        write_atomic(&dir.join("MODE"), format!("{}\n", mode.target_path()).as_bytes())?;
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

        let p = |files: &[&str]| vec![Payload {
            native_dir: "pkg/_native".into(),
            files: files.iter().map(|f| out.join(f)).collect(),
        }];
        install_payloads(&platlib, &p(&["a.so", "b.so"]), &mode).unwrap();
        let native = platlib.join("pkg/_native");
        assert_eq!(std::fs::read_to_string(native.join("a.so")).unwrap(), "a1");
        assert_eq!(std::fs::read_to_string(native.join("MANIFEST")).unwrap(), "a.so\nb.so\n");
        assert_eq!(std::fs::read_to_string(native.join("MODE")).unwrap(), "//mode:linux_dev\n");

        // A file someone else put there survives; a stale output doesn't.
        std::fs::write(native.join("user.txt"), "keep").unwrap();
        std::fs::write(out.join("a.so"), "a2").unwrap();
        install_payloads(&platlib, &p(&["a.so"]), &mode).unwrap();
        assert_eq!(std::fs::read_to_string(native.join("a.so")).unwrap(), "a2");
        assert!(!native.join("b.so").exists());
        assert!(native.join("user.txt").exists());
        assert_eq!(std::fs::read_to_string(native.join("MANIFEST")).unwrap(), "a.so\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_hook_tables_from_workspace_members() {
        let root = tmpdir("specs");
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='r'\n[tool.uv.workspace]\nmembers=['pkgs/*']\nexclude=['pkgs/skip']\n",
        )
        .unwrap();
        for (m, hook) in [("a", true), ("b", false), ("skip", true)] {
            let d = root.join("pkgs").join(m);
            std::fs::create_dir_all(&d).unwrap();
            let mut text = format!("[project]\nname='{m}'\n");
            if hook {
                text += "[tool.hatch.build.hooks.anubis]\ntargets=['//x:ext']\nnative_dir='a/_native/'\n";
            }
            std::fs::write(d.join("pyproject.toml"), text).unwrap();
        }
        let specs = read_payload_specs(&root).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].native_dir, "a/_native");
        assert_eq!(specs[0].targets[0].target_path(), "//x:ext");
        let _ = std::fs::remove_dir_all(&root);
    }
}
