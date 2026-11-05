//! Low-level filesystem helpers: read-only marking, atomic rename, directory
//! symlinks/junctions on Windows, etc.

use anyhow::{Context, Result};
use std::path::Path;

/// Mark a single regular file as read-only.
pub fn mark_file_readonly(path: &Path) -> Result<()> {
    let md = std::fs::metadata(path)?;
    let mut perms = md.permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// Clear the read-only attribute on a single file. Used when we need to
/// remove a file that was previously marked read-only (Windows refuses to
/// delete read-only files).
pub fn clear_readonly(path: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    let mut perms = md.permissions();
    if perms.readonly() {
        #[cfg(unix)]
        {
            // `set_readonly(false)` on Unix sets every write bit (0666 or
            // 0777). Only give the owner write access back.
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(perms.mode() | 0o200);
        }
        #[cfg(not(unix))]
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Recursively delete a directory tree, clearing read-only attrs as needed.
/// On Windows, plain `remove_dir_all` fails on read-only files; we walk and
/// clear first.
pub fn remove_dir_all_writable(path: &Path) -> Result<()> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    // A symlink or junction (a symlink-deployed toolchain): remove the link
    // itself. Walking it would follow the root link into the shared install
    // and strip read-only protection from every CAS blob behind it.
    if md.file_type().is_symlink() || is_junction(path) {
        return remove_link(path);
    }
    remove_real_dir_all(path)
}

/// On Unix, deleting a file needs write access to its directory, not to the
/// file, so read-only files are removed as they are. Clearing the bit would
/// change the shared inode of every hardlinked CAS blob.
#[cfg(unix)]
fn remove_real_dir_all(path: &Path) -> Result<()> {
    make_dirs_writable(path);
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

/// Windows refuses to delete read-only files, so clear the attribute first.
/// Note: on a hardlinked CAS blob this attribute is per-inode, so the blob
/// loses read-only protection until it is next re-marked.
#[cfg(not(unix))]
fn remove_real_dir_all(path: &Path) -> Result<()> {
    for entry in jwalk::WalkDir::new(path).skip_hidden(false).follow_links(false) {
        if let Ok(e) = entry {
            if e.file_type().is_file() {
                let _ = clear_readonly(&e.path());
            }
        }
    }
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

/// Give the owner write access to every directory under `root` (not
/// files), so `remove_dir_all` can unlink their entries on Unix.
#[cfg(unix)]
fn make_dirs_writable(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for entry in jwalk::WalkDir::new(root).skip_hidden(false).follow_links(false) {
        if let Ok(e) = entry {
            if e.file_type().is_dir() {
                if let Ok(md) = std::fs::symlink_metadata(e.path()) {
                    let mode = md.permissions().mode();
                    if mode & 0o700 != 0o700 {
                        let _ = std::fs::set_permissions(
                            e.path(),
                            std::fs::Permissions::from_mode(mode | 0o700),
                        );
                    }
                }
            }
        }
    }
}

/// Remove a symlink or junction without touching what it points at.
fn remove_link(path: &Path) -> Result<()> {
    // A directory symlink or junction on Windows must be removed with
    // `remove_dir`; a file symlink (and any symlink on Unix) with
    // `remove_file`.
    std::fs::remove_file(path)
        .or_else(|_| std::fs::remove_dir(path))
        .with_context(|| format!("removing link {}", path.display()))
}

#[cfg(windows)]
fn is_junction(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(path)
        .map(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_junction(_path: &Path) -> bool {
    false
}

/// Atomically write `bytes` to `path` via temp-then-rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent dir for {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    let tmp = tempfile::Builder::new()
        .prefix(".natron-tmp-")
        .tempfile_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    std::fs::write(tmp.path(), bytes)
        .with_context(|| format!("writing tempfile {}", tmp.path().display()))?;
    let tmp_path = tmp.into_temp_path();
    // Use persist to atomic-rename. On Windows, rename across volumes fails;
    // we only ever write within `parent`, so this is fine.
    tmp_path
        .persist(path)
        .map_err(|e| anyhow::anyhow!("persisting to {}: {e}", path.display()))?;
    Ok(())
}

/// Try to atomically rename `from` to `to`. Returns `Ok(true)` on success,
/// `Ok(false)` if the target already exists (peer beat us / collision case),
/// and `Err` on other failures.
///
/// POSIX `rename` of a directory onto a non-empty existing directory fails
/// with `ENOTEMPTY`. Windows `rename` always fails when target exists. Both
/// yield the desired "loser drops" outcome.
pub fn try_rename(from: &Path, to: &Path) -> Result<bool> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(true),
        Err(err) => {
            // If the target now exists (peer beat us, or it was always
            // there), treat as collision rather than error.
            if to.exists() {
                Ok(false)
            } else {
                Err(anyhow::anyhow!(
                    "rename {} -> {}: {err}",
                    from.display(),
                    to.display()
                ))
            }
        }
    }
}

/// Create a hardlink. Retries a few times on transient Access-Denied errors,
/// which can surface on Windows when another thread/process is holding the
/// source file open (e.g. via `SetFileAttributesW`) at the same instant.
pub fn hard_link(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    const MAX_ATTEMPTS: u32 = 6;
    let mut delay_ms = 5u64;
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match std::fs::hard_link(src, dst) {
            Ok(()) => return Ok(()),
            Err(err) => {
                let kind = err.kind();
                let raw = err.raw_os_error();
                // Retry on Access Denied / Sharing Violation (transient on
                // Windows under contention with concurrent attribute writes
                // on the source). On other kinds of error, fail fast.
                let transient = kind == std::io::ErrorKind::PermissionDenied
                    || raw == Some(5)   // ERROR_ACCESS_DENIED
                    || raw == Some(32); // ERROR_SHARING_VIOLATION
                if !transient || attempt + 1 == MAX_ATTEMPTS {
                    last_err = Some(err);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 2).min(80);
            }
        }
    }
    Err(anyhow::anyhow!(
        "hardlink {} -> {}: {}",
        src.display(),
        dst.display(),
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    ))
}

/// Create a directory symlink. On Windows, falls back to a junction if
/// symlink creation lacks privilege.
pub fn dir_symlink(target: &Path, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::symlink_dir;
        match symlink_dir(target, link) {
            Ok(()) => return Ok(()),
            Err(err) => {
                // ERROR_PRIVILEGE_NOT_HELD = 1314
                let raw = err.raw_os_error();
                if raw == Some(1314) {
                    return create_junction(target, link);
                }
                return Err(anyhow::anyhow!(
                    "symlink {} -> {}: {err}",
                    link.display(),
                    target.display()
                ));
            }
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).with_context(|| {
            format!("symlink {} -> {}", link.display(), target.display())
        })?;
        Ok(())
    }
}

/// Create a Windows directory junction. Junctions don't require any
/// privilege but are local-volume-only.
#[cfg(windows)]
fn create_junction(target: &Path, link: &Path) -> Result<()> {
    // Use mklink /J via cmd.exe. The Win32 API for junctions is fiddly
    // (DeviceIoControl with REPARSE_DATA_BUFFER); shelling out is the
    // pragmatic choice and matches what most Rust toolchain managers do.
    let target_abs = std::fs::canonicalize(target).with_context(|| {
        format!("canonicalizing junction target {}", target.display())
    })?;
    // Strip the \\?\ prefix that canonicalize adds; mklink rejects it.
    let target_str = target_abs.to_string_lossy();
    let target_clean = target_str
        .strip_prefix(r"\\?\")
        .unwrap_or(&target_str);

    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target_clean)
        .status()
        .with_context(|| "spawning cmd for mklink /J")?;
    if !status.success() {
        anyhow::bail!(
            "mklink /J {} {} failed with {}",
            link.display(),
            target_clean,
            status
        );
    }
    Ok(())
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn create_junction(_target: &Path, _link: &Path) -> Result<()> {
    anyhow::bail!("junctions are Windows-only");
}

/// Create a symlink at `link` pointing to `target`, reproducing a symlink from
/// an archive or another tree. Creates parent dirs and replaces any existing
/// entry at `link`. On Windows we don't know if the target is a file or a dir,
/// so we try a file symlink then fall back to a dir symlink. A failure (e.g.
/// the symlink-create privilege is not held on Windows) is returned as an
/// error — a tree missing a symlink is an incomplete, invalid install, not
/// something to silently continue past.
pub fn symlink_any(target: &Path, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(link); // tolerate a prior partial run
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).with_context(|| {
            format!("symlink {} -> {}", link.display(), target.display())
        })?;
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
            .or_else(|_| std::os::windows::fs::symlink_dir(target, link))
            .with_context(|| {
                format!("symlink {} -> {}", link.display(), target.display())
            })?;
    }
    Ok(())
}

/// Reproduce the symlink at `src` as a new symlink at `dst` (reads `src`'s
/// target via `read_link`, then recreates it at `dst`).
pub fn reproduce_symlink(src: &Path, dst: &Path) -> Result<()> {
    let target = std::fs::read_link(src)
        .with_context(|| format!("read_link {}", src.display()))?;
    symlink_any(&target, dst)
}

/// Return true if `link` is a symlink (or junction on Windows) that resolves
/// to `expected_target`. Returns false on any kind of mismatch or error.
pub fn symlink_points_to(link: &Path, expected_target: &Path) -> bool {
    let Ok(actual) = std::fs::read_link(link) else {
        return false;
    };
    let canon_actual = std::fs::canonicalize(&actual).unwrap_or(actual);
    let canon_expected = std::fs::canonicalize(expected_target)
        .unwrap_or_else(|_| expected_target.to_path_buf());
    canon_actual == canon_expected
}

/// Walk a directory and return the most-recent mtime found across any file
/// (recursively). Used for stale-staging GC: we want to know whether anyone
/// has touched anything inside the dir recently, not just the dir's own mtime.
pub fn latest_inside_mtime(dir: &Path) -> Result<std::time::SystemTime> {
    let mut latest = std::fs::metadata(dir)
        .with_context(|| format!("stat {}", dir.display()))?
        .modified()?;
    for entry in jwalk::WalkDir::new(dir)
        .skip_hidden(false)
        .follow_links(false)
    {
        let Ok(e) = entry else { continue };
        let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
        let Ok(m) = md.modified() else { continue };
        if m > latest {
            latest = m;
        }
    }
    Ok(latest)
}

/// Worker-pool size for a batch of `items` units of work: the machine's
/// parallelism, capped at 16 and never more than the number of items. Always
/// returns at least 1 (even for an empty batch), so a caller that sizes both
/// its worker loop and a bounded channel from this value can't accidentally
/// spawn zero workers and deadlock its producer. Centralizes the sizing used
/// by the CAS pass and the archive extractors so they scale with the host.
pub fn worker_count(items: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cores.clamp(1, 16).min(items.max(1))
}

/// Return forward-slash version of a path (best-effort UTF-8). Used when
/// serializing paths into TOML / JSON state files.
pub fn slash_str(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
#[cfg(test)]
#[path = "tests/fs_util.rs"]
mod tests;
