//! `py_wheel`: a Python wheel that carries a `py_package`'s native files.
//!
//! ```text
//! py_wheel(
//!     name = "wheel",
//!     package = Target(":native"),  # the py_package: native files, native_dir
//!     project = RelPath("."),       # directory with the pyproject.toml
//!     srcs = glob(["hello_pkg/**/*.py"]),
//! )
//! ```
//!
//! `anubis build -m //mode:win_release -t //pkg:wheel` writes
//! `.anubis-out/<mode>_<hash>/<dirname>_<target>_<hash>/py_wheel/name-1.0-cp312-cp312-win_amd64.whl`
//! (`--outputs-json` lists the path). Two cached actions:
//!
//! 1. The Python part. hatchling, the project's build backend, runs in
//!    `project` and writes a pure wheel (`name-1.0-py3-none-any.whl`) with
//!    all the metadata: METADATA, entry points, licenses. It is a build tool
//!    like any other: the toolchain's interpreter and a hatchling pinned in
//!    `natron.toml` (`PyToolchain.hatchling`), an empty environment, no
//!    network. The project is plain Python as far as hatchling knows.
//!    `srcs` and `project`'s `pyproject.toml` are the files it reads: the
//!    action's inputs.
//! 2. The final wheel: every entry of the pure one, the native files under
//!    `native_dir`, the toolchain's tag in `WHEEL` and in the file name, and
//!    a new `RECORD`. A wheel is a zip whose layout is fixed by PEP 427;
//!    this step touches two lines of `WHEEL` and rewrites `RECORD` (the list
//!    of files and their SHA-256).
//!
//! Anubis reads no `pyproject.toml`: name, version and dependencies stay
//! there, where uv needs them too. Only hatchling projects are supported.
//! Without a sandbox (#209) a file hatchling reads that is not in `srcs`
//! does not cause a rebuild.

use crate::anubis::{self, AnubisTarget, Rule, RuleTypeInfo, RuleTypename};
use crate::build_cache::{remove_outputs, ActionOutput, KeyBuilder};
use crate::job_system::*;
use crate::rules::py_rules::Payload;
use crate::rules::rule_utils::{ensure_directory, run_command_verbose};
use crate::{anyhow_loc, bail_loc, function_name};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::sync::Arc;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PyWheel {
    pub name: String,
    /// The `py_package` whose native files the wheel carries.
    pub package: AnubisTarget,
    /// Directory with the package's `pyproject.toml`.
    pub project: Utf8PathBuf,
    /// The files hatchling reads besides `pyproject.toml`: the Python
    /// sources, the readme and license if the project names them.
    pub srcs: Vec<Utf8PathBuf>,

    #[serde(skip_deserializing)]
    target: AnubisTarget,
}

/// The wheel a `py_wheel` rule wrote.
#[derive(Debug)]
pub struct WheelArtifact {
    pub output_file: Utf8PathBuf,
}
impl JobArtifact for WheelArtifact {}

impl crate::papyrus::PapyrusObjectType for PyWheel {
    fn name() -> &'static str {
        "py_wheel"
    }
}

impl Rule for PyWheel {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let rule = arc_self
            .downcast_arc::<PyWheel>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule to PyWheel"))?;
        Ok(ctx.new_job(
            format!("Build PyWheel: {}", self.target),
            JobDisplayInfo {
                verb: "Building",
                short_name: self.name.clone(),
                detail: format!("py_wheel {}", self.target),
            },
            Box::new(move |job| {
                let package_job = job.ctx.anubis.build_rule(&rule.package, &job.ctx)?;
                let rule = rule.clone();
                let display = JobDisplayInfo {
                    verb: "Packaging",
                    short_name: rule.name.clone(),
                    detail: format!("py_wheel {}", rule.target),
                };
                let continuation_job = job.ctx.new_job(
                    format!("{} (wheel)", job.desc),
                    display,
                    Box::new(move |job| {
                        let payload =
                            job.ctx.job_system.get_result(package_job)?.downcast_arc::<Payload>().map_err(|_| {
                                anyhow_loc!(
                                    "py_wheel {}: {} is not a py_package",
                                    rule.target.target_path(),
                                    rule.package.target_path()
                                )
                            })?;
                        let output_file = build_wheel(&rule, &payload, &job.ctx)?;
                        Ok(JobOutcome::Success(Arc::new(WheelArtifact { output_file })))
                    }),
                );
                Ok(JobOutcome::Deferred(JobDeferral { blocked_by: vec![package_job], continuation_job }))
            }),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        Ok(vec![ctx.anubis.preload_rule(self.package.clone(), &ctx)?])
    }
}

fn parse_py_wheel(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut rule = PyWheel::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    // `glob(["pkg/**/*"])` also matches directories.
    rule.srcs.retain(|p| !p.is_dir());
    // An input too; the rule knows where it is.
    rule.srcs.push(rule.project.join("pyproject.toml"));
    rule.srcs.sort();
    rule.srcs.dedup();
    rule.target = t;
    Ok(Arc::new(rule))
}

pub fn register_rule_typeinfos(anubis: &anubis::Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("py_wheel".to_owned()),
        parse_rule: parse_py_wheel,
    })
}

/// What the interpreter runs for step 1. argv: project, output directory,
/// then the `sys.path` entries for hatchling.
const RUN_HATCHLING: &str = "\
import os, sys
project, out = sys.argv[1], sys.argv[2]
sys.path[:0] = sys.argv[3:]
os.chdir(project)
sys.argv = ['hatchling', 'build', '-t', 'wheel', '-d', out]
from hatchling.cli import hatchling
sys.exit(hatchling())
";

fn build_wheel(rule: &PyWheel, payload: &Payload, ctx: &JobContext) -> anyhow::Result<Utf8PathBuf> {
    let out_dir = ctx.anubis.out_dir(&ctx.mode, &rule.target, "py_wheel");
    let pure = build_pure_wheel(rule, ctx, &out_dir.join("pure"))?;

    let cache = ctx.anubis.build_cache.as_ref();
    let mut key = None;
    if let Some(cache) = cache {
        let mut kb = KeyBuilder::new("py_wheel");
        kb.field(&payload.native_dir).field(&payload.wheel_tag);
        let inputs: Vec<Utf8PathBuf> =
            std::iter::once(pure.clone()).chain(payload.files.iter().map(|(src, _)| src.clone())).collect();
        if !cache.key_add_files(&mut kb, &inputs)? {
            bail_loc!("py_wheel {}: a native file of {} is missing", rule.target.target_path(), rule.package.target_path());
        }
        for (src, dest) in &payload.files {
            // The executable bit sets the entry's permissions, so it is an
            // input like the bytes are.
            let meta = std::fs::metadata(src).with_context(|| format!("reading {src}"))?;
            kb.field(dest).field(if is_executable(&meta) { "x" } else { "-" });
        }
        let k = kb.finish();
        if let Some(restored) = cache.restore_outputs(&k, &out_dir)? {
            if let [wheel] = restored.as_slice() {
                cache.record_hit();
                remove_wheels(&out_dir, Some(wheel))?;
                return Ok(wheel.clone());
            }
        }
        cache.record_miss();
        key = Some(k);
    }

    remove_wheels(&out_dir, None)?;
    let wheel = add_native_files(&pure, &payload.native_dir, &payload.files, &payload.wheel_tag, &out_dir)?;
    if let (Some(cache), Some(key)) = (cache, key) {
        cache.store(&key, &out_dir, &[ActionOutput { path: wheel.clone(), optional: false }])?;
    }
    Ok(wheel)
}

/// Step 1: hatchling writes the project's pure wheel into `dir`.
fn build_pure_wheel(rule: &PyWheel, ctx: &JobContext, dir: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let toolchain = ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("py_wheel needs a toolchain"))?;
    let python = toolchain.python()?;
    if python.hatchling.is_empty() {
        bail_loc!("the toolchain's PyToolchain has no `hatchling` directories");
    }
    for path in std::iter::once(&python.interpreter).chain(&python.hatchling) {
        if !path.exists() {
            bail_loc!("{path} not found; run `anubis install-toolchains`");
        }
    }
    ensure_directory(dir.as_ref())?;

    // -I: no PYTHON* variables and no user site-packages. -B: no .pyc
    // files written into the hatchling install.
    let mut args: Vec<String> =
        vec!["-I".into(), "-B".into(), "-c".into(), RUN_HATCHLING.into(), rule.project.to_string(), dir.to_string()];
    args.extend(python.hatchling.iter().map(|p| p.to_string()));

    let cache = ctx.anubis.build_cache.as_ref();
    let mut key = None;
    if let Some(cache) = cache {
        let mut kb = cache.key_for_command("py_wheel_pure", &python.interpreter, &args, Some(dir))?;
        for path in &python.hatchling {
            kb.field(cache.tool_identity(path)?);
        }
        if !cache.key_add_files(&mut kb, &rule.srcs)? {
            bail_loc!("py_wheel {}: a file in `srcs` does not exist", rule.target.target_path());
        }
        let k = kb.finish();
        if let Some(restored) = cache.restore_outputs(&k, dir)? {
            if let [wheel] = restored.as_slice() {
                cache.record_hit();
                remove_wheels(dir, Some(wheel))?;
                return Ok(wheel.clone());
            }
        }
        cache.record_miss();
        key = Some(k);
    }

    remove_wheels(dir, None)?;
    let result = run_command_verbose(python.interpreter.as_ref(), &args, ctx.anubis.verbose_tools, &ctx.anubis.root)?;
    if !result.status.success() {
        bail_loc!(
            "hatchling failed in {} [{}]\n  stdout: {}\n  stderr: {}",
            rule.project,
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let wheels = wheels_in(dir)?;
    let [wheel] = wheels.as_slice() else {
        bail_loc!("expected one wheel from hatchling in {}, found {}", rule.project, wheels.len());
    };
    if let (Some(cache), Some(key)) = (cache, key) {
        cache.store(&key, dir, &[ActionOutput { path: wheel.clone(), optional: false }])?;
    }
    Ok(wheel.clone())
}

fn wheels_in(dir: &Utf8Path) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let mut wheels = Vec::new();
    for entry in dir.read_dir_utf8().with_context(|| format!("reading {dir}"))? {
        let path = entry?.into_path();
        if path.extension() == Some("whl") {
            wheels.push(path);
        }
    }
    Ok(wheels)
}

/// Delete the wheels in `dir` other than `keep`: an earlier version's, or
/// one about to be rewritten (it may be a read-only link into the cache).
fn remove_wheels(dir: &Utf8Path, keep: Option<&Utf8PathBuf>) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let old: Vec<Utf8PathBuf> = wheels_in(dir)?.into_iter().filter(|w| Some(w) != keep).collect();
    remove_outputs(old.iter().map(|p| p.as_path()))
}

/// Step 2: `pure` plus `files` (source, path inside `native_dir`), tagged
/// `tag`, written to `out_dir`. Returns the new wheel's path.
pub fn add_native_files(
    pure: &Utf8Path,
    native_dir: &str,
    files: &[(Utf8PathBuf, String)],
    tag: &str,
    out_dir: &Utf8Path,
) -> anyhow::Result<Utf8PathBuf> {
    // name-version[-build]-python-abi-platform.whl
    let stem = pure.file_stem().unwrap_or_default();
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        bail_loc!("{pure} is not named like a wheel");
    }
    let out_path = out_dir.join(format!("{}-{tag}.whl", parts[..parts.len() - 3].join("-")));
    let tag_parts: Vec<&str> = tag.split('-').collect();
    let [py_tags, abi_tags, platform_tags] = tag_parts.as_slice() else {
        bail_loc!("wheel tag {tag:?} is not python-abi-platform");
    };

    let mut zin = zip::ZipArchive::new(std::fs::File::open(pure).with_context(|| format!("opening {pure}"))?)
        .with_context(|| format!("reading {pure}"))?;
    let names: Vec<String> = zin.file_names().map(str::to_owned).collect();
    let is_top_dist_info = |n: &str, file: &str| {
        n.split_once('/').is_some_and(|(dir, rest)| dir.ends_with(".dist-info") && rest == file)
    };
    let Some(wheel_name) = names.iter().find(|n| is_top_dist_info(n, "WHEEL")).cloned() else {
        bail_loc!("{pure} has no .dist-info/WHEEL");
    };
    let record_name = format!("{}/RECORD", wheel_name.trim_end_matches("/WHEEL"));

    // WHEEL: not pure any more, and the toolchain's tags.
    let mut wheel_text = String::new();
    zin.by_name(&wheel_name)?.read_to_string(&mut wheel_text)?;
    if !wheel_text.lines().any(|l| l.trim() == "Root-Is-Purelib: true") {
        bail_loc!(
            "{pure} is not a pure-Python wheel. `py_wheel` adds the native files itself: \
             the project's build backend must not (remove any build hook that does)."
        );
    }
    let mut new_wheel = String::new();
    for line in wheel_text.lines() {
        if line.starts_with("Tag:") {
            continue;
        }
        let line = if line.trim() == "Root-Is-Purelib: true" { "Root-Is-Purelib: false" } else { line };
        new_wheel.push_str(line);
        new_wheel.push('\n');
    }
    for py in py_tags.split('.') {
        for abi in abi_tags.split('.') {
            for platform in platform_tags.split('.') {
                new_wheel.push_str(&format!("Tag: {py}-{abi}-{platform}\n"));
            }
        }
    }

    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {out_dir}"))?;
    let tmp_path = out_dir.join(format!(".{}.tmp", out_path.file_name().unwrap_or_default()));
    let mut out = zip::ZipWriter::new(std::fs::File::create(&tmp_path).with_context(|| format!("creating {tmp_path}"))?);
    let mut record: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = Default::default();

    // The pure wheel's entries, as they are.
    for i in 0..zin.len() {
        let name = zin.by_index_raw(i)?.name().to_owned();
        if name == wheel_name || name == record_name || name.ends_with('/') {
            continue;
        }
        let mut hasher = HashingWriter::<std::io::Sink>::new(None);
        std::io::copy(&mut zin.by_index(i)?, &mut hasher)?;
        record.push(hasher.record_line(&name));
        seen.insert(name);
        out.raw_copy_file(zin.by_index_raw(i)?)?;
    }

    // The native files, in a fixed order.
    let mut native: Vec<(String, &Utf8PathBuf)> =
        files.iter().map(|(src, dest)| (format!("{native_dir}/{dest}"), src)).collect();
    native.sort();
    for (name, src) in native {
        if !seen.insert(name.clone()) {
            bail_loc!("{name} is both in the Python package and a native file of the py_package");
        }
        let mut file = std::fs::File::open(src).with_context(|| format!("opening {src}"))?;
        let meta = file.metadata()?;
        let options = entry_options()
            .unix_permissions(if is_executable(&meta) { 0o755 } else { 0o644 })
            .large_file(meta.len() >= u32::MAX as u64);
        out.start_file(&name, options)?;
        let mut hasher = HashingWriter::new(Some(&mut out));
        std::io::copy(&mut file, &mut hasher).with_context(|| format!("adding {src}"))?;
        record.push(hasher.record_line(&name));
    }

    out.start_file(&wheel_name, entry_options().unix_permissions(0o644))?;
    let mut hasher = HashingWriter::new(Some(&mut out));
    hasher.write_all(new_wheel.as_bytes())?;
    record.push(hasher.record_line(&wheel_name));

    record.push(format!("{},,", csv_field(&record_name)));
    out.start_file(&record_name, entry_options().unix_permissions(0o644))?;
    out.write_all((record.join("\n") + "\n").as_bytes())?;
    out.finish()?;

    if out_path.exists() {
        std::fs::remove_file(&out_path).with_context(|| format!("replacing {out_path}"))?;
    }
    std::fs::rename(&tmp_path, &out_path).with_context(|| format!("writing {out_path}"))?;
    Ok(out_path)
}

/// Deflate, and a fixed date: the same inputs give the same wheel.
fn entry_options() -> zip::write::FileOptions {
    zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Counts and hashes what passes through (to `inner`, if any), for a
/// `RECORD` line.
struct HashingWriter<'a, W: Write> {
    inner: Option<&'a mut W>,
    hasher: Sha256,
    len: u64,
}

impl<'a, W: Write> HashingWriter<'a, W> {
    fn new(inner: Option<&'a mut W>) -> Self {
        Self { inner, hasher: Sha256::new(), len: 0 }
    }
}

impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = match &mut self.inner {
            Some(w) => w.write(buf)?,
            None => buf.len(),
        };
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<W: Write> HashingWriter<'_, W> {
    /// `path,sha256=<urlsafe base64, no padding>,size` (PEP 376 / PEP 427).
    fn record_line(self, name: &str) -> String {
        format!("{},sha256={},{}", csv_field(name), base64_url(&self.hasher.finalize()), self.len)
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| (n << 8) | *b as u32) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_zip(path: &Utf8Path, entries: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, text) in entries {
            zip.start_file(*name, entry_options()).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn read_entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> String {
        let mut s = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn base64_url_matches_known_values() {
        assert_eq!(base64_url(b""), "");
        assert_eq!(base64_url(b"f"), "Zg");
        assert_eq!(base64_url(b"fo"), "Zm8");
        assert_eq!(base64_url(b"foo"), "Zm9v");
        assert_eq!(base64_url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn adds_native_files_and_retags() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let pure = dir.join("demo_pkg-1.2-py3-none-any.whl");
        write_zip(
            &pure,
            &[
                ("demo_pkg/__init__.py", "x = 1\n"),
                ("demo_pkg-1.2.dist-info/METADATA", "Name: demo-pkg\n"),
                ("demo_pkg-1.2.dist-info/WHEEL", "Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n"),
                ("demo_pkg-1.2.dist-info/RECORD", "old\n"),
            ],
        );
        let native = dir.join("demo.so");
        std::fs::write(&native, "abc").unwrap();
        let files = vec![(native, "demo.so".to_owned())];

        let out = add_native_files(&pure, "demo_pkg/_native", &files, "cp312-cp312-linux_x86_64.other", &dir.join("out")).unwrap();
        assert_eq!(out.file_name(), Some("demo_pkg-1.2-cp312-cp312-linux_x86_64.other.whl"));

        let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(read_entry(&mut zip, "demo_pkg/__init__.py"), "x = 1\n");
        assert_eq!(read_entry(&mut zip, "demo_pkg/_native/demo.so"), "abc");
        assert_eq!(
            read_entry(&mut zip, "demo_pkg-1.2.dist-info/WHEEL"),
            "Wheel-Version: 1.0\nRoot-Is-Purelib: false\nTag: cp312-cp312-linux_x86_64\nTag: cp312-cp312-other\n"
        );
        let record = read_entry(&mut zip, "demo_pkg-1.2.dist-info/RECORD");
        // sha256("abc"), as `python -c "import hashlib,base64; ..."` prints it.
        assert!(record.contains("demo_pkg/_native/demo.so,sha256=ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0,3\n"), "{record}");
        assert!(record.contains("demo_pkg/__init__.py,sha256="));
        assert!(record.ends_with("demo_pkg-1.2.dist-info/RECORD,,\n"));
        assert_eq!(record.lines().count(), 5);
        // RECORD is the last entry.
        assert_eq!(zip.by_index(zip.len() - 1).unwrap().name(), "demo_pkg-1.2.dist-info/RECORD");
    }

    #[test]
    fn refuses_a_wheel_that_is_not_pure_and_a_name_clash() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let native = dir.join("demo.so");
        std::fs::write(&native, "abc").unwrap();

        let platform = dir.join("a-1-cp312-cp312-linux_x86_64.whl");
        write_zip(&platform, &[("a-1.dist-info/WHEEL", "Root-Is-Purelib: false\nTag: cp312-cp312-linux_x86_64\n")]);
        let err = add_native_files(&platform, "a/_native", &[], "cp312-cp312-linux_x86_64", dir).unwrap_err();
        assert!(err.to_string().contains("not a pure-Python wheel"), "{err}");

        let pure = dir.join("a-1-py3-none-any.whl");
        write_zip(
            &pure,
            &[("a/_native/demo.so", "py"), ("a-1.dist-info/WHEEL", "Root-Is-Purelib: true\nTag: py3-none-any\n")],
        );
        let files = vec![(native, "demo.so".to_owned())];
        let err = add_native_files(&pure, "a/_native", &files, "cp312-cp312-linux_x86_64", &dir.join("out")).unwrap_err();
        assert!(err.to_string().contains("both in the Python package"), "{err}");
    }
}
