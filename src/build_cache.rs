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

/// Bumped whenever the key layout changes, so old entries stop matching.
const KEY_SALT: &str = "anubis-action-key-v1/sha256";

/// Maximum header sets remembered per compile manifest.
const MAX_MANIFEST_ENTRIES: usize = 16;

// ----------------------------------------------------------------------------
// Digest
// ----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
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
    /// `(normalized path, sha256 hex)` for every file the compile read.
    pub inputs: Vec<(String, String)>,
    pub result_key: String,
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
    /// Copy a blob to `dst`. Returns false if the blob is missing.
    fn get_blob(&self, digest: &Digest, dst: &Utf8Path) -> Result<bool>;
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
        std::fs::rename(tmp, dst).with_context(|| format!("publishing {dst}"))?;
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

    fn get_blob(&self, digest: &Digest, dst: &Utf8Path) -> Result<bool> {
        let src = self.path("cas", digest);
        match std::fs::copy(&src, dst) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow::Error::from(e).context(format!("copying {src} to {dst}"))),
        }
    }

    fn put_blob(&self, digest: &Digest, src: &Utf8Path) -> Result<()> {
        let dst = self.path("cas", digest);
        if dst.exists() {
            return Ok(());
        }
        let tmp = self.tmp_path();
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
    tool_ids: DashMap<Utf8PathBuf, String>,
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
            tool_ids: DashMap::new(),
            stats: CacheStats::default(),
        }
    }

    /// Local cache in `<workspace>/.anubis-cache`.
    pub fn local(workspace_root: &Utf8Path) -> Result<Self> {
        let dir = workspace_root.join(".anubis-cache");
        Ok(Self::new(workspace_root, Box::new(LocalDiskBackend::new(dir)?)))
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
        let d = match Digest::of_file(path) {
            Ok(d) => Some(d),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(anyhow::Error::from(e).context(format!("hashing {path}"))),
        };
        self.digests.insert(path.to_owned(), d);
        Ok(d)
    }

    /// Forget a memoized digest. Call for every file Anubis writes.
    pub fn invalidate(&self, path: &Utf8Path) {
        self.digests.remove(path);
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
            s = s.replace(&out, "out:");
        }
        s.replace(&self.root_prefix, "ws:")
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
        let Ok(result) = serde_json::from_slice::<ActionResult>(&bytes) else {
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
            if !self.backend.get_blob(d, &tmp)? {
                return Ok(None);
            }
            set_executable(&tmp, o.executable)?;
            std::fs::rename(&tmp, &dst).with_context(|| format!("restoring {dst}"))?;
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
        self.backend.put_ac(key, &serde_json::to_vec(&result)?)
    }

    // ---- compile manifests -------------------------------------------------

    /// Find a previous compile whose inputs all still match. Returns its
    /// result key.
    pub fn lookup_manifest(&self, manifest_key: &Digest) -> Result<Option<Digest>> {
        let Some(bytes) = self.backend.get_ac(manifest_key)? else {
            return Ok(None);
        };
        let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
            return Ok(None);
        };
        'entries: for entry in &manifest.entries {
            for (path, digest) in &entry.inputs {
                let current = self.file_digest(&self.denormalize_path(path))?;
                if current.map(|d| d.hex()) != Some(digest.clone()) {
                    continue 'entries;
                }
            }
            return Ok(Digest::from_hex(&entry.result_key));
        }
        Ok(None)
    }

    /// Result key for a compile given the files it read.
    pub fn compile_result_key(
        &self,
        manifest_key: &Digest,
        inputs: &[Utf8PathBuf],
    ) -> Result<Option<(Digest, Vec<(String, String)>)>> {
        let mut kb = KeyBuilder::new("cc_compile_result");
        kb.digest(manifest_key);
        let mut recorded = Vec::with_capacity(inputs.len());
        for p in inputs {
            let Some(d) = self.file_digest(p)? else { return Ok(None) };
            let norm = self.normalize_path(p);
            kb.field(&norm);
            kb.digest(&d);
            recorded.push((norm, d.hex()));
        }
        Ok(Some((kb.finish(), recorded)))
    }

    /// Remember that `inputs` led to `result_key` (newest first, capped).
    pub fn record_manifest(
        &self,
        manifest_key: &Digest,
        inputs: Vec<(String, String)>,
        result_key: &Digest,
    ) -> Result<()> {
        let mut manifest = self
            .backend
            .get_ac(manifest_key)?
            .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
            .unwrap_or_default();
        let rk = result_key.hex();
        manifest.entries.retain(|e| e.result_key != rk);
        manifest.entries.insert(0, ManifestEntry { inputs, result_key: rk });
        manifest.entries.truncate(MAX_MANIFEST_ENTRIES);
        self.backend.put_ac(manifest_key, &serde_json::to_vec(&manifest)?)
    }

    pub fn record_hit(&self) {
        self.stats.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self) {
        self.stats.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn hits(&self) -> u64 {
        self.stats.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.stats.misses.load(Ordering::Relaxed)
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
