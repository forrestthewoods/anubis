//! `sync_dir`: make a directory hold a wanted list of files.
//!
//! Rules come in two kinds. An *action* makes new bytes (compile, link,
//! archive) and has an action cache entry. A *placement* only puts bytes
//! that already exist at a path: a runtime library next to an executable,
//! the files of an app bundle. Every byte a placement writes is an input
//! file's, or a pure function of the rule's own fields (a generated
//! `Info.plist`), and it runs no tool, so it has nothing to cache. What it
//! owes instead is to be idempotent: run twice, the second run writes
//! nothing.
//!
//! `sync_dir` is that for a whole directory. It keeps no stamp and no record
//! of earlier runs: each run compares the wanted list with the disk (one
//! stat per file, digests from the stat cache) and touches only what
//! differs, so a file edited or deleted inside the output is put back.

use crate::build_cache::{self, BuildCache, Digest};
use crate::util::SlashFix;
use crate::{bail_loc, function_name};
use anyhow::Context;
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use std::collections::{HashMap, HashSet};

/// Where a placed file's content comes from.
#[derive(Clone, Debug)]
pub enum Source {
    /// A build output: hard link, or a copy where linking fails.
    Link(Utf8PathBuf),
    /// A source file: always a copy, so an edit inside the output can't
    /// change the source.
    Copy(Utf8PathBuf),
    /// Content generated in memory.
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// Destination, relative to the synced directory.
    pub dst: Utf8PathBuf,
    pub source: Source,
}

/// A file `sync_dir` put (or found) in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub path: Utf8PathBuf,
    pub digest: Digest,
    pub executable: bool,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    /// One per entry, in entry order.
    pub files: Vec<Placed>,
    pub written: usize,
    pub removed: usize,
}

/// Make `root` hold `entries`.
///
/// With `owned`, `root` belongs to the calling rule alone: every file not in
/// `entries` is deleted. `root` must then be under `<workspace>/.anubis-out`.
/// Without it, other files in `root` are left alone.
///
/// `cache` supplies stat-cached digests; without one (`--no-cache`) files
/// are read to compare them.
pub fn sync_dir(
    workspace_root: &Utf8Path,
    cache: Option<&BuildCache>,
    root: &Utf8Path,
    entries: &[Entry],
    owned: bool,
) -> anyhow::Result<SyncReport> {
    let wanted = check_entries(root, entries)?;

    // Know every source before changing anything on disk.
    let mut sources = Vec::with_capacity(entries.len());
    for e in entries {
        sources.push(SourceInfo::of(cache, &e.source)?);
    }

    let mut report = SyncReport::default();
    if owned {
        check_owned_root(workspace_root, root)?;
        if std::fs::symlink_metadata(root).is_ok() {
            sweep(root, Utf8Path::new(""), &wanted, &mut report.removed)?;
        }
    }

    for (e, info) in entries.iter().zip(sources) {
        let dst = root.join(&e.dst).slash_fix();
        if !in_place(cache, &dst, &e.source, &info)? {
            write(workspace_root, cache, &dst, &e.source, &info)?;
            report.written += 1;
        }
        report.files.push(Placed { path: dst, digest: info.digest, executable: info.executable });
    }
    Ok(report)
}

/// Destinations must be plain relative paths, unique ignoring ASCII case
/// (so a list valid on Linux is valid on Windows and macOS), and no
/// destination may be a directory another one needs. Returns the wanted
/// paths, `/`-separated.
fn check_entries(root: &Utf8Path, entries: &[Entry]) -> anyhow::Result<HashSet<String>> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut wanted = HashSet::new();
    for (i, e) in entries.iter().enumerate() {
        let rel = e.dst.as_str().replace('\\', "/");
        let plain = !rel.is_empty() && Utf8Path::new(&rel).components().all(|c| matches!(c, Utf8Component::Normal(_)));
        if !plain {
            bail_loc!("[{}] is not a plain relative path (placing files in {root})", e.dst);
        }
        if let Some(&j) = seen.get(&rel.to_ascii_lowercase()) {
            bail_loc!(
                "two files would be placed at {}: {} and {}",
                root.join(&rel),
                describe(&entries[j].source),
                describe(&e.source)
            );
        }
        seen.insert(rel.to_ascii_lowercase(), i);
        wanted.insert(rel);
    }
    for rel in &wanted {
        let mut dir = Utf8Path::new(rel).parent();
        while let Some(d) = dir.filter(|d| !d.as_str().is_empty()) {
            if seen.contains_key(&d.as_str().to_ascii_lowercase()) {
                bail_loc!("{} would be both a file and a directory (for {})", root.join(d), root.join(rel));
            }
            dir = d.parent();
        }
    }
    Ok(wanted)
}

fn describe(source: &Source) -> String {
    match source {
        Source::Link(p) | Source::Copy(p) => p.to_string(),
        Source::Bytes(_) => "generated content".to_owned(),
    }
}

/// An owned directory gets files deleted, so it must be Anubis's own: under
/// `<workspace>/.anubis-out`, both as written and after resolving symlinks.
fn check_owned_root(workspace_root: &Utf8Path, root: &Utf8Path) -> anyhow::Result<()> {
    let out = workspace_root.join(".anubis-out").slash_fix();
    let root = root.to_owned().slash_fix();
    let lexical = root.strip_prefix(&out).ok().is_some_and(|rel| {
        !rel.as_str().is_empty() && rel.components().all(|c| matches!(c, Utf8Component::Normal(_)))
    });
    if !lexical {
        bail_loc!("refusing to sync {root}: not under {out}");
    }
    // The deepest existing ancestor must resolve to a path inside the real
    // `.anubis-out`, in case a directory on the way is a symlink.
    let Ok(real_out) = out.canonicalize() else { return Ok(()) };
    let mut existing = root.as_path();
    while std::fs::symlink_metadata(existing).is_err() {
        match existing.parent() {
            Some(p) => existing = p,
            None => return Ok(()),
        }
    }
    let real = existing.canonicalize().with_context(|| format!("resolving {existing}"))?;
    if !real.starts_with(&real_out) {
        bail_loc!("refusing to sync {root}: {existing} resolves to {}, outside {out}", real.display());
    }
    Ok(())
}

/// Delete everything under `root/rel` that isn't wanted, then directories
/// left empty. Never follows a symlink: a link is removed itself. Names are
/// compared exactly, so on a case-insensitive filesystem a file whose name
/// differs only by case is removed and placed again under the wanted name.
/// Returns whether `root/rel` still has entries.
fn sweep(root: &Utf8Path, rel: &Utf8Path, wanted: &HashSet<String>, removed: &mut usize) -> anyhow::Result<bool> {
    let dir = root.join(rel);
    let mut kept = false;
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {dir}"))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            // Not UTF-8, so not something Anubis placed.
            remove_any(&entry.path(), entry.file_type()?.is_dir())?;
            *removed += 1;
            continue;
        };
        let child = if rel.as_str().is_empty() { name.to_owned() } else { format!("{rel}/{name}") };
        // `file_type` doesn't follow symlinks.
        let is_dir = entry.file_type()?.is_dir();
        if is_dir && !wanted.contains(&child) {
            if sweep(root, Utf8Path::new(&child), wanted, removed)? {
                kept = true;
            } else {
                std::fs::remove_dir(entry.path()).with_context(|| format!("removing {}", root.join(&child)))?;
            }
        } else if !is_dir && wanted.contains(&child) {
            kept = true;
        } else {
            // A file that isn't wanted, or a directory where a file is.
            remove_any(&entry.path(), is_dir)?;
            *removed += 1;
        }
    }
    Ok(kept)
}

fn remove_any(path: &std::path::Path, is_dir: bool) -> anyhow::Result<()> {
    // `remove_dir_all` doesn't follow symlinks either.
    let r = if is_dir { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    r.with_context(|| format!("removing {}", path.display()))
}

/// What an entry's destination must look like.
struct SourceInfo {
    size: u64,
    digest: Digest,
    executable: bool,
}

impl SourceInfo {
    fn of(cache: Option<&BuildCache>, source: &Source) -> anyhow::Result<SourceInfo> {
        match source {
            Source::Bytes(b) => {
                Ok(SourceInfo { size: b.len() as u64, digest: Digest::of_bytes(b), executable: false })
            }
            Source::Link(p) | Source::Copy(p) => {
                let meta = std::fs::metadata(p).with_context(|| format!("file to place: {p}"))?;
                if !meta.is_file() {
                    bail_loc!("file to place is not a file: {p}");
                }
                let Some(digest) = digest_of(cache, p)? else { bail_loc!("file to place vanished: {p}") };
                Ok(SourceInfo { size: meta.len(), digest, executable: build_cache::is_executable(&meta) })
            }
        }
    }
}

fn digest_of(cache: Option<&BuildCache>, path: &Utf8Path) -> anyhow::Result<Option<Digest>> {
    match cache {
        Some(c) => c.file_digest(path),
        None => match Digest::of_file(path) {
            Ok(d) => Ok(Some(d)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::Error::from(e).context(format!("hashing {path}"))),
        },
    }
}

/// In place: a regular file (not a symlink) with the wanted size, content
/// and executable bit. A copy must also not be a hard link to its source.
fn in_place(cache: Option<&BuildCache>, dst: &Utf8Path, source: &Source, want: &SourceInfo) -> anyhow::Result<bool> {
    let meta = match std::fs::symlink_metadata(dst) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    if !meta.is_file() || meta.len() != want.size || build_cache::is_executable(&meta) != want.executable {
        return Ok(false);
    }
    if let Source::Copy(src) = source {
        if same_inode(src, &meta) {
            return Ok(false);
        }
    }
    Ok(digest_of(cache, dst)? == Some(want.digest))
}

#[cfg(unix)]
fn same_inode(a: &Utf8Path, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(a).is_ok_and(|a| a.dev() == b.dev() && a.ino() == b.ino())
}

#[cfg(not(unix))]
fn same_inode(_a: &Utf8Path, _b: &std::fs::Metadata) -> bool {
    false
}

fn write(
    workspace_root: &Utf8Path,
    cache: Option<&BuildCache>,
    dst: &Utf8Path,
    source: &Source,
    want: &SourceInfo,
) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(dst) {
        // Only reachable in a directory that isn't owned (the sweep removed
        // it otherwise): not ours to delete.
        Ok(m) if m.is_dir() => bail_loc!("cannot place a file at {dst}: it is a directory"),
        _ => {}
    }
    let parent = crate::parent_dir!(dst)?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {parent}"))?;
    let tmp = build_cache::place_tmp(workspace_root, dst)?;
    match source {
        Source::Link(src) => {
            if std::fs::hard_link(src, &tmp).is_ok() {
                // The new link changed the inode's ctime, which the stat
                // cache compares: refresh the source's entry so the next
                // build doesn't rehash it.
                if let Some(c) = cache {
                    c.set_known_digest(src, want.digest);
                }
            } else {
                copy(src, &tmp, want.executable)?;
            }
        }
        Source::Copy(src) => copy(src, &tmp, want.executable)?,
        Source::Bytes(b) => std::fs::write(&tmp, b).with_context(|| format!("writing {tmp}"))?,
    }
    match cache {
        Some(c) => c.finish_place(&tmp, dst, want.digest)?,
        None => build_cache::replace_file(&tmp, dst)?,
    }
    Ok(())
}

/// A writable copy with the source's executable bit and modification time.
/// Keeping the time means the copy isn't "just modified" to the stat cache,
/// so the next build trusts its recorded digest instead of reading it again.
fn copy(src: &Utf8Path, tmp: &Utf8Path, executable: bool) -> anyhow::Result<()> {
    let mut from = std::fs::File::open(src).with_context(|| format!("reading {src}"))?;
    let mut to = std::fs::File::create(tmp).with_context(|| format!("creating {tmp}"))?;
    std::io::copy(&mut from, &mut to).with_context(|| format!("copying {src} to {tmp}"))?;
    if let Ok(modified) = from.metadata().and_then(|m| m.modified()) {
        let _ = to.set_modified(modified);
    }
    drop(to);
    build_cache::set_executable(tmp, executable)
}

#[cfg(test)]
#[path = "../tests/sync_dir_tests.rs"]
mod tests;
