//! `anubis wheel`: a Python wheel that carries a `py_package`'s native files.
//!
//! `anubis wheel -m //mode:win_release -t //pkg:wheel`
//!
//! 1. Build: the `py_package`, like `anubis build`.
//! 2. `uv build --wheel` in the package's `project` directory. The project is
//!    plain Python as far as its build backend knows, so this is a pure
//!    wheel (`name-1.0-py3-none-any.whl`) with all the metadata: METADATA,
//!    entry points, licenses.
//! 3. Write the final wheel: every entry of the pure one, the native files
//!    under `native_dir`, the toolchain's tag in `WHEEL` and in the file
//!    name (`name-1.0-cp312-cp312-win_amd64.whl`), and a new `RECORD`.
//!
//! Anubis reads no `pyproject.toml` and no environment variable for this:
//! the mode comes from the command line and the tag from the toolchain. A
//! wheel is a zip whose layout is fixed by PEP 427; step 3 touches two lines
//! of `WHEEL` and rewrites `RECORD` (the list of files and their SHA-256).
//!
//! Not a cached build action: uv needs the network and the user's cache to
//! get the build backend, so it runs with the user's environment, like
//! `anubis run`. The native build under it is cached as usual.

use crate::rules::py_rules::Payload;
use crate::toolchain::PyToolchain;
use crate::{bail_loc, function_name};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

/// Steps 2 and 3. `scratch` is emptied and used for the pure wheel.
pub fn build_wheel(
    payload: &Payload,
    python: &PyToolchain,
    scratch: &Utf8Path,
    out_dir: &Utf8Path,
) -> anyhow::Result<Utf8PathBuf> {
    let Some(project) = &payload.project else {
        bail_loc!("`anubis wheel` needs the py_package's `project` (the directory with its pyproject.toml)");
    };
    if !python.uv.exists() {
        bail_loc!("uv not found at {}; run `anubis install-toolchains`", python.uv);
    }
    if scratch.exists() {
        std::fs::remove_dir_all(scratch).with_context(|| format!("removing {scratch}"))?;
    }
    std::fs::create_dir_all(scratch).with_context(|| format!("creating {scratch}"))?;

    let mut uv = std::process::Command::new(python.uv.as_std_path());
    uv.current_dir(project).args(["build", "--wheel", "--out-dir", scratch.as_str()]);
    if python.interpreter.exists() {
        uv.arg("--python").arg(python.interpreter.as_std_path());
    }
    tracing::info!("uv build --wheel in {project}");
    let status = uv.status().with_context(|| format!("running {} build", python.uv))?;
    if !status.success() {
        bail_loc!("`uv build --wheel` failed in {project} ({status})");
    }

    let mut wheels = Vec::new();
    for entry in scratch.read_dir_utf8()? {
        let path = entry?.into_path();
        if path.extension() == Some("whl") {
            wheels.push(path);
        }
    }
    let [pure] = wheels.as_slice() else {
        bail_loc!("expected one wheel from `uv build --wheel` in {project}, found {}", wheels.len());
    };
    add_native_files(pure, &payload.native_dir, &payload.files, &payload.wheel_tag, out_dir)
}

/// Step 3: `pure` plus `files` (source, path inside `native_dir`), tagged
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
            "{pure} is not a pure-Python wheel. `anubis wheel` adds the native files itself: \
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
