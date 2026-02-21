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
//! ## Network filesystems
//!
//! Hashing paths on network filesystems (NFS, SMB, CIFS, Docker bind mounts)
//! is not supported. These filesystems cannot deliver reliable change events,
//! making cached results unsafe. Attempting to hash a network path returns
//! an error.
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
//! ```ignore
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
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use camino::{Utf8Path, Utf8PathBuf};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct FsTreeHasher {
    mode: HashMode,
    state: Arc<Mutex<State>>,
    watcher: Mutex<RecommendedWatcher>,
}

/// Hashing strategy, chosen at construction time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashMode {
    /// mtime + size. Fast (one `stat` per miss), but can miss same-size
    /// writes within the same filesystem timestamp quantum.
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

struct State {
    generation: u64,
    files: HashMap<Utf8PathBuf, CachedFile>,
    dirs: HashMap<Utf8PathBuf, CachedDir>,
    watched: HashMap<Utf8PathBuf, WatchInfo>,
}

#[derive(Clone, Copy)]
struct WatchInfo {
    success: bool,
    mode: RecursiveMode,
}

struct CachedFile {
    hash: ContentHash,
}

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
        let state = Arc::new(Mutex::new(State {
            generation: 0,
            files: HashMap::new(),
            dirs: HashMap::new(),
            watched: HashMap::new(),
        }));

        let state_ref = state.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };

            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }

            let mut s = state_ref.lock().unwrap_or_else(|e| e.into_inner());
            s.generation += 1;

            for path in &event.paths {
                let Some(raw) = path.to_str().map(Utf8PathBuf::from) else {
                    continue;
                };

                // Resolve canonical form once. May fail if file was deleted,
                // but that's fine — we still have the raw path. On macOS,
                // FSEvents can deliver NFD-normalized paths while canonical
                // paths are NFC. On Windows, event paths may have different
                // casing. Using both forms ensures eviction works regardless
                // of platform normalization differences.
                let canonical = canonicalize_utf8(path).ok();

                // --- File eviction ---
                s.files.remove(&raw);
                if let Some(ref c) = canonical {
                    s.files.remove(c);
                }

                // --- Directory eviction ---
                // A dir entry must be evicted if the event path falls under:
                //   (a) the dir's own path, or
                //   (b) any of the dir's symlink targets.
                // We check both raw and canonical event paths to handle
                // platform normalization mismatches.
                s.dirs.retain(|dir, cached| {
                    // Check raw event path
                    if raw.starts_with(dir.as_str()) {
                        return false;
                    }
                    for target in &cached.symlink_targets {
                        if raw.starts_with(target.as_str()) {
                            return false;
                        }
                    }

                    // Check canonical event path (if available)
                    if let Some(ref c) = canonical {
                        if c.starts_with(dir.as_str()) {
                            return false;
                        }
                        for target in &cached.symlink_targets {
                            if c.starts_with(target.as_str()) {
                                return false;
                            }
                        }
                    }

                    true
                });

                // Also evict parent dir entries directly.
                if let Some(parent) = raw.parent() {
                    s.dirs.remove(parent);
                }
                if let Some(ref c) = canonical {
                    if let Some(parent) = c.parent() {
                        s.dirs.remove(parent);
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
        let path = canonicalize_utf8(path.as_ref().as_std_path())?;
        reject_network_path(&path)?;

        // Fast path: cache hit
        {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = s.files.get(&path) {
                return Ok(cached.hash);
            }
        }

        let is_watched = self.ensure_parent_watched(&path)?;

        for _ in 0..MAX_RETRIES {
            let gen_before = {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .generation
            };

            let hash = self.compute_file_hash(&path)?;

            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());

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
        let dir = canonicalize_utf8(dir.as_ref().as_std_path())?;
        reject_network_path(&dir)?;

        // Fast path: cache hit with identity verification.
        {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
            let gen_before = {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .generation
            };

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

            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());

            if !is_watched {
                return Ok(result.hash);
            }

            if s.generation == gen_before {
                let hash = result.hash;
                let identity = get_dir_identity(&dir);
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
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.generation += 1;

        let raw = path.as_ref();
        s.files.remove(raw);
        s.dirs.remove(raw);

        if let Ok(canonical) = canonicalize_utf8(raw.as_std_path()) {
            s.files.remove(&canonical);
            s.dirs.remove(&canonical);
            s.dirs.retain(|dir, _| {
                !canonical.starts_with(dir.as_str()) && !dir.starts_with(canonical.as_str())
            });
        }

        s.dirs.retain(|dir, _| {
            !raw.starts_with(dir.as_str()) && !dir.starts_with(raw.as_str())
        });
    }

    /// Invalidate all cached entries.
    pub fn invalidate_all(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.generation += 1;
        s.files.clear();
        s.dirs.clear();
    }

    /// Number of cached entries: (files, dirs).
    pub fn cached_count(&self) -> (usize, usize) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (s.files.len(), s.dirs.len())
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
        {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());

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
            .unwrap_or_else(|e| e.into_inner())
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

        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.watched.insert(dir.to_owned(), WatchInfo { success, mode });
        Ok(success)
    }
}

// ---------------------------------------------------------------------------
// Network filesystem detection
// ---------------------------------------------------------------------------

/// Reject paths on network filesystems where watchers are unreliable.
fn reject_network_path(path: &Utf8Path) -> io::Result<()> {
    if is_network_path(path)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "fs_tree_hasher: refusing to hash path on network filesystem: {path}. \
                 Network filesystems (NFS, SMB, CIFS) cannot deliver reliable \
                 change events. Copy files to a local filesystem first."
            ),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn is_network_path(path: &Utf8Path) -> io::Result<bool> {
    use std::mem::MaybeUninit;

    let c_path =
        std::ffi::CString::new(path.as_str()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let mut buf = MaybeUninit::<libc::statfs>::uninit();
    let ret = unsafe { libc::statfs(c_path.as_ptr(), buf.as_mut_ptr()) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { buf.assume_init() };

    // Known network filesystem magic numbers (from linux/magic.h).
    // Compare as u32: f_type is __fsword_t which is i32 on 32-bit arches
    // and i64 on 64-bit. All magic numbers fit in 32 bits, so truncating
    // from i64 is lossless, and interpreting i32 as u32 is bitwise correct.
    const NFS_SUPER_MAGIC: u32 = 0x6969;
    const SMB_SUPER_MAGIC: u32 = 0x517B;
    const SMB2_SUPER_MAGIC: u32 = 0xFE534D42;
    const CIFS_SUPER_MAGIC: u32 = 0xFF534D42;
    const CODA_SUPER_MAGIC: u32 = 0x73757245;
    const AFS_SUPER_MAGIC: u32 = 0x5346414F;

    let ftype = stat.f_type as u32;
    let is_network = matches!(
        ftype,
        NFS_SUPER_MAGIC
            | SMB_SUPER_MAGIC
            | SMB2_SUPER_MAGIC
            | CIFS_SUPER_MAGIC
            | CODA_SUPER_MAGIC
            | AFS_SUPER_MAGIC
    );

    Ok(is_network)
}

#[cfg(target_os = "macos")]
fn is_network_path(path: &Utf8Path) -> io::Result<bool> {
    use std::mem::MaybeUninit;

    let c_path =
        std::ffi::CString::new(path.as_str()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let mut buf = MaybeUninit::<libc::statfs>::uninit();
    let ret = unsafe { libc::statfs(c_path.as_ptr(), buf.as_mut_ptr()) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { buf.assume_init() };

    // f_fstypename is a C string on macOS
    let fstypename = unsafe {
        std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr())
            .to_string_lossy()
    };

    let is_network = matches!(
        fstypename.as_ref(),
        "nfs" | "smbfs" | "afpfs" | "webdav" | "cifs"
    );

    Ok(is_network)
}

#[cfg(windows)]
fn is_network_path(path: &Utf8Path) -> io::Result<bool> {
    let s = path.as_str();

    // Canonical UNC network paths: \\?\UNC\server\share\...
    if s.starts_with("\\\\?\\UNC\\") {
        return Ok(true);
    }

    // Non-canonical UNC paths: \\server\share\...
    // But NOT the \\?\ extended-length prefix (which is local).
    if s.starts_with("\\\\") && !s.starts_with("\\\\?\\") {
        return Ok(true);
    }

    // Drive letter check. Canonical paths from std::fs::canonicalize
    // have the form \\?\C:\..., so strip that prefix first.
    let check = s.strip_prefix("\\\\?\\").unwrap_or(s);
    if check.len() >= 3 && check.as_bytes()[1] == b':' && check.as_bytes()[2] == b'\\' {
        let root: Vec<u16> = check[..3].encode_utf16().chain(std::iter::once(0)).collect();
        let drive_type =
            unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) };
        // DRIVE_REMOTE = 4
        return Ok(drive_type == 4);
    }

    Ok(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn is_network_path(_path: &Utf8Path) -> io::Result<bool> {
    // Conservative: assume local on unknown platforms.
    // If this is wrong, the watcher will just fail to deliver events
    // and we'll serve uncached results (correct but slow).
    Ok(false)
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

fn canonicalize_utf8(path: &std::path::Path) -> io::Result<Utf8PathBuf> {
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
}

fn compute_dir_hash(dir: &Utf8Path, mode: HashMode) -> io::Result<DirHashResult> {
    let mut entries: Vec<(Utf8PathBuf, FileFingerprint)> = Vec::new();
    let mut symlink_targets: Vec<Utf8PathBuf> = Vec::new();

    for entry in jwalk::WalkDir::new(dir.as_std_path())
        .follow_links(true)
        .into_iter()
    {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_dir() && entry.depth() > 0 {
                    if let Ok(meta) = std::fs::symlink_metadata(entry.path()) {
                        if meta.is_symlink() {
                            if let Ok(target) = std::fs::canonicalize(entry.path()) {
                                if !target.starts_with(dir.as_std_path()) {
                                    if let Ok(utf8) = Utf8PathBuf::try_from(target) {
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

    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&entries.len().to_le_bytes());

    for (path, fingerprint) in &entries {
        let bytes = path.as_str().as_bytes();
        hasher.update(&bytes.len().to_le_bytes());
        hasher.update(bytes);

        match fingerprint {
            FileFingerprint::Fast { mtime, size } => {
                hasher.update(&[0x00]); // tag
                hash_mtime(&mut hasher, mtime);
                hasher.update(&size.to_le_bytes());
            }
            FileFingerprint::Full { content_hash } => {
                hasher.update(&[0x01]); // tag
                hasher.update(&content_hash.to_le_bytes());
            }
        }
    }

    // Hash symlink targets so retargeting is detected
    hasher.update(&symlink_targets.len().to_le_bytes());
    for target in &symlink_targets {
        hasher.update(target.as_str().as_bytes());
    }

    Ok(DirHashResult {
        hash: ContentHash(hasher.digest128()),
        symlink_targets,
    })
}

enum FileFingerprint {
    Fast { mtime: SystemTime, size: u64 },
    Full { content_hash: u128 },
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
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn tmp_utf8(dir: &tempfile::TempDir) -> &Utf8Path {
        Utf8Path::from_path(dir.path()).unwrap()
    }

    fn settle() {
        thread::sleep(Duration::from_millis(200));
    }

    #[test]
    fn file_hash_deterministic_full() {
        let dir = tmp_dir();
        let root = tmp_utf8(&dir);
        let file = root.join("test.txt");
        fs::write(file.as_std_path(), b"hello").unwrap();

        let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
        let h1 = hasher.hash_file(&file).unwrap();
        let h2 = hasher.hash_file(&file).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn file_hash_deterministic_fast() {
        let dir = tmp_dir();
        let root = tmp_utf8(&dir);
        let file = root.join("test.txt");
        fs::write(file.as_std_path(), b"hello").unwrap();

        let hasher = FsTreeHasher::new(HashMode::Fast).unwrap();
        let h1 = hasher.hash_file(&file).unwrap();
        let h2 = hasher.hash_file(&file).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn file_content_change_detected_full() {
        let dir = tmp_dir();
        let root = tmp_utf8(&dir);
        let file = root.join("test.txt");
        fs::write(file.as_std_path(), b"hello").unwrap();

        let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
        let h1 = hasher.hash_file(&file).unwrap();
        settle();

        fs::write(file.as_std_path(), b"world").unwrap();
        settle();

        let h2 = hasher.hash_file(&file).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn same_content_same_hash_full() {
        let dir = tmp_dir();
        let root = tmp_utf8(&dir);
        let f1 = root.join("a.txt");
        let f2 = root.join("b.txt");
        fs::write(f1.as_std_path(), b"identical").unwrap();
        fs::write(f2.as_std_path(), b"identical").unwrap();

        let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
        let h1 = hasher.hash_file(&f1).unwrap();
        let h2 = hasher.hash_file(&f2).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn dir_hash_add_file() {
        let dir = tmp_dir();
        let root = tmp_utf8(&dir);
        fs::write(root.join("a.txt").as_std_path(), b"a").unwrap();

        let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
        let h1 = hasher.hash_dir(root).unwrap();
        settle();

        fs::write(root.join("b.txt").as_std_path(), b"b").unwrap();
        settle();

        let h2 = hasher.hash_dir(root).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn dir_hash_delete_file() {
        let dir = tmp_dir();
        let root = tmp_utf8(&dir);
        let file = root.join("a.txt");
        fs::write(file.as_std_path(), b"a").unwrap();

        let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
        let h1 = hasher.hash_dir(root).unwrap();
        settle();

        fs::remove_file(file.as_std_path()).unwrap();
        settle();

        let h2 = hasher.hash_dir(root).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn manual_invalidation() {
        let dir = tmp_dir();
        let root = tmp_utf8(&dir);
        let file = root.join("test.txt");
        fs::write(file.as_std_path(), b"hello").unwrap();

        let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
        let h1 = hasher.hash_file(&file).unwrap();

        fs::write(file.as_std_path(), b"world").unwrap();
        hasher.invalidate(&file);

        let h2 = hasher.hash_file(&file).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn invalidate_all_clears() {
        let dir = tmp_dir();
        let root = tmp_utf8(&dir);
        fs::write(root.join("a.txt").as_std_path(), b"a").unwrap();

        let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
        hasher.hash_file(&root.join("a.txt")).unwrap();
        hasher.hash_dir(root).unwrap();
        assert_eq!(hasher.cached_count(), (1, 1));

        hasher.invalidate_all();
        assert_eq!(hasher.cached_count(), (0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dir_changes_detected() {
        let project = tmp_dir();
        let external = tmp_dir();
        let proj = tmp_utf8(&project);
        let ext = tmp_utf8(&external);

        fs::write(ext.join("lib.h").as_std_path(), b"int foo();").unwrap();
        std::os::unix::fs::symlink(ext.as_std_path(), proj.join("vendor").as_std_path())
            .unwrap();

        let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
        let h1 = hasher.hash_dir(proj).unwrap();
        settle();

        fs::write(ext.join("lib.h").as_std_path(), b"int bar();").unwrap();
        settle();

        let h2 = hasher.hash_dir(proj).unwrap();
        assert_ne!(h1, h2);
    }

    #[cfg(unix)]
    #[test]
    fn hash_file_through_symlink() {
        let project = tmp_dir();
        let external = tmp_dir();
        let proj = tmp_utf8(&project);
        let ext = tmp_utf8(&external);

        fs::write(ext.join("header.h").as_std_path(), b"original").unwrap();
        std::os::unix::fs::symlink(ext.as_std_path(), proj.join("inc").as_std_path())
            .unwrap();

        let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
        let h1 = hasher.hash_file(&proj.join("inc/header.h")).unwrap();
        settle();

        fs::write(ext.join("header.h").as_std_path(), b"modified").unwrap();
        settle();

        let h2 = hasher.hash_file(&proj.join("inc/header.h")).unwrap();
        assert_ne!(h1, h2);
    }
}

