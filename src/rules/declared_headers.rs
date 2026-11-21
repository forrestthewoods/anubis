//! Declared headers: every file a compile reads must be declared.
//!
//! C/C++ targets list their headers (`public_headers`, `private_headers`, a
//! `cc_binary`'s `headers`), NASM targets theirs (`headers`, plus
//! `preincludes`). A compile can see its target's own headers and the
//! `public_headers` of every library it depends on, transitively.
//!
//! - **Cache key.** The sorted names of the visible headers, and whether
//!   each exists (a stat, not a read), go in the compile's manifest key as
//!   one digest per declared list: the target's own, and each visible
//!   library's `public_headers`, built once per run and shared (#226).
//!   Adding, removing or renaming a declared header, or creating a declared
//!   one that didn't exist yet, is a miss for every compile that can see
//!   it; that covers a new header that shadows another and a new
//!   `__has_include` target. Contents stay precise through the
//!   `.d` second level: editing a header only misses the compiles that read
//!   it.
//! - **`.d` check.** After each compile, every file in the `.d` must be the
//!   source, a visible declared header, or under one of the toolchain's
//!   `system_include_dirs`. Anything else (an undeclared header, a file
//!   outside the workspace such as `/usr/include/x.h`) fails the build.
//!   Paths are matched lexically (`..` folded), so a read through a symlink
//!   that leaves the workspace, e.g. `link/../x.h`, is checked as the path
//!   it spells. The cache still hashes the file the compiler actually read.
//!
//! Anubis never reads source or header files; only the compiler's `.d`.
//! Known gap without a sandbox (#209): an undeclared header created after a
//! result was cached can still hit, where a fresh compile would fail the
//! check. That never produces a wrong object, only a missed error.

use std::collections::HashSet;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;

use crate::anubis::AnubisTarget;
use crate::build_cache::{BuildCache, Digest, KeyBuilder};
use crate::util::lexical_path;
use crate::{anyhow_loc, bail_loc, function_name};

/// A header list is required whenever its include dirs are given.
pub fn require(
    target: &AnubisTarget,
    headers: &Option<Vec<Utf8PathBuf>>,
    headers_field: &str,
    dirs: &[Utf8PathBuf],
    dirs_field: &str,
) -> anyhow::Result<()> {
    if headers.is_none() && !dirs.is_empty() {
        bail_loc!(
            "{}: `{}` is set but `{}` is missing. List every header under those dirs (globs are fine, \
             `[]` if there are none): each compile's .d is checked against the declared headers.",
            target,
            dirs_field,
            headers_field
        );
    }
    Ok(())
}

/// One declared header list (a library's `public_headers`, a target's own
/// headers), processed once: a library's public list is built once per run
/// and shared by every target that can see it (see [`HeaderSet`]).
#[derive(Debug, Default)]
pub struct HeaderList {
    /// Lexical form of each path (see [`lexical_path`]), for the `.d` check.
    norm: HashSet<String>,
    /// Digest of the sorted names and whether each exists, for the cache key.
    /// `None` without a cache.
    digest: Option<Digest>,
    /// Whether every header existed when it was stat'ed. A list with a
    /// missing (e.g. not yet generated) header isn't shared, so a target
    /// built after the header appears keys it as present, as before.
    all_exist: bool,
}

impl HeaderList {
    /// With a cache, stats each header once (memoized for the run). A
    /// declared header that doesn't exist is allowed (e.g. one a rule
    /// generates), but it is keyed as missing: creating it is a miss.
    pub fn new<'a>(paths: impl IntoIterator<Item = &'a Utf8PathBuf>, cache: Option<&BuildCache>) -> Self {
        let mut norm = HashSet::new();
        let mut sorted = Vec::new();
        for p in paths {
            let n = lexical_path(p.as_str());
            if norm.insert(n.clone()) {
                sorted.push(Utf8PathBuf::from(n));
            }
        }
        sorted.sort();
        let mut all_exist = true;
        let digest = cache.map(|cache| {
            let mut kb = KeyBuilder::new("declared_headers");
            kb.field((sorted.len() as u64).to_le_bytes());
            for p in sorted {
                let exists = cache.is_file(&p);
                all_exist &= exists;
                kb.field(cache.normalize_path(&p));
                kb.field(if exists { b"1" } else { b"0" });
            }
            kb.finish()
        });
        HeaderList { norm, digest, all_exist }
    }

    pub fn is_empty(&self) -> bool {
        self.norm.is_empty()
    }

    pub fn all_exist(&self) -> bool {
        self.all_exist
    }
}

/// The headers a compile may read, as declared: the target's own lists and
/// the public list of every library it can see, in the deterministic order
/// the deps are walked. Built per target from shared lists, so its cost is
/// proportional to the number of visible libraries, not visible headers.
#[derive(Debug, Default, Clone)]
pub struct HeaderSet {
    lists: Vec<Arc<HeaderList>>,
}

impl HeaderSet {
    /// Empty lists are left out: they add nothing to the key or the check.
    pub fn new(lists: impl IntoIterator<Item = Arc<HeaderList>>) -> Self {
        HeaderSet { lists: lists.into_iter().filter(|l| !l.is_empty()).collect() }
    }

    /// One list from paths (NASM rules, tests).
    pub fn from_paths<'a>(paths: impl IntoIterator<Item = &'a Utf8PathBuf>, cache: Option<&BuildCache>) -> Self {
        Self::new([Arc::new(HeaderList::new(paths, cache))])
    }

    fn contains(&self, norm: &str) -> bool {
        self.lists.iter().any(|l| l.norm.contains(norm))
    }

    /// Add the header names and existence (not contents) to a compile's
    /// manifest key: each list's digest, sorted, so reordering deps changes
    /// nothing. The same headers split into lists differently, or listed by
    /// two libraries, make a different key (a spurious miss), never the
    /// same key for different headers.
    pub fn key_add(&self, kb: &mut KeyBuilder) -> anyhow::Result<()> {
        let mut digests = self
            .lists
            .iter()
            .map(|l| l.digest.as_ref().ok_or_else(|| anyhow_loc!("HeaderList built without a cache")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        digests.sort_by(|a, b| a.0.cmp(&b.0));
        kb.field("declared_headers-v2");
        kb.field((digests.len() as u64).to_le_bytes());
        for d in digests {
            kb.digest(d);
        }
        Ok(())
    }
}

/// Check a compile's `.d` file: every file listed must be `src`, a declared
/// header in `headers`, or under (or equal to: a toolchain's `system_files`)
/// one of `system_dirs` (the toolchain's declaration). Files outside the workspace are always an error. Matching
/// uses the lexical form (`..` folded); the returned paths, for the cache's
/// second level, are as the `.d` spells them (relative ones joined to `root`,
/// the compile's working directory), so the OS resolves `..` through
/// symlinks the way the compiler did.
pub fn check_depfile(
    dep_file: &Utf8Path,
    root: &Utf8Path,
    src: &Utf8Path,
    headers: &HeaderSet,
    system_dirs: &[Utf8PathBuf],
) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let content = std::fs::read_to_string(dep_file)
        .map_err(|e| anyhow_loc!("Failed to read dependency file {}: {}", dep_file, e))?;

    let root_n = format!("{}/", lexical_path(root.as_str()).trim_end_matches('/'));
    let src_n = lexical_path(src.as_str());
    let system_n: Vec<String> = system_dirs
        .iter()
        .map(|d| lexical_path(d.as_str()).trim_end_matches('/').to_owned())
        .collect();

    let mut read = Vec::new();
    let mut outside = Vec::new();
    let mut undeclared = Vec::new();
    for dep in crate::build_cache::parse_depfile(&content) {
        let p = Utf8Path::new(&dep);
        let abs = if p.is_absolute() { p.to_owned() } else { root.join(p) };
        let n = lexical_path(abs.as_str());
        if !n.starts_with(&root_n) {
            outside.push(n);
        } else if n == src_n
            || system_n.iter().any(|d| n == *d || n.strip_prefix(d.as_str()).is_some_and(|r| r.starts_with('/')))
            || headers.contains(&n)
        {
            read.push(abs);
        } else {
            undeclared.push(n);
        }
    }

    if !outside.is_empty() || !undeclared.is_empty() {
        let mut msg = format!("{}: the compile read files that aren't declared.", src);
        if !undeclared.is_empty() {
            msg += &format!(
                "\nNot in any visible header list:\n  {}\nAdd each to the `public_headers` or `private_headers` \
                 (a cc_binary's `headers`, a NASM rule's `headers`) of the target that owns it, and depend on \
                 that target.",
                undeclared.iter().join("\n  ")
            );
        }
        if !outside.is_empty() {
            msg += &format!(
                "\nOutside the workspace (builds may only read files under {root}):\n  {}",
                outside.iter().join("\n  ")
            );
        }
        bail_loc!("{}", msg);
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_cache::LocalDiskBackend;

    fn ws() -> (tempfile::TempDir, Utf8PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().join("ws")).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        (tmp, ws)
    }

    fn depfile(ws: &Utf8Path, deps: &[Utf8PathBuf]) -> Utf8PathBuf {
        let d = ws.join("main.d");
        let body = deps.iter().map(|p| p.as_str()).join(" \\\n  ");
        std::fs::write(&d, format!("{}: {}\n", ws.join("main.o"), body)).unwrap();
        d
    }

    #[test]
    fn declared_system_and_source_pass() {
        let (_t, ws) = ws();
        let src = ws.join("src/main.c");
        let hdr = ws.join("inc/a.h");
        let sys = ws.join("toolchains/llvm/include");
        let d = depfile(&ws, &[src.clone(), ws.join("src/../inc/a.h"), sys.join("stdio.h")]);
        let read = check_depfile(&d, &ws, &src, &HeaderSet::from_paths([&hdr], None), &[sys]).unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(read[1], ws.join("src/../inc/a.h"), "'..' is matched folded but returned as spelled");
    }

    #[test]
    fn undeclared_and_outside_fail() {
        let (_t, ws) = ws();
        let src = ws.join("main.c");
        let d = depfile(&ws, &[src.clone(), ws.join("inc/b.h")]);
        let err = check_depfile(&d, &ws, &src, &HeaderSet::from_paths([&ws.join("inc/a.h")], None), &[]).unwrap_err();
        assert!(err.to_string().contains("inc/b.h"), "{err}");

        let d = depfile(&ws, &[src.clone(), Utf8PathBuf::from("/usr/include/stdio.h")]);
        let err = check_depfile(&d, &ws, &src, &HeaderSet::default(), &[Utf8PathBuf::from("/usr/include")]).unwrap_err();
        assert!(err.to_string().contains("Outside the workspace"), "{err}");
    }

    #[test]
    fn system_file_matches_exactly() {
        let (_t, ws) = ws();
        let src = ws.join("main.c");
        let sdk = ws.join("toolchains/macos_sdk");
        let d = depfile(&ws, &[src.clone(), sdk.join("SDKSettings.json")]);
        check_depfile(&d, &ws, &src, &HeaderSet::default(), &[sdk.join("SDKSettings.json")]).unwrap();

        // An entry is not a bare string prefix: `inc` doesn't cover `include/x.h`.
        let d = depfile(&ws, &[src.clone(), ws.join("include/x.h")]);
        check_depfile(&d, &ws, &src, &HeaderSet::default(), &[ws.join("inc")]).unwrap_err();
    }

    #[test]
    fn key_changes_with_header_names_not_order() {
        let (t, ws) = ws();
        let backend = LocalDiskBackend::new(Utf8PathBuf::from_path_buf(t.path().join("cache")).unwrap()).unwrap();
        let cache = BuildCache::new(&ws, Box::new(backend));
        let key = |hs: &[Utf8PathBuf]| {
            let mut kb = KeyBuilder::new("t");
            HeaderSet::from_paths(hs, Some(&cache)).key_add(&mut kb).unwrap();
            kb.finish()
        };
        let (a, b) = (ws.join("a.h"), ws.join("b.h"));
        std::fs::write(&a, "").unwrap();
        assert_eq!(key(&[a.clone(), b.clone()]), key(&[b.clone(), a.clone(), a.clone()]));
        assert_ne!(key(&[a.clone()]), key(&[a.clone(), b.clone()]), "a new declared header is a miss");
        let before = key(&[a.clone(), b.clone()]);
        std::fs::write(&b, "").unwrap();
        cache.invalidate(&b);
        assert_ne!(before, key(&[a.clone(), b.clone()]), "creating a declared header is a miss");
    }

    /// A target sees its own list plus each visible library's shared list.
    /// A change in any of them (a header added to a library two levels down)
    /// changes the key; the `.d` check accepts a header from any of them.
    #[test]
    fn key_and_check_cover_every_visible_list() {
        let (t, ws) = ws();
        let backend = LocalDiskBackend::new(Utf8PathBuf::from_path_buf(t.path().join("cache")).unwrap()).unwrap();
        let cache = BuildCache::new(&ws, Box::new(backend));
        let list = |hs: &[Utf8PathBuf]| Arc::new(HeaderList::new(hs, Some(&cache)));
        let key = |lists: Vec<Arc<HeaderList>>| {
            let mut kb = KeyBuilder::new("t");
            HeaderSet::new(lists).key_add(&mut kb).unwrap();
            kb.finish()
        };
        let (own, mid, low, low2) = (ws.join("own.h"), ws.join("mid.h"), ws.join("low.h"), ws.join("low2.h"));
        let (l_own, l_mid, l_low) = (list(&[own.clone()]), list(&[mid.clone()]), list(&[low.clone()]));
        let before = key(vec![l_own.clone(), l_mid.clone(), l_low.clone()]);
        assert_eq!(before, key(vec![l_own.clone(), l_mid.clone(), l_low.clone()]));
        assert_ne!(before, key(vec![l_own.clone(), l_mid.clone(), list(&[low.clone(), low2.clone()])]), "header added two levels down");
        assert_ne!(before, key(vec![l_own.clone(), l_mid.clone()]), "a library no longer visible");
        assert_eq!(before, key(vec![l_low.clone(), l_own.clone(), l_mid.clone()]), "dep order doesn't matter");
        assert_eq!(before, key(vec![l_own.clone(), Arc::new(HeaderList::default()), l_mid.clone(), l_low.clone()]), "empty lists add nothing");

        let src = ws.join("main.c");
        let set = HeaderSet::new([l_own, l_mid, l_low]);
        let d = depfile(&ws, &[src.clone(), own, mid, low]);
        assert_eq!(check_depfile(&d, &ws, &src, &set, &[]).unwrap().len(), 4);
        let d = depfile(&ws, &[src.clone(), low2]);
        check_depfile(&d, &ws, &src, &set, &[]).unwrap_err();
    }

    #[test]
    fn header_list_required_with_include_dirs() {
        let t = AnubisTarget::new("//x:y").unwrap();
        let dirs = [Utf8PathBuf::from("inc")];
        assert!(require(&t, &None, "public_headers", &dirs, "public_include_dirs").is_err());
        assert!(require(&t, &Some(vec![]), "public_headers", &dirs, "public_include_dirs").is_ok());
        assert!(require(&t, &None, "public_headers", &[], "public_include_dirs").is_ok());
    }
}
