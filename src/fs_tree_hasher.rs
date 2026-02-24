//! # FsTreeHasher — cached filesystem hashing with watch-based invalidation
//!
//! Designed for build system cache invalidation where correctness is paramount.
//!
//! ## Hashing modes
//!
//! The hashing strategy is chosen at construction time:
//!
//! - **[`HashMode::Fast`]**: Uses mtime + size. Very cheap (one `stat` per miss).
//!   Can miss same-size writes within the same timestamp quantum (1ns ext4,
//!   100ns NTFS). Suitable for structural change detection.
//!
//! - **[`HashMode::Full`]**: Uses xxh3-128 of file content. Reads the full file on
//!   cache miss. Collision probability negligible (~2⁻⁶⁴ per pair).
//!   Suitable when correctness is paramount.
//!
//! For directories, `hash_dir` always uses a structural fingerprint (sorted file
//! listing + per-file hash), where each file's hash follows the configured mode.
//!
//! ## Correctness guarantees
//!
//! 1. **Negligible false-negative probability** (in `Full` mode). Uses xxh3-128.
//!    Collision probability is ~2⁻⁶⁴ per file pair (birthday bound) —
//!    effectively zero for any real build tree.
//!
//! 2. **Best-effort change detection** (in `Fast` mode). Detects additions,
//!    deletions, renames, and most modifications. Can miss same-size writes
//!    within the same timestamp quantum.
//!
//! 3. **Race safety.** A generation counter prevents caching data computed
//!    while the filesystem was being modified.
//!
//! 4. **Watch-before-compute ordering.** The filesystem watcher is always
//!    registered before any hashing begins.
//!
//! 5. **Graceful degradation.** If the OS watcher cannot be registered
//!    (e.g., inotify limit), that path will be re-computed on every query.
//!
//! ## Known limitations
//!
//! - **Symlinked directories.** `hash_dir` detects subdirectories that are
//!   symlinks pointing outside the root and watches them automatically.
//!   Individual file symlinks pointing outside the root are NOT detected
//!   by `hash_dir`; use `hash_file` on those files directly.
//!
//! - **Parent directory replacement.** Detected via filesystem identity
//!   (dev+ino on Unix, volume+file_index on Windows). If identity cannot
//!   be determined, we fall back to watcher-only invalidation.
//!
//! ## Usage
//!
//! ```text
//! use fs_tree_hasher::{FsTreeHasher, HashMode};
//!
//! let hasher = FsTreeHasher::new(HashMode::Full)?;
//!
//! let h = hasher.hash_file("src/main.rs")?;
//! let dh = hasher.hash_dir("include/")?;
//!
//! if hasher.hash_dir("include/")? != saved_hash {
//!     // rebuild
//! }
//! ```

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};
use std::time::SystemTime;

use camino::{Utf8Path, Utf8PathBuf};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;

use crate::util::SlashFix;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct FsTreeHasher {
    mode: HashMode,
    state: Arc<RwLock<State>>,
    watcher: Mutex<RecommendedWatcher>,
}

/// Hashing strategy, chosen at construction time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashMode {
    /// mtime + size. Fast (one `stat` per miss), but can miss same-size
    /// writes within the same filesystem timestamp quantum.
    #[default]
    Fast,
    /// xxh3-128 of file content. Reads the full file on cache miss.
    /// Collision probability negligible (~2⁻⁶⁴).
    Full,
}

/// A 128-bit non-cryptographic hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContentHash(pub u128);

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------
#[derive(Debug)]
struct State {
    generation: u64,
    files: HashMap<Utf8PathBuf, CachedFile>,
    dirs: HashMap<Utf8PathBuf, CachedDir>,
    watched: HashMap<Utf8PathBuf, WatchInfo>,
    /// Maps canonical symlink target dir → walk path (through the symlink).
    /// Used by the watcher to translate canonical event paths to walk-path
    /// space for proper parent-chain invalidation.
    symlink_reverse_map: HashMap<Utf8PathBuf, Utf8PathBuf>,
}

#[derive(Clone, Copy, Debug)]
struct WatchInfo {
    success: bool,
    mode: RecursiveMode,
}

#[derive(Debug)]
struct CachedFile {
    hash: ContentHash,
}

#[derive(Debug)]
struct CachedDir {
    hash: ContentHash,
    identity: Option<DirIdentity>,
    symlink_targets: Vec<Utf8PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirIdentity {
    volume: u64,
    index: u64,
}

const MAX_RETRIES: usize = 3;

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl FsTreeHasher {
    /// Create a new hasher with the given hashing strategy.
    pub fn new(mode: HashMode) -> io::Result<Self> {
        let state = Arc::new(RwLock::new(State {
            generation: 0,
            files: HashMap::new(),
            dirs: HashMap::new(),
            watched: HashMap::new(),
            symlink_reverse_map: HashMap::new(),
        }));

        let state_ref = state.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };

            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }

            let mut s = state_ref.write();
            s.generation += 1;

            for path in &event.paths {
                let Some(raw) = path.to_str().map(Utf8PathBuf::from) else {
                    continue;
                };

                tracing::info!("Watcher event: {:?} on {}", event.kind, raw);

                // Normalize the event path using cheap string normalization
                // (lowercase + forward slashes on Windows) to match the keys
                // stored in our caches.
                let normalized = normalize_path(Utf8Path::new(raw.as_str()));

                // --- File eviction ---
                if s.files.remove(&normalized).is_some() {
                    tracing::info!("Watcher invalidated file: {}", normalized);
                }

                // --- Directory eviction ---
                // A dir entry must be evicted if the event path falls under:
                //   (a) the dir's own path, or
                //   (b) any of the dir's symlink targets.
                s.dirs.retain(|dir, cached| {
                    if normalized.starts_with(dir.as_str()) {
                        tracing::info!("Watcher invalidated dir (under): {}", dir);
                        return false;
                    }
                    for target in &cached.symlink_targets {
                        if normalized.starts_with(target.as_str()) {
                            tracing::info!("Watcher invalidated dir (symlink target): {} (target: {})", dir, target);
                            return false;
                        }
                    }

                    true
                });

                // Translate event paths to walk-space using symlink reverse map,
                // then walk up ALL parents to evict intermediate dir entries.
                // Only do canonical lookup if the reverse map is non-empty.
                if !s.symlink_reverse_map.is_empty() {
                    let mut walk_equivalents: Vec<Utf8PathBuf> = Vec::new();
                    {
                        // For symlink reverse map lookup, we need canonical paths
                        // since the map keys are canonical symlink targets.
                        let canonical = canonicalize_utf8(path).ok();
                        let event_paths: Vec<&str> = std::iter::once(normalized.as_str())
                            .chain(canonical.as_deref().map(|c| c.as_str()))
                            .collect();
                        for event_path in &event_paths {
                            for (canonical_target, walk_path) in &s.symlink_reverse_map {
                                if let Some(suffix) = event_path.strip_prefix(canonical_target.as_str()) {
                                    walk_equivalents.push(Utf8PathBuf::from(format!("{}{}", walk_path, suffix)));
                                }
                            }
                        }
                    }
                    for walk_eq in &walk_equivalents {
                        let mut current = walk_eq.as_path();
                        while let Some(parent) = current.parent() {
                            if parent.as_str().is_empty() { break; }
                            if s.dirs.remove(parent).is_some() {
                                tracing::info!("Watcher invalidated dir (symlink parent walk-up): {}", parent);
                            }
                            current = parent;
                        }
                    }
                }

                // Also evict parent dir entries directly.
                if let Some(parent) = normalized.parent() {
                    if s.dirs.remove(parent).is_some() {
                        tracing::info!("Watcher invalidated parent dir: {}", parent);
                    }
                }
            }
        })
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(Self {
            mode,
            state,
            watcher: Mutex::new(watcher),
        })
    }

    /// The hashing mode this hasher was created with.
    pub fn mode(&self) -> HashMode {
        self.mode
    }

    // -----------------------------------------------------------------------
    // File hashing
    // -----------------------------------------------------------------------

    /// Hash a single file.
    ///
    /// In `Full` mode, reads the file content and hashes with xxh3-128.
    /// In `Fast` mode, hashes mtime + size (one `stat` call).
    pub fn hash_file(&self, path: impl AsRef<Utf8Path>) -> io::Result<ContentHash> {
        let _sl = crate::superluminal_span!("hash_file");
        let path = normalize_path(path.as_ref());

        // Fast path: cache hit
        {
            let s = self.state.read();
            if let Some(cached) = s.files.get(&path) {
                return Ok(cached.hash);
            }
        }

        let is_watched = self.ensure_parent_watched(&path)?;

        for _ in 0..MAX_RETRIES {
            let gen_before = { self.state.read().generation };

            let hash = self.compute_file_hash(&path)?;

            let mut s = self.state.write();

            if !is_watched {
                return Ok(hash);
            }

            if s.generation == gen_before {
                s.files.insert(path, CachedFile { hash });
                return Ok(hash);
            }
        }

        self.compute_file_hash(&path)
    }

    // -----------------------------------------------------------------------
    // Directory hashing
    // -----------------------------------------------------------------------

    /// Hash a directory tree.
    ///
    /// The hash incorporates sorted file paths and per-file hashes (using
    /// the configured mode). Detects additions, deletions, renames, and
    /// modifications.
    pub fn hash_dir(&self, dir: impl AsRef<Utf8Path>) -> io::Result<ContentHash> {
        let dir = normalize_path(dir.as_ref());

        // Fast path: cache hit with identity verification.
        {
            let s = self.state.read();
            if let Some(cached) = s.dirs.get(&dir) {
                match (cached.identity, get_dir_identity(&dir)) {
                    (Some(old), Some(current)) if old == current => {
                        return Ok(cached.hash);
                    }
                    (Some(_), Some(_)) => { /* replaced — recompute */ }
                    (Some(_), None) => { /* stat failed — recompute */ }
                    (None, _) => {
                        // No identity support — rely on watcher only.
                        return Ok(cached.hash);
                    }
                }
            }
        }

        let is_watched = self.ensure_dir_watched(&dir)?;

        // Discover and watch external symlink targets BEFORE gen snapshot.
        let mut known_targets = discover_external_symlinks(&dir);
        for target in &known_targets {
            let _ = self.do_watch(target, RecursiveMode::Recursive);
        }

        for _ in 0..MAX_RETRIES {
            let gen_before = { self.state.read().generation };

            let result = self.compute_dir_hash(&dir)?;

            // If the full walk found new symlink targets, watch and retry.
            let new_targets: Vec<_> = result
                .symlink_targets
                .iter()
                .filter(|t| !known_targets.contains(t))
                .cloned()
                .collect();

            if !new_targets.is_empty() {
                for target in &new_targets {
                    let _ = self.do_watch(target, RecursiveMode::Recursive);
                }
                known_targets.extend(new_targets);
                continue;
            }

            let mut s = self.state.write();

            if !is_watched {
                return Ok(result.hash);
            }

            if s.generation == gen_before {
                let hash = result.hash;
                let identity = get_dir_identity(&dir);
                // Store per-file hashes so subsequent hash_file() calls are cache hits
                for (file_path, file_hash) in result.file_hashes {
                    s.files.insert(file_path, CachedFile { hash: file_hash });
                }
                // Store subdirectory hashes so subsequent hash_dir() calls are cache hits
                for (subdir_path, subdir_hash) in result.subdir_hashes {
                    let subdir_identity = get_dir_identity(&subdir_path);
                    s.dirs.insert(
                        subdir_path,
                        CachedDir {
                            hash: subdir_hash,
                            identity: subdir_identity,
                            symlink_targets: Vec::new(),
                        },
                    );
                }
                // Populate symlink reverse map for watcher event translation
                for (walk_path, canonical_target) in &result.symlink_walk_map {
                    s.symlink_reverse_map.insert(canonical_target.clone(), walk_path.clone());
                }
                s.dirs.insert(
                    dir,
                    CachedDir {
                        hash,
                        identity,
                        symlink_targets: result.symlink_targets,
                    },
                );
                return Ok(hash);
            }
        }

        Ok(self.compute_dir_hash(&dir)?.hash)
    }

    // -----------------------------------------------------------------------
    // Invalidation
    // -----------------------------------------------------------------------

    /// Manually invalidate a path (file or directory).
    pub fn invalidate(&self, path: impl AsRef<Utf8Path>) {
        let mut s = self.state.write();
        s.generation += 1;

        let normalized = normalize_path(path.as_ref());
        s.files.remove(&normalized);
        s.dirs.remove(&normalized);

        s.dirs.retain(|dir, _| {
            !normalized.starts_with(dir.as_str()) && !dir.starts_with(normalized.as_str())
        });
    }

    /// Invalidate all cached entries.
    pub fn invalidate_all(&self) {
        let mut s = self.state.write();
        s.generation += 1;
        s.files.clear();
        s.dirs.clear();
    }

    /// Number of cached entries: (files, dirs).
    pub fn cached_count(&self) -> (usize, usize) {
        let s = self.state.read();
        (s.files.len(), s.dirs.len())
    }

    // -----------------------------------------------------------------------
    // Cache serialization
    // -----------------------------------------------------------------------

    /// Check if a directory is already in the cache.
    pub fn has_dir_cached(&self, dir: impl AsRef<Utf8Path>) -> bool {
        let dir = normalize_path(dir.as_ref());
        let s = self.state.read();
        s.dirs.contains_key(&dir)
    }

    /// Save the current cache to a text file on disk.
    ///
    /// Format:
    /// ```text
    /// # anubis cache v1
    /// mode fast|full
    /// dir <hash_hex> <path> [symlink1;symlink2;...]
    /// file <hash_hex> <path>
    /// ```
    ///
    /// Errors are non-fatal — logged and swallowed.
    pub fn save_cache(&self, path: &Utf8Path) {
        if let Err(e) = self.save_cache_inner(path) {
            tracing::warn!("Failed to save FsTreeHasher cache to {path}: {e}");
        }
    }

    fn save_cache_inner(&self, path: &Utf8Path) -> io::Result<()> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent.as_std_path())?;
        }

        let tmp = Utf8PathBuf::from(format!("{}.tmp", path));
        let file = std::fs::File::create(tmp.as_std_path())?;
        let mut w = std::io::BufWriter::new(file);

        writeln!(w, "# anubis cache v1")?;
        let mode_str = match self.mode {
            HashMode::Fast => "fast",
            HashMode::Full => "full",
        };
        writeln!(w, "mode {mode_str}")?;

        let s = self.state.read();

        for (dir_path, cached) in &s.dirs {
            let symlinks = cached
                .symlink_targets
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(";");
            writeln!(w, "dir {:032x} {} {}", cached.hash.0, dir_path, symlinks)?;
        }

        for (file_path, cached) in &s.files {
            writeln!(w, "file {:032x} {}", cached.hash.0, file_path)?;
        }

        drop(w);

        // Atomic rename
        std::fs::rename(tmp.as_std_path(), path.as_std_path())?;
        Ok(())
    }

    /// Load a previously saved cache from disk.
    ///
    /// Returns `true` if the cache was loaded successfully, `false` if the
    /// file was missing, the mode mismatched, or parsing failed.
    pub fn load_cache(&self, path: &Utf8Path) -> bool {
        match self.load_cache_inner(path) {
            Ok(loaded) => loaded,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!("Failed to load FsTreeHasher cache from {path}: {e}");
                }
                false
            }
        }
    }

    fn load_cache_inner(&self, path: &Utf8Path) -> io::Result<bool> {
        use std::io::BufRead;

        let file = std::fs::File::open(path.as_std_path())?;
        let reader = std::io::BufReader::new(file);

        let mut dirs: Vec<(Utf8PathBuf, CachedDir)> = Vec::new();
        let mut files: Vec<(Utf8PathBuf, CachedFile)> = Vec::new();
        let mut mode_validated = false;

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some(mode_str) = line.strip_prefix("mode ") {
                let expected = match self.mode {
                    HashMode::Fast => "fast",
                    HashMode::Full => "full",
                };
                if mode_str.trim() != expected {
                    tracing::info!(
                        "FsTreeHasher cache mode mismatch: expected {expected}, got {mode_str}"
                    );
                    return Ok(false);
                }
                mode_validated = true;
                continue;
            }

            if !mode_validated {
                tracing::info!("FsTreeHasher cache missing mode header");
                return Ok(false);
            }

            if let Some(rest) = line.strip_prefix("dir ") {
                // dir <hash_hex> <path> [symlinks]
                let mut parts = rest.splitn(3, ' ');
                let Some(hash_hex) = parts.next() else {
                    return Ok(false);
                };
                let Some(dir_path) = parts.next() else {
                    return Ok(false);
                };
                let symlinks_str = parts.next().unwrap_or("");

                let Some(hash) = ContentHash::from_hex(hash_hex) else {
                    return Ok(false);
                };

                let symlink_targets: Vec<Utf8PathBuf> = if symlinks_str.is_empty() {
                    Vec::new()
                } else {
                    symlinks_str
                        .split(';')
                        .filter(|s| !s.is_empty())
                        .map(Utf8PathBuf::from)
                        .collect()
                };

                dirs.push((
                    Utf8PathBuf::from(dir_path),
                    CachedDir {
                        hash,
                        identity: None, // Cannot restore identity from cache
                        symlink_targets,
                    },
                ));
            } else if let Some(rest) = line.strip_prefix("file ") {
                // file <hash_hex> <path>
                let mut parts = rest.splitn(2, ' ');
                let Some(hash_hex) = parts.next() else {
                    return Ok(false);
                };
                let Some(file_path) = parts.next() else {
                    return Ok(false);
                };

                let Some(hash) = ContentHash::from_hex(hash_hex) else {
                    return Ok(false);
                };

                files.push((Utf8PathBuf::from(file_path), CachedFile { hash }));
            } else {
                // Unknown line type — reject cache
                return Ok(false);
            }
        }

        if !mode_validated {
            return Ok(false);
        }

        // Populate state
        let mut s = self.state.write();
        for (path, cached_dir) in dirs {
            s.dirs.insert(path, cached_dir);
        }
        for (path, cached_file) in files {
            s.files.insert(path, cached_file);
        }

        tracing::info!(
            "Loaded FsTreeHasher cache: {} dirs, {} files",
            s.dirs.len(),
            s.files.len()
        );
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Hash computation (delegates to mode)
    // -----------------------------------------------------------------------

    fn compute_file_hash(&self, path: &Utf8Path) -> io::Result<ContentHash> {
        match self.mode {
            HashMode::Fast => hash_file_fast(path),
            HashMode::Full => hash_file_full(path),
        }
    }

    fn compute_dir_hash(&self, dir: &Utf8Path) -> io::Result<DirHashResult> {
        compute_dir_hash(dir, self.mode)
    }

    // -----------------------------------------------------------------------
    // Watch management
    // -----------------------------------------------------------------------

    fn ensure_parent_watched(&self, path: &Utf8Path) -> io::Result<bool> {
        match path.parent() {
            Some(parent) => self.do_watch(parent, RecursiveMode::NonRecursive),
            None => Ok(false),
        }
    }

    fn ensure_dir_watched(&self, dir: &Utf8Path) -> io::Result<bool> {
        self.do_watch(dir, RecursiveMode::Recursive)
    }

    fn do_watch(&self, dir: &Utf8Path, mode: RecursiveMode) -> io::Result<bool> {
        let _sl = crate::superluminal_span!("do_watch");

        {
            let s = self.state.read();

            if let Some(info) = s.watched.get(dir) {
                if !info.success {
                    return Ok(false);
                }
                if mode == RecursiveMode::Recursive
                    && info.mode == RecursiveMode::NonRecursive
                {
                    // Need upgrade — fall through to re-register.
                } else {
                    return Ok(true);
                }
            }

            for (watched_dir, info) in &s.watched {
                if info.success
                    && info.mode == RecursiveMode::Recursive
                    && dir.starts_with(watched_dir.as_str())
                {
                    return Ok(true);
                }
            }
        }

        // State lock NOT held — watcher callback also takes it.
        let watch_result = self
            .watcher
            .lock()
            .watch(dir.as_std_path(), mode);

        let success = match watch_result {
            Ok(()) => true,
            Err(e) => {
                eprintln!(
                    "fs_tree_hasher: failed to watch {dir}: {e} \
                     (falling back to uncached mode — consider raising \
                     fs.inotify.max_user_watches)",
                );
                false
            }
        };

        let mut s = self.state.write();
        s.watched.insert(dir.to_owned(), WatchInfo { success, mode });
        Ok(success)
    }
}

// ---------------------------------------------------------------------------
// Filesystem identity (directory replacement detection)
// ---------------------------------------------------------------------------

fn get_dir_identity(path: &Utf8Path) -> Option<DirIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path.as_std_path())
            .ok()
            .map(|m| DirIdentity {
                volume: m.dev(),
                index: m.ino(),
            })
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;

        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path.as_std_path())
            .ok()?;

        unsafe {
            let handle = dir.as_raw_handle() as *mut std::ffi::c_void;
            let mut info: windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION =
                std::mem::zeroed();
            let ret = windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(
                handle, &mut info,
            );
            if ret == 0 {
                return None;
            }
            Some(DirIdentity {
                volume: info.dwVolumeSerialNumber as u64,
                index: ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64),
            })
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

// ---------------------------------------------------------------------------
// Symlink discovery
// ---------------------------------------------------------------------------

fn discover_external_symlinks(root: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut targets = Vec::new();
    let mut visited = std::collections::HashSet::new();
    visited.insert(root.to_owned());
    discover_symlinks_recursive(root, root.as_std_path(), &mut targets, &mut visited);
    targets.sort();
    targets.dedup();
    targets
}

fn discover_symlinks_recursive(
    root: &Utf8Path,
    current: &std::path::Path,
    targets: &mut Vec<Utf8PathBuf>,
    visited: &mut std::collections::HashSet<Utf8PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };

        if meta.is_symlink() {
            let Ok(target) = std::fs::canonicalize(&path) else {
                continue;
            };
            if target.is_dir() && !target.starts_with(root.as_std_path()) {
                if let Ok(utf8) = Utf8PathBuf::try_from(target.clone()) {
                    if visited.insert(utf8.clone()) {
                        targets.push(utf8);
                        discover_symlinks_recursive(root, &target, targets, visited);
                    }
                    // else: already visited — cycle detected, skip
                }
            }
        } else if meta.is_dir() {
            discover_symlinks_recursive(root, &path, targets, visited);
        }
    }
}

// ---------------------------------------------------------------------------
// Path utilities
// ---------------------------------------------------------------------------

/// Cheap string-only path normalization: lowercase + forward slashes on Windows,
/// forward slashes only on other platforms. Used instead of `canonicalize_utf8()`
/// in hot paths to avoid expensive filesystem syscalls.
pub(crate) fn normalize_path(path: &Utf8Path) -> Utf8PathBuf {
    #[cfg(windows)]
    {
        let p: Utf8PathBuf = path.as_str().to_lowercase().into();
        p.slash_fix()
    }
    #[cfg(not(windows))]
    {
        let p: Utf8PathBuf = path.into();
        p.slash_fix()
    }
}

fn canonicalize_utf8(path: &std::path::Path) -> io::Result<Utf8PathBuf> {
    let _sl = crate::superluminal_span!("canonicalize_utf8");

    let canonical = std::fs::canonicalize(path)?;
    Utf8PathBuf::try_from(canonical).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("path is not valid UTF-8: {e}"),
        )
    })
}

// ---------------------------------------------------------------------------
// Hash computation
// ---------------------------------------------------------------------------

/// Hash file by mtime + size.
fn hash_file_fast(path: &Utf8Path) -> io::Result<ContentHash> {
    let meta = std::fs::metadata(path.as_std_path())?;
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let size = meta.len();

    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hash_mtime(&mut hasher, &mtime);
    hasher.update(&size.to_le_bytes());
    Ok(ContentHash(hasher.digest128()))
}

/// Hash file by full content (xxh3-128).
fn hash_file_full(path: &Utf8Path) -> io::Result<ContentHash> {
    let content = std::fs::read(path.as_std_path())?;
    Ok(ContentHash(xxhash_rust::xxh3::xxh3_128(&content)))
}

struct DirHashResult {
    hash: ContentHash,
    symlink_targets: Vec<Utf8PathBuf>,
    file_hashes: Vec<(Utf8PathBuf, ContentHash)>,
    /// Hashes for every subdirectory encountered during the walk.
    /// Each entry is (canonical_abs_path, hash).
    subdir_hashes: Vec<(Utf8PathBuf, ContentHash)>,
    /// (walk_path, canonical_target) for each external symlink found during the walk.
    /// Used to build the reverse map for watcher event translation.
    symlink_walk_map: Vec<(Utf8PathBuf, Utf8PathBuf)>,
}

fn compute_dir_hash(dir: &Utf8Path, mode: HashMode) -> io::Result<DirHashResult> {
    let mut entries: Vec<(Utf8PathBuf, FileFingerprint)> = Vec::new();
    let mut symlink_targets: Vec<Utf8PathBuf> = Vec::new();
    let mut file_hashes: Vec<(Utf8PathBuf, ContentHash)> = Vec::new();
    // Map relative dir path → canonical absolute path, populated during the walk
    let mut dir_canonicals = HashMap::<Utf8PathBuf, Utf8PathBuf>::new();
    // (walk_path, canonical_target) for external symlinks
    let mut symlink_walk_map: Vec<(Utf8PathBuf, Utf8PathBuf)> = Vec::new();

    for entry in jwalk::WalkDir::new(dir.as_std_path())
        .follow_links(true)
        .into_iter()
    {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_dir() && entry.depth() > 0 {
                    if let Ok(rel) = entry.path().strip_prefix(dir.as_std_path()) {
                        if let Ok(rel_utf8) = Utf8PathBuf::try_from(rel.to_path_buf()) {
                            // Record normalized absolute path for subdirectory hash caching.
                            let abs_path = dir.join(&rel_utf8);
                            dir_canonicals.insert(rel_utf8, abs_path);
                        }
                    }

                    // Detect external symlinks for watcher registration
                    if let Ok(meta) = std::fs::symlink_metadata(entry.path()) {
                        if meta.is_symlink() {
                            if let Ok(target) = std::fs::canonicalize(entry.path()) {
                                if !target.starts_with(dir.as_std_path()) {
                                    if let Ok(utf8) = Utf8PathBuf::try_from(target) {
                                        // Record walk_path → canonical_target for reverse map
                                        if let Ok(walk_utf8) = Utf8PathBuf::try_from(entry.path().to_path_buf()) {
                                            symlink_walk_map.push((walk_utf8, utf8.clone()));
                                        }
                                        symlink_targets.push(utf8);
                                    }
                                }
                            }
                        }
                    }
                }

                if entry.file_type().is_file() {
                    let Ok(rel) = entry
                        .path()
                        .strip_prefix(dir.as_std_path())
                        .map(|p| p.to_path_buf())
                    else {
                        continue;
                    };
                    let Ok(rel_utf8) = Utf8PathBuf::try_from(rel) else {
                        continue;
                    };

                    let fingerprint = match mode {
                        HashMode::Fast => {
                            let meta = std::fs::metadata(entry.path())?;
                            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                            FileFingerprint::Fast {
                                mtime,
                                size: meta.len(),
                            }
                        }
                        HashMode::Full => {
                            let content = std::fs::read(entry.path()).map_err(|e| {
                                io::Error::new(
                                    e.kind(),
                                    format!("fs_tree_hasher: cannot read {}: {e}", entry.path().display()),
                                )
                            })?;
                            FileFingerprint::Full {
                                content_hash: xxhash_rust::xxh3::xxh3_128(&content),
                            }
                        }
                    };

                    // Collect per-file hash for cache population
                    let file_hash = fingerprint_to_content_hash(&fingerprint);
                    let abs_path = dir.join(&rel_utf8);
                    file_hashes.push((abs_path, file_hash));

                    entries.push((rel_utf8, fingerprint));
                }
            }
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("fs_tree_hasher: directory walk error in {dir}: {e}"),
                ));
            }
        }
    }

    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    symlink_targets.sort();
    symlink_targets.dedup();

    // Compute main directory hash
    let hash = hash_entries(&entries, &symlink_targets);

    // Compute hashes for every subdirectory encountered during the walk.
    let mut unique_dirs: Vec<_> = dir_canonicals.keys().cloned().collect();
    unique_dirs.sort();

    let mut subdir_hashes: Vec<(Utf8PathBuf, ContentHash)> = Vec::with_capacity(unique_dirs.len());
    for rel_dir in &unique_dirs {
        let prefix = format!("{}{}", rel_dir, std::path::MAIN_SEPARATOR);

        let sub_entries: Vec<(Utf8PathBuf, &FileFingerprint)> = entries
            .iter()
            .filter(|(p, _)| p.as_str().starts_with(&prefix))
            .map(|(p, fp)| {
                let sub_rel = Utf8PathBuf::from(&p.as_str()[prefix.len()..]);
                (sub_rel, fp)
            })
            .collect();

        let sub_hash = hash_entry_refs(&sub_entries, &[]);
        let canonical = &dir_canonicals[rel_dir];
        subdir_hashes.push((canonical.clone(), sub_hash));
    }

    Ok(DirHashResult {
        hash,
        symlink_targets,
        file_hashes,
        subdir_hashes,
        symlink_walk_map,
    })
}

enum FileFingerprint {
    Fast { mtime: SystemTime, size: u64 },
    Full { content_hash: u128 },
}

/// Hash a list of (relative_path, fingerprint) entries plus symlink targets.
/// Entries must already be sorted by path.
fn hash_entries(entries: &[(Utf8PathBuf, FileFingerprint)], symlink_targets: &[Utf8PathBuf]) -> ContentHash {
    hash_entry_refs(
        &entries.iter().map(|(p, fp)| (p.clone(), fp)).collect::<Vec<_>>(),
        symlink_targets,
    )
}

/// Same as `hash_entries` but with borrowed fingerprints (for subdirectory computation).
fn hash_entry_refs(entries: &[(Utf8PathBuf, &FileFingerprint)], symlink_targets: &[Utf8PathBuf]) -> ContentHash {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&entries.len().to_le_bytes());

    for (path, fingerprint) in entries {
        let bytes = path.as_str().as_bytes();
        hasher.update(&bytes.len().to_le_bytes());
        hasher.update(bytes);

        match fingerprint {
            FileFingerprint::Fast { mtime, size } => {
                hasher.update(&[0x00]);
                hash_mtime(&mut hasher, mtime);
                hasher.update(&size.to_le_bytes());
            }
            FileFingerprint::Full { content_hash } => {
                hasher.update(&[0x01]);
                hasher.update(&content_hash.to_le_bytes());
            }
        }
    }

    hasher.update(&symlink_targets.len().to_le_bytes());
    for target in symlink_targets {
        hasher.update(target.as_str().as_bytes());
    }

    ContentHash(hasher.digest128())
}

/// Convert a FileFingerprint to a standalone ContentHash, producing the same
/// value as hash_file_fast/hash_file_full would without re-reading the file.
fn fingerprint_to_content_hash(fp: &FileFingerprint) -> ContentHash {
    match fp {
        FileFingerprint::Fast { mtime, size } => {
            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
            hash_mtime(&mut hasher, mtime);
            hasher.update(&size.to_le_bytes());
            ContentHash(hasher.digest128())
        }
        FileFingerprint::Full { content_hash } => ContentHash(*content_hash),
    }
}

fn hash_mtime(hasher: &mut xxhash_rust::xxh3::Xxh3, mtime: &SystemTime) {
    match mtime.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(dur) => {
            hasher.update(&dur.as_secs().to_le_bytes());
            hasher.update(&dur.subsec_nanos().to_le_bytes());
        }
        Err(_) => {
            hasher.update(&[0xFF; 12]);
        }
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

impl ContentHash {
    pub fn as_u128(self) -> u128 {
        self.0
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        u128::from_str_radix(s, 16).ok().map(ContentHash)
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}
