//! Archive extraction: zip, tar.xz, tar.gz, vsix (zip with `Contents/`
//! prefix), and msi.
//!
//! MSI extraction is pure-Rust (cross-platform) via the [`msi`] + [`cab`]
//! crates — implementation in [`crate::extract_msi`], re-exported here as
//! [`extract_msi`] for call-site symmetry with the other archive
//! extractors.
//!
//! Zip-related code uses `zip::ZipFile::enclosed_name()` to defend
//! against zip-slip (entries like `../../../etc/passwd`).

use anyhow::{Context, Result, anyhow, bail};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::ArchiveKind;

pub use crate::extract_msi::{extract_msi, extract_msis_in_parallel};

/// Extract the archive at `archive` into `dest`. If `strip_prefix` is set
/// and an entry's path starts with it, that prefix is removed before
/// placement (used for archives that nest everything in a top-level dir
/// like `clang+llvm-21.1.6-.../bin/clang.exe`).
pub fn extract_archive(
    archive: &Path,
    kind: ArchiveKind,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating dest {}", dest.display()))?;
    match kind {
        ArchiveKind::Zip => extract_zip(archive, dest, strip_prefix),
        ArchiveKind::TarXz => extract_tar_xz(archive, dest, strip_prefix),
        ArchiveKind::TarGz => extract_tar_gz(archive, dest, strip_prefix),
    }
}

fn extract_zip(archive: &Path, dest: &Path, strip_prefix: Option<&str>) -> Result<()> {
    tracing::debug!(
        "extracting zip {} -> {}",
        archive.display(),
        dest.display()
    );
    let f = File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(f)
        .with_context(|| format!("reading zip {}", archive.display()))?;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let raw = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => {
                bail!(
                    "zip entry has invalid path (zip-slip?): {}",
                    entry.name()
                );
            }
        };
        let rel = match apply_strip_prefix(&raw, strip_prefix) {
            Some(r) => r,
            None => continue, // entry lives outside strip_prefix; skip
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)
                .with_context(|| format!("mkdir {}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mode = entry.unix_mode();
        let mut outf = File::create(&out)
            .with_context(|| format!("creating {}", out.display()))?;
        std::io::copy(&mut entry, &mut outf)
            .with_context(|| format!("writing {}", out.display()))?;
        drop(outf);
        apply_mode(&out, mode)?;
    }
    Ok(())
}

/// Apply an archive entry's Unix permission bits to an extracted file.
///
/// Only the low `0o777` bits are kept, and the owner always keeps read and
/// write so later stages (CAS hashing, staging cleanup) can work on the file.
/// On Windows this is a no-op: there is no executable bit, and read-only is
/// applied later by the CAS.
#[cfg(unix)]
pub(crate) fn apply_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let Some(mode) = mode else { return Ok(()) };
    let bits = (mode & 0o777) | 0o600;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
        .with_context(|| format!("chmod {:o} {}", bits, path.display()))
}

#[cfg(not(unix))]
pub(crate) fn apply_mode(_path: &Path, _mode: Option<u32>) -> Result<()> {
    Ok(())
}

/// Reject a tar or zip entry path that is absolute or climbs with `..`.
fn check_entry_path(raw_path: &Path) -> Result<()> {
    if raw_path.is_absolute()
        || raw_path.has_root()
        || raw_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::Prefix(_)))
    {
        bail!("archive entry has unsafe path (tar-slip?): {}", raw_path.display());
    }
    Ok(())
}

/// Lexically resolve `target` (a symlink's contents) against the directory
/// holding the link, `link_parent_rel` (relative to the extraction root),
/// and return the result relative to the root, or `None` if it escapes.
fn resolve_within_root(link_parent_rel: &Path, target: &Path) -> Option<PathBuf> {
    if target.is_absolute() || target.has_root() {
        return None;
    }
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for c in link_parent_rel.components().chain(target.components()) {
        match c {
            std::path::Component::Normal(n) => parts.push(n.to_os_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(parts.iter().collect())
}

/// True if any proper ancestor of `rel` is one of the archive's symlinks.
/// Writing through such a path could land outside the extraction root, so
/// it is refused.
fn passes_through_symlink(rel: &Path, symlinks: &std::collections::HashSet<PathBuf>) -> bool {
    rel.ancestors()
        .skip(1)
        .filter(|a| !a.as_os_str().is_empty())
        .any(|a| symlinks.contains(a))
}

fn extract_tar_xz(archive: &Path, dest: &Path, strip_prefix: Option<&str>) -> Result<()> {
    tracing::debug!(
        "extracting tar.xz {} -> {}",
        archive.display(),
        dest.display()
    );
    let f = File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let dec = mt_xz_decoder(f)?;
    extract_tar_inner(dec, dest, strip_prefix)
}

/// Build a multithreaded .xz decoder over `reader`. xz files written by `xz`
/// (e.g. LLVM's release tarballs) are split into independent ~64 MiB blocks,
/// so liblzma can decode them across threads. Falls back to a single-threaded
/// stream decoder (the same flags=0 decoder the old `xz2` path used) if the MT
/// decoder can't be initialized.
fn mt_xz_decoder<R: std::io::Read>(reader: R) -> Result<liblzma::read::XzDecoder<R>> {
    let threads = crate::fs_util::worker_count(usize::MAX) as u32;
    if threads <= 1 {
        return Ok(liblzma::read::XzDecoder::new(reader));
    }
    // memlimit_stop = MAX disables the hard decode limit; memlimit_threading is
    // the soft budget below which liblzma keeps blocks single-threaded, so it
    // must comfortably hold several in-flight 64 MiB blocks per worker.
    let threading_budget = (threads as u64 + 1) * 256 * 1024 * 1024;
    match liblzma::stream::MtStreamBuilder::new()
        .threads(threads)
        .memlimit_stop(u64::MAX)
        .memlimit_threading(threading_budget)
        .timeout_ms(0)
        .decoder()
    {
        Ok(stream) => Ok(liblzma::read::XzDecoder::new_stream(reader, stream)),
        Err(err) => {
            tracing::warn!("MT xz decoder init failed ({err}); falling back to single-threaded");
            Ok(liblzma::read::XzDecoder::new(reader))
        }
    }
}

fn extract_tar_gz(archive: &Path, dest: &Path, strip_prefix: Option<&str>) -> Result<()> {
    tracing::debug!(
        "extracting tar.gz {} -> {}",
        archive.display(),
        dest.display()
    );
    let f = File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let dec = flate2::read::GzDecoder::new(std::io::BufReader::new(f));
    extract_tar_inner(dec, dest, strip_prefix)
}

/// Files at or above this size are written inline on the reader thread
/// (streamed, never buffered whole); smaller files are handed to the writer
/// pool as an in-memory blob. This bounds the writer pool's memory to
/// roughly `channel_capacity * THRESHOLD` while still parallelizing the many
/// small-file creates — the per-file syscall + AV-scan cost that dominates on
/// Windows.
const TAR_INLINE_WRITE_THRESHOLD: u64 = 1 << 20; // 1 MiB

fn extract_tar_inner<R: Read>(
    reader: R,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_overwrite(true);
    let mut symlinks: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut hard_links: Vec<(PathBuf, PathBuf)> = Vec::new();

    let workers = crate::fs_util::worker_count(usize::MAX);
    // tar is a sequential stream, so a single reader thread parses entries and
    // either writes large files inline or dispatches small ones to the pool.
    let (tx, rx) = std::sync::mpsc::sync_channel::<(PathBuf, Vec<u8>, Option<u32>)>(workers * 4);
    let rx = std::sync::Mutex::new(rx);
    let first_err: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);

    std::thread::scope(|s| -> Result<()> {
        for _ in 0..workers {
            let rx = &rx;
            let first_err = &first_err;
            s.spawn(move || loop {
                // Hold the lock only for the dequeue; the write happens after
                // releasing it so workers write in parallel.
                let job = {
                    let guard = rx.lock().unwrap();
                    guard.recv()
                };
                let (path, bytes, mode) = match job {
                    Ok(j) => j,
                    Err(_) => return, // channel closed: no more work
                };
                if let Err(e) = write_file_bytes(&path, &bytes, mode) {
                    let mut g = first_err.lock().unwrap();
                    if g.is_none() {
                        *g = Some(e);
                    }
                }
            });
        }

        let mut produce = || -> Result<()> {
            for entry in archive.entries()? {
                let mut entry = entry?;
                let raw_path = entry.path()?.to_path_buf();
                check_entry_path(&raw_path)?;
                let rel = match apply_strip_prefix(&raw_path, strip_prefix) {
                    Some(r) => r,
                    None => continue,
                };
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let out = dest.join(&rel);
                let kind = entry.header().entry_type();
                if kind.is_dir() {
                    std::fs::create_dir_all(&out)?;
                    continue;
                }
                if kind.is_symlink() || kind.is_hard_link() {
                    // Links are created after every regular file is on disk:
                    // a hard link needs its target to exist, and a symlink
                    // created early could redirect a later entry's write
                    // outside `dest`.
                    let target = entry
                        .link_name()?
                        .ok_or_else(|| anyhow!("link entry without link_name: {}", raw_path.display()))?
                        .into_owned();
                    if kind.is_symlink() {
                        symlinks.push((rel, target));
                    } else {
                        hard_links.push((rel, target));
                    }
                    continue;
                }
                let mode = entry.header().mode().ok();
                let size = entry.header().size().unwrap_or(0);
                if size >= TAR_INLINE_WRITE_THRESHOLD {
                    if let Some(parent) = out.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let mut outf = File::create(&out)
                        .with_context(|| format!("creating {}", out.display()))?;
                    std::io::copy(&mut entry, &mut outf)
                        .with_context(|| format!("writing {}", out.display()))?;
                    drop(outf);
                    apply_mode(&out, mode)?;
                } else {
                    let mut buf = Vec::with_capacity(size as usize);
                    entry
                        .read_to_end(&mut buf)
                        .with_context(|| format!("reading {}", out.display()))?;
                    // A send error means all workers died; surface their error.
                    if tx.send((out, buf, mode)).is_err() {
                        break;
                    }
                }
                // Bail early if a worker has already failed.
                if first_err.lock().unwrap().is_some() {
                    break;
                }
            }
            Ok(())
        };

        let result = produce();
        drop(tx); // close the channel so workers drain and exit
        result
    })?;

    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e);
    }

    // Hard links: the archive names an earlier entry (by its archive path)
    // whose bytes this entry shares. Copy rather than link so the staging
    // tree holds independent files; the CAS dedupes them afterwards anyway.
    let symlink_set: std::collections::HashSet<PathBuf> =
        symlinks.iter().map(|(rel, _)| rel.clone()).collect();
    for (rel, target) in &hard_links {
        check_entry_path(target)?;
        let Some(target_rel) = apply_strip_prefix(target, strip_prefix) else {
            bail!(
                "hard link {} points outside the extracted prefix: {}",
                rel.display(),
                target.display()
            );
        };
        if passes_through_symlink(rel, &symlink_set)
            || passes_through_symlink(&target_rel, &symlink_set)
            || symlink_set.contains(&target_rel)
        {
            bail!("hard link {} goes through a symlink; refusing", rel.display());
        }
        let out = dest.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let src = dest.join(&target_rel);
        std::fs::copy(&src, &out).with_context(|| {
            format!("hard link {} -> {}", out.display(), src.display())
        })?;
    }

    // Symlinks last, and only ones that stay inside `dest`. Each link's
    // target is resolved lexically from the link's own directory, and no
    // link may sit beneath another link (which would make the lexical
    // check meaningless).
    for (rel, target) in &symlinks {
        if passes_through_symlink(rel, &symlink_set) {
            bail!("symlink {} sits inside another symlink; refusing", rel.display());
        }
        let parent_rel = rel.parent().unwrap_or(Path::new(""));
        if resolve_within_root(parent_rel, target).is_none() {
            bail!(
                "symlink {} -> {} escapes the extraction directory; refusing",
                rel.display(),
                target.display()
            );
        }
        let out = dest.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::fs_util::symlink_any(target, &out)?;
    }
    Ok(())
}

/// Write `bytes` to `path`, creating parent directories. Used by the tar
/// writer pool. `create_dir_all` is race-tolerant across workers.
fn write_file_bytes(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    apply_mode(path, mode)
}

/// Strip the leading `prefix` (a path component, may be multi-component) from
/// `path`. Returns `None` if the path doesn't start with the prefix (such
/// entries are dropped during extraction).
fn apply_strip_prefix(path: &Path, prefix: Option<&str>) -> Option<PathBuf> {
    let Some(prefix) = prefix else {
        return Some(path.to_path_buf());
    };
    let prefix = Path::new(prefix);
    match path.strip_prefix(prefix) {
        Ok(rest) => Some(rest.to_path_buf()),
        Err(_) => None,
    }
}

/// VSIX is a zip whose payload lives under a `Contents/` prefix. Anything
/// outside `Contents/` is metadata we don't want.
pub fn extract_vsix(archive: &Path, dest: &Path) -> Result<()> {
    tracing::debug!("extracting vsix {} -> {}", archive.display(), dest.display());
    std::fs::create_dir_all(dest)?;
    let f = File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(f)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        let Some(rel) = name.strip_prefix("Contents/") else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        // Re-use enclosed_name on a synthetic path to defend against slip.
        let rel_path = Path::new(rel);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("vsix entry has unsafe path: {name}");
        }
        let out = dest.join(rel_path);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut outf = File::create(&out)?;
        std::io::copy(&mut entry, &mut outf)?;
    }
    Ok(())
}

/// Extract one MSVC / Windows SDK payload, dispatching on filename extension:
/// `.vsix` / `.zip` via the VSIX `Contents/` extractor, `.msi` via the
/// pure-Rust MSI extractor. Unknown extensions are logged and skipped.
pub fn extract_msvc_payload(archive: &Path, filename: &str, dest: &Path) -> Result<()> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".vsix") || lower.ends_with(".zip") {
        extract_vsix(archive, dest)?;
    } else if lower.ends_with(".msi") {
        extract_msi(archive, dest)?;
    } else {
        tracing::warn!("skipping payload with unknown extension: {filename}");
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/extract.rs"]
mod tests;
