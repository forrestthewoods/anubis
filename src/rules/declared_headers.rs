//! Declared headers: every file a compile reads must be declared.
//!
//! C/C++ targets list their headers (`public_headers`, `private_headers`, a
//! `cc_binary`'s `headers`), NASM targets theirs (`headers`, plus
//! `preincludes`). A compile can see its target's own headers and the
//! `public_headers` of every library it depends on, transitively.
//!
//! - **Cache key.** The sorted names of the visible headers go in the
//!   compile's manifest key. Adding, removing or renaming a declared header
//!   is a miss for every compile that can see it; that covers a new header
//!   that shadows another and a new `__has_include` target. Contents stay
//!   precise through the `.d` second level: editing a header only misses the
//!   compiles that read it.
//! - **`.d` check.** After each compile, every file in the `.d` must be the
//!   source, a visible declared header, or under one of the toolchain's
//!   `system_include_dirs`. Anything else (an undeclared header, a file
//!   outside the workspace such as `/usr/include/x.h`) fails the build.
//!
//! Anubis never reads source or header files; only the compiler's `.d`.
//! Known gap without a sandbox (#209): an undeclared header created after a
//! result was cached can still hit, where a fresh compile would fail the
//! check. That never produces a wrong object, only a missed error.

use std::collections::HashSet;

use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;

use crate::anubis::AnubisTarget;
use crate::build_cache::{BuildCache, KeyBuilder};
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

/// The headers a compile may read, as declared.
#[derive(Debug, Default, Clone)]
pub struct HeaderSet {
    /// Lexical form of each path (see [`lexical_path`]), for the `.d` check.
    norm: HashSet<String>,
    /// Sorted, deduplicated, for the cache key.
    sorted: Vec<Utf8PathBuf>,
}

impl HeaderSet {
    pub fn new<'a>(paths: impl IntoIterator<Item = &'a Utf8PathBuf>) -> Self {
        let mut norm = HashSet::new();
        let mut sorted = Vec::new();
        for p in paths {
            let n = lexical_path(p.as_str());
            if norm.insert(n.clone()) {
                sorted.push(Utf8PathBuf::from(n));
            }
        }
        sorted.sort();
        HeaderSet { norm, sorted }
    }

    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    /// Add the header names (not contents) to a compile's manifest key.
    pub fn key_add(&self, cache: &BuildCache, kb: &mut KeyBuilder) {
        kb.field("declared_headers");
        kb.field((self.sorted.len() as u64).to_le_bytes());
        for p in &self.sorted {
            kb.field(cache.normalize_path(p));
        }
    }
}

/// Check a compile's `.d` file: every file listed must be `src`, a declared
/// header in `headers`, or under one of `system_dirs` (the toolchain's
/// declaration). Files outside the workspace are always an error. Returns
/// the files read, in lexical form, for the cache's second level.
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
        .map(|d| format!("{}/", lexical_path(d.as_str()).trim_end_matches('/')))
        .collect();

    let mut read = Vec::new();
    let mut outside = Vec::new();
    let mut undeclared = Vec::new();
    for dep in crate::build_cache::parse_depfile(&content) {
        let p = Utf8Path::new(&dep);
        let n = if p.is_absolute() { lexical_path(&dep) } else { lexical_path(root.join(p).as_str()) };
        if !n.starts_with(&root_n) {
            outside.push(n);
        } else if n == src_n || headers.norm.contains(&n) || system_n.iter().any(|d| n.starts_with(d)) {
            read.push(Utf8PathBuf::from(n));
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
        let read = check_depfile(&d, &ws, &src, &HeaderSet::new([&hdr]), &[sys]).unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(read[1].as_str(), lexical_path(hdr.as_str()), "'..' is folded");
    }

    #[test]
    fn undeclared_and_outside_fail() {
        let (_t, ws) = ws();
        let src = ws.join("main.c");
        let d = depfile(&ws, &[src.clone(), ws.join("inc/b.h")]);
        let err = check_depfile(&d, &ws, &src, &HeaderSet::new([&ws.join("inc/a.h")]), &[]).unwrap_err();
        assert!(err.to_string().contains("inc/b.h"), "{err}");

        let d = depfile(&ws, &[src.clone(), Utf8PathBuf::from("/usr/include/stdio.h")]);
        let err = check_depfile(&d, &ws, &src, &HeaderSet::default(), &[Utf8PathBuf::from("/usr/include")]).unwrap_err();
        assert!(err.to_string().contains("Outside the workspace"), "{err}");
    }

    #[test]
    fn key_changes_with_header_names_not_order() {
        let (t, ws) = ws();
        let backend = LocalDiskBackend::new(Utf8PathBuf::from_path_buf(t.path().join("cache")).unwrap()).unwrap();
        let cache = BuildCache::new(&ws, Box::new(backend));
        let key = |hs: &[Utf8PathBuf]| {
            let mut kb = KeyBuilder::new("t");
            HeaderSet::new(hs).key_add(&cache, &mut kb);
            kb.finish()
        };
        let (a, b) = (ws.join("a.h"), ws.join("b.h"));
        assert_eq!(key(&[a.clone(), b.clone()]), key(&[b.clone(), a.clone(), a.clone()]));
        assert_ne!(key(&[a.clone()]), key(&[a.clone(), b.clone()]), "a new declared header is a miss");
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
