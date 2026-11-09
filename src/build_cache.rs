//! Content-addressed build cache.
//!
//! Vocabulary follows Bazel's Remote Execution API so that a remote back end
//! can slot in behind [`CacheBackend`] later without changing the keys:
//!
//! - **CAS** (content-addressable storage): output blobs keyed by SHA-256.
//! - **AC** (action cache): `ActionKey -> ActionResult`, where the result
//!   lists each output's name and CAS digest.
//!
//! Keys never contain machine-specific data. Paths under the workspace root
//! are written as `ws:<relative path>`, each action's own output directory
//! becomes `out:`, tools are identified by their Natron install fingerprint
//! (or by content when they are not Natron-installed), and every input file
//! is identified by its SHA-256. The environment is always empty (Anubis
//! clears it before building), so it is not part of the key.
//!
//! C/C++ compiles use two levels, like ccache's direct mode: a *manifest key*
//! over tool + argv + source digest maps to a list of header sets seen on
//! previous compiles (from the compiler's `.d` file); a matching set yields
//! the *result key* that names the object in the action cache.

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::util::SlashFix;

/// Bumped whenever the key layout or the entry encoding changes, so old
/// entries stop matching. v2: entries are MessagePack, not JSON.
const KEY_SALT: &str = "anubis-action-key-v2/sha256";

/// Maximum header sets remembered per compile manifest.
const MAX_MANIFEST_ENTRIES: usize = 16;

// ----------------------------------------------------------------------------
// Digest
// ----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Digest(pub [u8; 32]);

/// Serialized as 32 raw bytes (MessagePack `bin`), not hex.
impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Digest;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("32 digest bytes")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> std::result::Result<Digest, E> {
                <[u8; 32]>::try_from(v).map(Digest).map_err(|_| E::invalid_length(v.len(), &self))
            }
        }
        d.deserialize_bytes(V)
    }
}

/// Action cache entries are MessagePack: compact, and much faster to parse
/// than JSON (a no-op superdex build reads ~1,200 manifests).
fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    Ok(rmp_serde::to_vec_named(v)?)
}

fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    rmp_serde::from_slice(bytes).ok()
}

impl Digest {
    pub fn hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0xf) as usize] as char);
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<Digest> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out[i] = (hi * 16 + lo) as u8;
        }
        Some(Digest(out))
    }

    pub fn of_bytes(bytes: &[u8]) -> Digest {
        Digest(Sha256::digest(bytes).into())
    }

    pub fn of_file(path: &Utf8Path) -> std::io::Result<Digest> {
        let mut f = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(Digest(hasher.finalize().into()))
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

/// Builds a key from length-prefixed fields, so field boundaries can never
/// be confused (`["ab", "c"]` and `["a", "bc"]` hash differently).
pub struct KeyBuilder(Sha256);

impl KeyBuilder {
    pub fn new(kind: &str) -> Self {
        let mut kb = KeyBuilder(Sha256::new());
        kb.field(KEY_SALT);
        kb.field(kind);
        kb
    }

    pub fn field(&mut self, s: impl AsRef<[u8]>) -> &mut Self {
        let b = s.as_ref();
        self.0.update((b.len() as u64).to_le_bytes());
        self.0.update(b);
        self
    }

    pub fn digest(&mut self, d: &Digest) -> &mut Self {
        self.field(d.0)
    }

    pub fn finish(self) -> Digest {
        Digest(self.0.finalize().into())
    }
}

// ----------------------------------------------------------------------------
// Stored records
// ----------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    /// Output file name relative to the action's output directory.
    pub name: String,
    pub digest: String,
    pub size: u64,
    pub executable: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionResult {
    pub outputs: Vec<OutputFile>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// `(normalized path, sha256)` for every file the compile read.
    pub inputs: Vec<(String, Digest)>,
    pub result_key: Digest,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub entries: Vec<ManifestEntry>,
}

// ----------------------------------------------------------------------------
// Back end
// ----------------------------------------------------------------------------

/// Storage for the action cache and CAS. The local disk implementation is
/// the only one today; a bazel-remote (HTTP) or REAPI (gRPC) client
/// implements the same five calls.
pub trait CacheBackend: Send + Sync + std::fmt::Debug {
    fn get_ac(&self, key: &Digest) -> Result<Option<Vec<u8>>>;
    fn put_ac(&self, key: &Digest, value: &[u8]) -> Result<()>;
    fn has_blob(&self, digest: &Digest) -> Result<bool>;
    /// Materialize a blob at `dst` (which must not exist) with the given
    /// executable bit. Returns false if the blob is missing.
    fn get_blob(&self, digest: &Digest, dst: &Utf8Path, executable: bool) -> Result<bool>;
    fn put_blob(&self, digest: &Digest, src: &Utf8Path) -> Result<()>;
}

/// Plain files under one directory: `ac/<ab>/<key>` and `cas/<ab>/<digest>`.
/// Every write is temp-file-then-rename, so readers never see a partial
/// entry and concurrent writers of the same content are harmless.
#[derive(Debug)]
pub struct LocalDiskBackend {
    root: Utf8PathBuf,
}

impl LocalDiskBackend {
    pub fn new(root: Utf8PathBuf) -> Result<Self> {
        for sub in ["ac", "cas", "tmp"] {
            std::fs::create_dir_all(root.join(sub))
                .with_context(|| format!("creating {}", root.join(sub)))?;
        }
        Ok(Self { root })
    }

    fn path(&self, kind: &str, d: &Digest) -> Utf8PathBuf {
        let hex = d.hex();
        self.root.join(kind).join(&hex[..2]).join(hex)
    }

    fn tmp_path(&self) -> Utf8PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        self.root
            .join("tmp")
            .join(format!("{}-{}", std::process::id(), n))
    }

    fn publish(&self, tmp: &Utf8Path, dst: &Utf8Path) -> Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Err(e) = std::fs::rename(tmp, dst) {
            let _ = std::fs::remove_file(tmp);
            // On Windows a rename onto a file another process has open
            // fails. If someone else already published this key, keep theirs.
            if !dst.exists() {
                return Err(anyhow::Error::from(e).context(format!("publishing {dst}")));
            }
        }
        Ok(())
    }
}

impl CacheBackend for LocalDiskBackend {
    fn get_ac(&self, key: &Digest) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.path("ac", key)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn put_ac(&self, key: &Digest, value: &[u8]) -> Result<()> {
        let tmp = self.tmp_path();
        std::fs::write(&tmp, value)?;
        self.publish(&tmp, &self.path("ac", key))
    }

    fn has_blob(&self, digest: &Digest) -> Result<bool> {
        Ok(self.path("cas", digest).exists())
    }

    fn get_blob(&self, digest: &Digest, dst: &Utf8Path, executable: bool) -> Result<bool> {
        let src = self.path("cas", digest);
        // Hard link when the blob already has the wanted mode; the output
        // then shares the blob's read-only inode (see `put_blob`).
        if HARD_LINKS {
            match std::fs::metadata(&src) {
                Ok(m) if is_executable(&m) == executable => {
                    if std::fs::hard_link(&src, dst).is_ok() {
                        return Ok(true);
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(anyhow::Error::from(e).context(format!("reading {src}"))),
            }
        }
        match std::fs::copy(&src, dst) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(anyhow::Error::from(e).context(format!("copying {src} to {dst}"))),
        }
        set_executable(dst, executable)?;
        Ok(true)
    }

    fn put_blob(&self, digest: &Digest, src: &Utf8Path) -> Result<()> {
        let dst = self.path("cas", digest);
        if dst.exists() {
            return Ok(());
        }
        let tmp = self.tmp_path();
        // Hard link instead of copying. The blob and the output become one
        // file, so both are made read-only: anything that edits the output
        // in place (strip, code signing) then fails loudly instead of
        // silently changing the cached blob. Compilers, linkers and ar write
        // a new file and rename it over the output, which is unaffected.
        if HARD_LINKS {
            let original = std::fs::metadata(src)
                .with_context(|| format!("reading {src}"))?
                .permissions();
            set_read_only(src)?;
            if std::fs::hard_link(src, &tmp).is_ok() {
                return self.publish(&tmp, &dst);
            }
            // Different filesystem or no hard links: copy instead.
            std::fs::set_permissions(src, original)?;
        }
        std::fs::copy(src, &tmp).with_context(|| format!("copying {src} into the cache"))?;
        // Verify what we copied is what we keyed, in case `src` changed
        // underneath us.
        if Digest::of_file(&tmp)? != *digest {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!("{src} changed while it was being cached");
        }
        self.publish(&tmp, &dst)
    }
}

// ----------------------------------------------------------------------------
// BuildCache
// ----------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct CacheStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
}

#[derive(Debug)]
pub struct BuildCache {
    root: Utf8PathBuf,
    root_prefix: String,
    backend: Box<dyn CacheBackend>,
    /// Per-run memo of file digests (None = file missing). Files Anubis
    /// writes are invalidated explicitly via [`BuildCache::invalidate`].
    digests: DashMap<Utf8PathBuf, Option<Digest>>,
    /// Same memo keyed by the normalized (`ws:`) manifest spelling, with a
    /// fast hasher: a no-op superdex build checks ~1.4M manifest inputs.
    norm_digests: DashMap<String, Option<Digest>, rustc_hash::FxBuildHasher>,
    tool_ids: DashMap<Utf8PathBuf, String>,
    /// Per-run memo: is this path a file? Keyed like `norm_digests`.
    files: DashMap<String, bool, rustc_hash::FxBuildHasher>,
    /// Per-run memo: is this dir inside a Natron install (immutable)?
    natron_dirs: DashMap<Utf8PathBuf, bool>,
    /// Persisted `path -> (stat, digest)` so warm builds only rehash files
    /// whose stat changed. None when the cache has no directory (tests).
    stat_cache: Option<StatCache>,
    pub stats: CacheStats,
}

/// A file an action produces. `optional` outputs (e.g. a PDB) are captured
/// when present.
pub struct ActionOutput {
    pub path: Utf8PathBuf,
    pub optional: bool,
}

impl BuildCache {
    pub fn new(workspace_root: &Utf8Path, backend: Box<dyn CacheBackend>) -> Self {
        let root = workspace_root.to_owned().slash_fix();
        let root_prefix = format!("{}/", root.as_str().trim_end_matches('/'));
        Self {
            root,
            root_prefix,
            backend,
            digests: DashMap::new(),
            norm_digests: DashMap::default(),
            tool_ids: DashMap::new(),
            files: DashMap::default(),
            natron_dirs: DashMap::new(),
            stat_cache: None,
            stats: CacheStats::default(),
        }
    }

    /// Local cache in `<workspace>/.anubis-cache`, with a persisted stat
    /// cache. Call [`save`](Self::save) after the build.
    pub fn local(workspace_root: &Utf8Path) -> Result<Self> {
        let dir = workspace_root.join(".anubis-cache");
        let mut cache = Self::new(workspace_root, Box::new(LocalDiskBackend::new(dir.clone())?));
        cache.stat_cache = Some(StatCache::load(dir.join("statcache")));
        Ok(cache)
    }

    /// Use a persisted stat cache at `path` (loaded now, written by `save`).
    pub fn with_stat_cache(mut self, path: Utf8PathBuf) -> Self {
        self.stat_cache = Some(StatCache::load(path));
        self
    }

    /// Persist the stat cache. Errors are logged, not fatal: it is only an
    /// accelerator.
    pub fn save(&self) {
        if let Some(sc) = &self.stat_cache {
            if let Err(e) = sc.save() {
                tracing::warn!("could not save stat cache: {e:#}");
            }
        }
    }

    pub fn workspace_root(&self) -> &Utf8Path {
        &self.root
    }

    // ---- digests -----------------------------------------------------------

    /// SHA-256 of a file, memoized for the rest of the run.
    pub fn file_digest(&self, path: &Utf8Path) -> Result<Option<Digest>> {
        if let Some(d) = self.digests.get(path) {
            return Ok(*d);
        }
        let d = match &self.stat_cache {
            Some(sc) => sc.digest(path)?,
            None => match Digest::of_file(path) {
                Ok(d) => Some(d),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(anyhow::Error::from(e).context(format!("hashing {path}"))),
            },
        };
        self.digests.insert(path.to_owned(), d);
        Ok(d)
    }

    /// Forget a memoized digest. Call for every file Anubis writes.
    pub fn invalidate(&self, path: &Utf8Path) {
        self.digests.remove(path);
        self.norm_digests.remove(&self.normalize_path(path));
        self.files.remove(&self.normalize_path(path));
    }

    // ---- file existence --------------------------------------------------

    /// Whether `path` is a file, memoized for the run (dropped by
    /// [`invalidate`](Self::invalidate)).
    pub fn is_file(&self, path: &Utf8Path) -> bool {
        self.is_file_norm(&self.normalize_path(path))
    }

    fn is_file_norm(&self, norm: &str) -> bool {
        if let Some(b) = self.files.get(norm) {
            return *b;
        }
        let b = self.denormalize_path(norm).is_file();
        self.files.insert(norm.to_owned(), b);
        b
    }

    /// Whether `dir` is inside a Natron install (`installs-v2/<fp>/tree/...`
    /// outside the workspace), whose contents never change. Memoized for the
    /// run. A toolchain deployed with `deploy_mode = "copy"` isn't one.
    pub fn is_natron_dir(&self, dir: &Utf8Path) -> bool {
        if let Some(b) = self.natron_dirs.get(dir) {
            return *b;
        }
        // Outside the workspace, so a project dir that happens to be named
        // like an install is never taken for one.
        let root = std::fs::canonicalize(&self.root).ok();
        let b = std::fs::canonicalize(dir).is_ok_and(|c| {
            let parts: Vec<_> = c.components().map(|p| p.as_os_str().to_owned()).collect();
            !root.as_ref().is_some_and(|r| c.starts_with(r))
                && parts.windows(3).any(|w| w[0] == natron::cache::INSTALLS_DIR && w[2] == "tree")
        });
        self.natron_dirs.insert(dir.to_owned(), b);
        b
    }

    // ---- normalization -----------------------------------------------------

    /// Rewrite every occurrence of the workspace root to `ws:` and of
    /// `out_dir` (if given) to `out:`, so keys match across checkouts and
    /// machines. Works inside composite args like `-I<path>` or
    /// `/LIBPATH:<path>`.
    pub fn normalize_arg(&self, arg: &str, out_dir: Option<&Utf8Path>) -> String {
        let mut s = arg.replace('\\', "/");
        if let Some(out) = out_dir {
            let out = format!("{}/", out.to_owned().slash_fix().as_str().trim_end_matches('/'));
            s = replace_all(&s, &out, "out:", PATHS_CASE_INSENSITIVE);
        }
        replace_all(&s, &self.root_prefix, "ws:", PATHS_CASE_INSENSITIVE)
    }

    /// Normalized form of a path used in manifests (`ws:` relative when it
    /// is under the workspace).
    pub fn normalize_path(&self, p: &Utf8Path) -> String {
        self.normalize_arg(p.as_str(), None)
    }

    fn denormalize_path(&self, s: &str) -> Utf8PathBuf {
        match s.strip_prefix("ws:") {
            Some(rel) => self.root.join(rel),
            None => Utf8PathBuf::from(s),
        }
    }

    /// Identity of a tool binary. Natron installs are immutable per
    /// fingerprint, so a tool inside one is `natron:<fingerprint>/<path>`
    /// (no hashing of a 100 MB clang). Anything else is identified by
    /// content.
    pub fn tool_identity(&self, tool: &Utf8Path) -> Result<String> {
        if let Some(id) = self.tool_ids.get(tool) {
            return Ok(id.clone());
        }
        let canonical = std::fs::canonicalize(tool)
            .with_context(|| format!("resolving tool {tool}"))?;
        let canonical = canonical.to_string_lossy().replace('\\', "/");
        let marker = format!("/{}/", natron::cache::INSTALLS_DIR);
        let id = match canonical.find(&marker) {
            Some(i) => {
                let rest = &canonical[i + marker.len()..];
                // rest = <fingerprint>/tree/<path inside install>
                format!("natron:{}", rest.replacen("/tree/", "/", 1))
            }
            None => {
                let d = self
                    .file_digest(tool)?
                    .ok_or_else(|| anyhow::anyhow!("tool {tool} does not exist"))?;
                format!("sha256:{d}")
            }
        };
        self.tool_ids.insert(tool.to_owned(), id.clone());
        Ok(id)
    }

    /// Start a key with the tool identity and normalized argv.
    pub fn key_for_command(
        &self,
        kind: &str,
        tool: &Utf8Path,
        args: &[String],
        out_dir: Option<&Utf8Path>,
    ) -> Result<KeyBuilder> {
        let mut kb = KeyBuilder::new(kind);
        kb.field(self.tool_identity(tool)?);
        kb.field((args.len() as u64).to_le_bytes());
        for a in args {
            kb.field(self.normalize_arg(a, out_dir));
        }
        Ok(kb)
    }

    /// Add `(normalized path, digest)` for each input file, in order.
    /// Returns false if an input is missing.
    pub fn key_add_files(&self, kb: &mut KeyBuilder, files: &[Utf8PathBuf]) -> Result<bool> {
        kb.field((files.len() as u64).to_le_bytes());
        for f in files {
            let Some(d) = self.file_digest(f)? else {
                return Ok(false);
            };
            kb.field(self.normalize_path(f));
            kb.digest(&d);
        }
        Ok(true)
    }

    // ---- action results ----------------------------------------------------

    /// Restore an action's outputs from the cache. Returns true on a hit.
    pub fn restore(&self, key: &Digest, out_dir: &Utf8Path) -> Result<bool> {
        Ok(self.restore_outputs(key, out_dir)?.is_some())
    }

    /// Like [`restore`](Self::restore), but returns the restored paths in
    /// the order they were stored.
    pub fn restore_outputs(&self, key: &Digest, out_dir: &Utf8Path) -> Result<Option<Vec<Utf8PathBuf>>> {
        let Some(bytes) = self.backend.get_ac(key)? else {
            return Ok(None);
        };
        let Some(result) = decode::<ActionResult>(&bytes) else {
            tracing::warn!("ignoring unreadable action cache entry {key}");
            return Ok(None);
        };
        // Check every blob exists before touching the output dir, so a
        // partial hit never leaves a mix of old and new outputs.
        let mut digests = Vec::with_capacity(result.outputs.len());
        for o in &result.outputs {
            let Some(d) = Digest::from_hex(&o.digest) else { return Ok(None) };
            if !self.backend.has_blob(&d)? {
                return Ok(None);
            }
            digests.push(d);
        }
        let mut restored = Vec::with_capacity(result.outputs.len());
        for (o, d) in result.outputs.iter().zip(&digests) {
            let dst = out_dir.join(&o.name);
            restored.push(dst.clone());
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Already in place (common for no-op rebuilds): skip the copy.
            if std::fs::metadata(&dst).map(|m| m.len() == o.size).unwrap_or(false)
                && self.file_digest(&dst)? == Some(*d)
            {
                continue;
            }
            let file_name = dst.file_name().unwrap_or("out");
            let tmp = dst.with_file_name(format!(".{file_name}.anubis-restore"));
            let _ = std::fs::remove_file(&tmp);
            if !self.backend.get_blob(d, &tmp, o.executable)? {
                return Ok(None);
            }
            // Windows can't rename over a read-only file (an older restore).
            if cfg!(windows) {
                remove_outputs([dst.as_path()])?;
            }
            std::fs::rename(&tmp, &dst).with_context(|| format!("restoring {dst}"))?;
            self.invalidate(&dst);
            if let Some(sc) = &self.stat_cache {
                sc.record(&dst, *d);
            }
            self.digests.insert(dst, Some(*d));
        }
        Ok(Some(restored))
    }

    /// Store the outputs of a successful action.
    pub fn store(&self, key: &Digest, out_dir: &Utf8Path, outputs: &[ActionOutput]) -> Result<()> {
        let mut result = ActionResult::default();
        for o in outputs {
            self.invalidate(&o.path);
            let meta = match std::fs::metadata(&o.path) {
                Ok(m) => m,
                Err(_) if o.optional => continue,
                Err(e) => return Err(anyhow::Error::from(e).context(format!("output {}", o.path))),
            };
            let name = o
                .path
                .strip_prefix(out_dir)
                .with_context(|| format!("output {} is not under {out_dir}", o.path))?
                .as_str()
                .replace('\\', "/");
            let d = self
                .file_digest(&o.path)?
                .ok_or_else(|| anyhow::anyhow!("output {} vanished", o.path))?;
            self.backend.put_blob(&d, &o.path)?;
            result.outputs.push(OutputFile {
                name,
                digest: d.hex(),
                size: meta.len(),
                executable: is_executable(&meta),
            });
        }
        self.backend.put_ac(key, &encode(&result)?)
    }

    // ---- compile manifests -------------------------------------------------

    /// Find a previous compile whose inputs all still match. Returns its
    /// result key.
    pub fn lookup_manifest(&self, manifest_key: &Digest) -> Result<Option<Digest>> {
        let Some(bytes) = self.backend.get_ac(manifest_key)? else {
            return Ok(None);
        };
        let Some(manifest) = decode::<Manifest>(&bytes) else {
            return Ok(None);
        };
        'entries: for entry in &manifest.entries {
            for (path, digest) in &entry.inputs {
                let current = match self.norm_digests.get(path.as_str()) {
                    Some(d) => *d,
                    None => {
                        let d = self.file_digest(&self.denormalize_path(path))?;
                        self.norm_digests.insert(path.clone(), d);
                        d
                    }
                };
                if current != Some(*digest) {
                    continue 'entries;
                }
            }
            return Ok(Some(entry.result_key));
        }
        Ok(None)
    }

    /// Result key for a compile given the files it read.
    pub fn compile_result_key(
        &self,
        manifest_key: &Digest,
        inputs: &[Utf8PathBuf],
    ) -> Result<Option<(Digest, Vec<(String, Digest)>)>> {
        let mut kb = KeyBuilder::new("cc_compile_result");
        kb.digest(manifest_key);
        let mut recorded = Vec::with_capacity(inputs.len());
        for p in inputs {
            let Some(d) = self.file_digest(p)? else { return Ok(None) };
            let norm = self.normalize_path(p);
            kb.field(&norm);
            kb.digest(&d);
            recorded.push((norm, d));
        }
        Ok(Some((kb.finish(), recorded)))
    }

    /// Remember that `inputs` led to `result_key` (newest first, capped).
    pub fn record_manifest(
        &self,
        manifest_key: &Digest,
        inputs: Vec<(String, Digest)>,
        result_key: &Digest,
    ) -> Result<()> {
        let mut manifest = self
            .backend
            .get_ac(manifest_key)?
            .and_then(|b| decode::<Manifest>(&b))
            .unwrap_or_default();
        let rk = *result_key;
        manifest.entries.retain(|e| e.result_key != rk);
        manifest.entries.insert(0, ManifestEntry { inputs, result_key: rk });
        manifest.entries.truncate(MAX_MANIFEST_ENTRIES);
        self.backend.put_ac(manifest_key, &encode(&manifest)?)
    }

    pub fn record_hit(&self) {
        self.stats.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self) {
        self.stats.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// `(files rehashed, files whose digest came from the stat cache)`.
    pub fn stat_counts(&self) -> (u64, u64) {
        match &self.stat_cache {
            Some(sc) => (sc.rehashed.load(Ordering::Relaxed), sc.reused.load(Ordering::Relaxed)),
            None => (0, 0),
        }
    }

    pub fn hits(&self) -> u64 {
        self.stats.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.stats.misses.load(Ordering::Relaxed)
    }
}

/// Windows (and default macOS) filesystems are case-insensitive, and the
/// same file can be spelled `C:/Work/anubis` in one place and
/// `c:/work/anubis` in another (the `.d` paths are lowercased). Match path
/// prefixes case-insensitively there so keys stay `ws:`-relative.
const PATHS_CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// Replace every occurrence of `needle` in `haystack`, optionally ignoring
/// ASCII case. The rest of the string keeps its original spelling.
pub fn replace_all(haystack: &str, needle: &str, with: &str, case_insensitive: bool) -> String {
    if !case_insensitive {
        return haystack.replace(needle, with);
    }
    if needle.is_empty() {
        return haystack.to_owned();
    }
    let hay_l = haystack.to_ascii_lowercase();
    let needle_l = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut last = 0;
    for (i, _) in hay_l.match_indices(&needle_l) {
        if i < last {
            continue;
        }
        out.push_str(&haystack[last..i]);
        out.push_str(with);
        last = i + needle.len();
    }
    out.push_str(&haystack[last..]);
    out
}

// ----------------------------------------------------------------------------
// Stat cache
// ----------------------------------------------------------------------------

/// Identity of a file's current contents as cheaply observable via `stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
    pub mtime_ns: i128,
    /// ctime on Unix (changes on any write or rename-over), creation time on
    /// Windows.
    pub ctime_ns: i128,
    pub dev: u64,
    pub ino: u64,
}

impl FileStat {
    pub fn of(meta: &std::fs::Metadata) -> FileStat {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            FileStat {
                size: meta.size(),
                mtime_ns: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128,
                ctime_ns: meta.ctime() as i128 * 1_000_000_000 + meta.ctime_nsec() as i128,
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let ns = |t: std::io::Result<std::time::SystemTime>| -> i128 {
                t.ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as i128)
                    .unwrap_or(0)
            };
            FileStat {
                size: meta.len(),
                mtime_ns: ns(meta.modified()),
                ctime_ns: ns(meta.created()),
                dev: 0,
                ino: 0,
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct StatEntry {
    stat: FileStat,
    digest: Digest,
    /// Wall-clock time the digest was computed.
    recorded_ns: i128,
}

/// A file modified less than this long before it was hashed may change
/// again without its mtime changing (coarse timestamps: FAT 2s, some
/// network filesystems 1s), so its entry is not trusted.
const RACY_WINDOW_NS: i128 = 2_000_000_000;

/// Persisted `path -> (stat, sha256)`, like git's index or Bazel's digest
/// cache. An entry is reused only when the file's current stat matches
/// exactly and the entry is not "racily clean" (the file's mtime is safely
/// older than when it was hashed). Anything else rehashes. Entries are never
/// trusted without a stat, so edits between runs are always seen; no file
/// watcher is needed.
pub struct StatCache {
    path: Utf8PathBuf,
    entries: DashMap<Utf8PathBuf, StatEntry>,
    dirty: std::sync::atomic::AtomicBool,
    pub rehashed: AtomicU64,
    pub reused: AtomicU64,
}

impl std::fmt::Debug for StatCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatCache")
            .field("path", &self.path)
            .field("entries", &self.entries.len())
            .finish()
    }
}

const STAT_CACHE_HEADER: &str = "anubis-statcache-v1";

fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

impl StatCache {
    /// Load from `path`. A missing or unreadable file gives an empty cache.
    pub fn load(path: Utf8PathBuf) -> StatCache {
        let entries = DashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut lines = text.lines();
            if lines.next() == Some(STAT_CACHE_HEADER) {
                for line in lines {
                    if let Some((p, e)) = Self::parse_line(line) {
                        entries.insert(p, e);
                    }
                }
            }
        }
        StatCache {
            path,
            entries,
            dirty: std::sync::atomic::AtomicBool::new(false),
            rehashed: AtomicU64::new(0),
            reused: AtomicU64::new(0),
        }
    }

    // size mtime ctime dev ino recorded digest path
    fn parse_line(line: &str) -> Option<(Utf8PathBuf, StatEntry)> {
        let mut it = line.splitn(8, ' ');
        let size = it.next()?.parse().ok()?;
        let mtime_ns = it.next()?.parse().ok()?;
        let ctime_ns = it.next()?.parse().ok()?;
        let dev = it.next()?.parse().ok()?;
        let ino = it.next()?.parse().ok()?;
        let recorded_ns = it.next()?.parse().ok()?;
        let digest = Digest::from_hex(it.next()?)?;
        let path = Utf8PathBuf::from(it.next()?);
        Some((
            path,
            StatEntry {
                stat: FileStat { size, mtime_ns, ctime_ns, dev, ino },
                digest,
                recorded_ns,
            },
        ))
    }

    /// Digest of `path`, from the cache when its stat is unchanged.
    pub fn digest(&self, path: &Utf8Path) -> Result<Option<Digest>> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow::Error::from(e).context(format!("stat {path}"))),
        };
        let stat = FileStat::of(&meta);
        if let Some(e) = self.entries.get(path) {
            if e.stat == stat && stat.mtime_ns + RACY_WINDOW_NS < e.recorded_ns {
                self.reused.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(e.digest));
            }
        }
        let recorded_ns = now_ns();
        let digest = match Digest::of_file(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow::Error::from(e).context(format!("hashing {path}"))),
        };
        // If the file changed while we hashed it, don't remember the result.
        let after = std::fs::metadata(path).map(|m| FileStat::of(&m)).ok();
        if after == Some(stat) {
            self.entries.insert(path.to_owned(), StatEntry { stat, digest, recorded_ns });
            self.dirty.store(true, Ordering::Relaxed);
        }
        self.rehashed.fetch_add(1, Ordering::Relaxed);
        Ok(Some(digest))
    }

    /// Remember `digest` for a file whose content Anubis already knows (an
    /// output it just restored from the cache), so the next run needn't
    /// hash it.
    pub fn record(&self, path: &Utf8Path, digest: Digest) {
        if let Ok(meta) = std::fs::metadata(path) {
            let recorded_ns = now_ns();
            self.entries.insert(path.to_owned(), StatEntry { stat: FileStat::of(&meta), digest, recorded_ns });
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Write the cache (temp file + rename) if anything changed.
    pub fn save(&self) -> Result<()> {
        if !self.dirty.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut out = String::with_capacity(self.entries.len() * 160);
        out.push_str(STAT_CACHE_HEADER);
        out.push('\n');
        for e in self.entries.iter() {
            let (p, v) = (e.key(), e.value());
            if p.as_str().contains('\n') {
                continue;
            }
            let s = &v.stat;
            out.push_str(&format!(
                "{} {} {} {} {} {} {} {}\n",
                s.size, s.mtime_ns, s.ctime_ns, s.dev, s.ino, v.recorded_ns, v.digest, p
            ));
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
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

/// Share cache blobs with outputs via hard links (falling back to copies
/// where links aren't possible). Shared files are read-only, so before an
/// action runs its old outputs are deleted with [`remove_outputs`]: Windows
/// can't rename over a read-only file, and a tool that writes in place
/// must not reach the blob.
const HARD_LINKS: bool = true;

/// Delete an action's existing outputs before running it, so the tool
/// writes fresh files instead of replacing (or editing) files that may be
/// read-only hard links to cache blobs. On Windows `remove_file` deletes
/// read-only files without clearing the attribute, which the blob shares.
pub fn remove_outputs<'a>(paths: impl IntoIterator<Item = &'a Utf8Path>) -> Result<()> {
    for p in paths {
        match std::fs::remove_file(p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::Error::from(e).context(format!("removing old output {p}"))),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_read_only(path: &Utf8Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o222))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_read_only(path: &Utf8Path) -> Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Utf8Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(path: &Utf8Path, _executable: bool) -> Result<()> {
    // CAS blobs may be read-only; restored outputs must be writable so the
    // next real build can overwrite them.
    let mut perms = std::fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

// ----------------------------------------------------------------------------
// Makefile dependency (.d) parsing
// ----------------------------------------------------------------------------

/// Parse the prerequisites out of a compiler-generated `.d` file.
///
/// Handles `\`-newline continuations, `\ ` escaped spaces, `\#`, `$$`, and
/// Windows drive letters (`C:\...`) in both the target and prerequisites.
pub fn parse_depfile(content: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = content.chars().peekable();
    let mut seen_colon = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek().copied() {
                Some('\n') => {
                    chars.next();
                    flush(&mut cur, &mut tokens, seen_colon);
                }
                Some('\r') => {
                    chars.next();
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    flush(&mut cur, &mut tokens, seen_colon);
                }
                Some(' ') | Some('#') => {
                    cur.push(chars.next().unwrap());
                }
                _ => cur.push('\\'),
            },
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                cur.push('$');
            }
            ':' if !seen_colon => {
                // A drive letter (`C:` followed by a separator) is part of
                // the path, not the rule separator.
                let is_drive = cur.len() == 1
                    && cur.chars().all(|ch| ch.is_ascii_alphabetic())
                    && matches!(chars.peek(), Some('/') | Some('\\'));
                if is_drive {
                    cur.push(':');
                } else {
                    cur.clear(); // the target name
                    seen_colon = true;
                }
            }
            ' ' | '\t' | '\n' | '\r' => flush(&mut cur, &mut tokens, seen_colon),
            _ => cur.push(c),
        }
    }
    flush(&mut cur, &mut tokens, seen_colon);
    tokens
}

fn flush(cur: &mut String, tokens: &mut Vec<String>, seen_colon: bool) {
    if !cur.is_empty() {
        if seen_colon {
            tokens.push(std::mem::take(cur));
        } else {
            cur.clear(); // one of several targets
        }
    }
}

#[cfg(test)]
#[path = "tests/build_cache_tests.rs"]
mod tests;
