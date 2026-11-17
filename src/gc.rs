//! `anubis gc`: delete outputs and cache entries that haven't been used for
//! a while (#235).
//!
//! A cached output is one file with several names: the blob in
//! `.anubis-cache/cas` and a hard link per place it sits in `.anubis-out`.
//! Its space comes back only when every name is gone, so both sides are
//! aged by one rule:
//!
//! - Every target's output directory holds a stamp file, [`USED_STAMP`],
//!   touched by `Anubis::out_dir` whenever a build asks for the directory.
//! - Every action cache entry's own mtime is its stamp, touched when a build
//!   reads the entry (`LocalDiskBackend::get_ac`).
//!
//! Stamps are refreshed at most once per [`STAMP_REFRESH`], so they cost a
//! build next to nothing. gc deletes the target directories and entries
//! whose stamp is older than the age limit, then every blob no remaining
//! entry names. Nothing here can make a later build wrong: a missing entry
//! or blob is a cache miss, and a missing output is rebuilt or restored.
//!
//! Blobs and outputs are never stamped themselves: they share one inode,
//! and the stat cache reads a changed mtime as changed content.

use crate::build_cache::{ActionResult, Digest, Manifest, StatCache};
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

/// Name of the stamp file in each target's output directory.
pub const USED_STAMP: &str = ".anubis-used";

/// A stamp younger than this is left alone, so a stamp is at most this much
/// older than the last use. gc adds it to the age limit.
pub const STAMP_REFRESH: Duration = Duration::from_secs(60 * 60);

/// A blob younger than this is never swept: a running build may have stored
/// it and not yet written the entry that names it.
const BLOB_GRACE: Duration = Duration::from_secs(60 * 60);

/// Age limit when none is given, and for the automatic pass.
pub const DEFAULT_MAX_AGE_DAYS: u64 = 30;

/// The automatic pass at the end of a command runs at most this often.
const AUTO_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether a stamp with this metadata is too young to refresh.
fn is_fresh(meta: &std::fs::Metadata, now: SystemTime) -> bool {
    meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age < STAMP_REFRESH)
}

fn set_mtime(path: &Utf8Path, now: SystemTime) {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(now);
    }
}

/// Mark the file `path`, already open as `file`, as used now: set its mtime
/// unless it is younger than [`STAMP_REFRESH`]. Stamps are best-effort: a
/// failure costs at worst an early gc of something in use.
pub fn touch_open(path: &Utf8Path, file: &std::fs::File) {
    let now = SystemTime::now();
    if !file.metadata().is_ok_and(|m| is_fresh(&m, now)) {
        set_mtime(path, now);
    }
}

/// Mark the stamp file `path` as used now, creating it (and its directory)
/// if it isn't there.
pub fn touch_stamp(path: &Utf8Path) {
    let now = SystemTime::now();
    match std::fs::metadata(path) {
        Ok(meta) if is_fresh(&meta, now) => {}
        Ok(_) => set_mtime(path, now),
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, b"");
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GcOptions {
    /// Delete what hasn't been used for this long (plus [`STAMP_REFRESH`]).
    pub max_age: Duration,
    /// Change nothing; report what would be deleted.
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct GcReport {
    /// Stale target directories per mode directory, for modes that have any.
    pub stale_targets: Vec<(String, usize)>,
    pub target_dirs: usize,
    pub entries: usize,
    pub blobs: usize,
    /// Bytes of the files whose last name was deleted.
    pub freed_bytes: u64,
    /// Files that couldn't be deleted (in use, on Windows).
    pub skipped: Vec<Utf8PathBuf>,
}

impl GcReport {
    pub fn removed_anything(&self) -> bool {
        self.target_dirs + self.entries + self.blobs > 0
    }

    pub fn summary(&self, dry_run: bool) -> String {
        let mut s = format!(
            "gc: {} {} target directories, {} cache entries and {} blobs, {} {}",
            if dry_run { "would remove" } else { "removed" },
            self.target_dirs,
            self.entries,
            self.blobs,
            if dry_run { "freeing" } else { "freed" },
            human_bytes(self.freed_bytes),
        );
        if !self.skipped.is_empty() {
            s.push_str(&format!("; skipped {} files that are in use, e.g. {}", self.skipped.len(), self.skipped[0]));
        }
        s
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// Identity and link count of a file, to tell when a deleted name was the
/// file's last one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct FileId(u64, u64);

#[cfg(unix)]
fn file_links(path: &Utf8Path, meta: &std::fs::Metadata) -> Option<(FileId, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((FileId(meta.dev(), meta.ino()), meta.nlink()))
}

#[cfg(windows)]
fn file_links(path: &Utf8Path, _meta: &std::fs::Metadata) -> Option<(FileId, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let file = std::fs::File::open(path).ok()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        return None;
    }
    let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    Some((FileId(info.dwVolumeSerialNumber as u64, index), info.nNumberOfLinks as u64))
}

/// Deletes files and keeps the books: bytes count as freed when the last of
/// a file's names goes. In a dry run nothing is deleted and the names that
/// would go are counted the same way.
struct Deleter {
    dry_run: bool,
    /// Names left for each file some of whose names were deleted.
    links_left: HashMap<FileId, u64>,
    freed_bytes: u64,
    skipped: Vec<Utf8PathBuf>,
}

impl Deleter {
    /// Delete one file (never a directory; a symlink is removed, not followed).
    fn remove_file(&mut self, path: &Utf8Path) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(path) else { return false };
        let links = if meta.file_type().is_file() { file_links(path, &meta) } else { None };
        if !self.dry_run {
            // A directory symlink on Windows is removed with remove_dir.
            if std::fs::remove_file(path).is_err() && !(meta.file_type().is_symlink() && std::fs::remove_dir(path).is_ok()) {
                self.skipped.push(path.to_owned());
                return false;
            }
        }
        match links {
            Some((id, nlink)) => {
                let left = self.links_left.entry(id).or_insert(nlink);
                *left = left.saturating_sub(1);
                if *left == 0 {
                    self.freed_bytes += meta.len();
                }
            }
            // Link count unknown: assume this was the only name.
            None if meta.file_type().is_file() => self.freed_bytes += meta.len(),
            None => {}
        }
        true
    }

    /// Delete a directory tree. Directories that still hold a skipped file stay.
    fn remove_tree(&mut self, dir: &Utf8Path) {
        let Ok(entries) = dir.read_dir_utf8() else { return };
        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                self.remove_tree(entry.path());
            } else {
                self.remove_file(entry.path());
            }
        }
        if !self.dry_run {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn mtime(path: &Utf8Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn subdirs(dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    let Ok(entries) = dir.read_dir_utf8() else { return vec![] };
    let mut dirs: Vec<Utf8PathBuf> =
        entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path().to_owned()).collect();
    dirs.sort();
    dirs
}

/// Files of a two-level store (`ac/<xx>/<hex>`, `cas/<xx>/<hex>`).
fn store_files(dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut files = vec![];
    for shard in subdirs(dir) {
        let Ok(entries) = shard.read_dir_utf8() else { continue };
        files.extend(entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_file())).map(|e| e.path().to_owned()));
    }
    files
}

/// Run `f` over `items` on a few threads; results in no particular order.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(items.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let out = std::sync::Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut local = vec![];
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    local.push(f(item));
                }
                out.lock().unwrap().extend(local);
            });
        }
    });
    out.into_inner().unwrap()
}

/// What an action cache entry says about blobs.
enum Entry {
    /// An action result: the blobs it names.
    Result(Vec<Digest>),
    /// A compile manifest. It names results (other entries), not blobs.
    Manifest,
    Unreadable,
}

fn read_entry(path: &Utf8Path) -> Entry {
    let Ok(bytes) = std::fs::read(path) else { return Entry::Unreadable };
    // Entries are MessagePack maps with field names, so a manifest doesn't
    // decode as a result or the other way round.
    if let Ok(result) = rmp_serde::from_slice::<ActionResult>(&bytes) {
        return Entry::Result(result.outputs.iter().filter_map(|o| Digest::from_hex(&o.digest)).collect());
    }
    if rmp_serde::from_slice::<Manifest>(&bytes).is_ok() {
        return Entry::Manifest;
    }
    Entry::Unreadable
}

/// One gc pass over `<root>/.anubis-out` and `<root>/.anubis-cache`.
/// `now` is a parameter for tests.
pub fn run(root: &Utf8Path, opts: GcOptions, now: SystemTime) -> Result<GcReport> {
    let out_root = root.join(".anubis-out");
    let cache_root = root.join(".anubis-cache");
    let is_stale = |t: SystemTime| now.duration_since(t).is_ok_and(|age| age > opts.max_age + STAMP_REFRESH);
    let mut report = GcReport::default();
    let mut del = Deleter { dry_run: opts.dry_run, links_left: HashMap::new(), freed_bytes: 0, skipped: vec![] };

    // 1. Outputs. Every directory in a mode directory is one target's
    // (`Anubis::out_dir`). One without a stamp predates stamps, or the
    // current layout: it gets a stamp now and its turn later.
    for mode_dir in subdirs(&out_root) {
        let mut stale = 0;
        for target_dir in subdirs(&mode_dir) {
            let stamp = target_dir.join(USED_STAMP);
            match mtime(&stamp) {
                None => {
                    if !opts.dry_run {
                        touch_stamp(&stamp);
                    }
                }
                // Read again just before deleting: a build may have started using it.
                Some(t) if is_stale(t) && mtime(&stamp).is_some_and(is_stale) => {
                    del.remove_tree(&target_dir);
                    stale += 1;
                }
                Some(_) => {}
            }
        }
        if stale > 0 {
            report.stale_targets.push((mode_dir.file_name().unwrap_or_default().to_owned(), stale));
            report.target_dirs += stale;
            if !opts.dry_run {
                let _ = std::fs::remove_dir(&mode_dir); // only if now empty
            }
        }
    }

    // 2. Action cache entries, by their own mtime.
    let mut live_entries = vec![];
    for entry in store_files(&cache_root.join("ac")) {
        if mtime(&entry).is_some_and(is_stale) && del.remove_file(&entry) {
            report.entries += 1;
        } else {
            live_entries.push(entry);
        }
    }

    // 3. Blobs no remaining entry names. Always done: entries are also
    // overwritten during builds, which can leave a blob without one.
    let mut live_blobs: HashSet<Digest> = HashSet::new();
    for (path, entry) in par_map(&live_entries, |p| (p.clone(), read_entry(p))) {
        match entry {
            Entry::Result(digests) => live_blobs.extend(digests),
            Entry::Manifest => {}
            Entry::Unreadable => {
                if del.remove_file(&path) {
                    report.entries += 1;
                }
            }
        }
    }
    let is_settled = |t: SystemTime| now.duration_since(t).is_ok_and(|age| age > BLOB_GRACE);
    for blob in store_files(&cache_root.join("cas")) {
        let named = blob.file_name().and_then(Digest::from_hex).is_some_and(|d| live_blobs.contains(&d));
        if !named && mtime(&blob).is_some_and(is_settled) && del.remove_file(&blob) {
            report.blobs += 1;
        }
    }

    // 4. Temp files a killed process left behind.
    if let Ok(entries) = cache_root.join("tmp").read_dir_utf8() {
        for entry in entries.flatten() {
            if mtime(entry.path()).is_some_and(is_settled) {
                del.remove_file(entry.path());
            }
        }
    }

    // 5. The stat cache remembers every output it has seen.
    if !opts.dry_run && report.target_dirs > 0 {
        let stat_cache = StatCache::load(cache_root.join("statcache"));
        stat_cache.drop_missing();
        stat_cache.save().context("saving the stat cache")?;
    }

    report.freed_bytes = del.freed_bytes;
    report.skipped = del.skipped;
    Ok(report)
}

/// The pass at the end of every command (`main`): at most once per
/// [`AUTO_INTERVAL`], with the default age limit. Does nothing where there is
/// no `.anubis-cache`. Says something only if it removed something, and never
/// fails the command.
pub fn run_if_due(root: &Utf8Path) {
    let marker = root.join(".anubis-cache").join("gc-last");
    let now = SystemTime::now();
    if mtime(&marker).is_some_and(|t| now.duration_since(t).map_or(true, |age| age < AUTO_INTERVAL)) {
        return;
    }
    // Written first, so builds finishing together don't all start a pass.
    if std::fs::write(&marker, b"").is_err() {
        return;
    }
    let opts = GcOptions { max_age: Duration::from_secs(DEFAULT_MAX_AGE_DAYS * 24 * 60 * 60), dry_run: false };
    match run(root, opts, now) {
        Ok(report) if report.removed_anything() => {
            tracing::info!("{} (unused for {DEFAULT_MAX_AGE_DAYS} days; see `anubis gc`)", report.summary(false))
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("gc failed: {e:#}"),
    }
}
