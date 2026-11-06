//! Negative dependencies for cached C/C++ compiles (#172).
//!
//! The `.d` file names the headers a compile read, so editing one is a cache
//! miss. It can't name the files the preprocessor looked for and didn't
//! find, so creating one of those must also be a miss:
//!
//! - a header that now shadows one found later on the search path, e.g.
//!   `inc_a/x.h` when `inc_b/x.h` was used;
//! - the target of a `__has_include` that was false, or of an `#include`
//!   in a branch that wasn't taken.
//!
//! So after a compile, every project file it read is scanned for
//! `#include`, `#include_next`, `#import` and `__has_include` names. Each
//! name is looked up the way the compiler does (quote includes first in the
//! including file's dir), and every candidate path before the one that
//! exists is recorded in the manifest entry as "must not exist". A lookup
//! then checks those paths with one memoized stat each. Only real
//! shadowing is a miss, and the manifest key stays tool + argv + source,
//! so it's stable across machines (for a remote cache).
//!
//! Not covered (also misses on every other cache we know of except a full
//! preprocessor run): `#include MACRO`, MSVC-compatible lookup in the dirs
//! of every file on the include stack (only the including file's dir and
//! the source's dir are used), and new headers inside Natron installs,
//! which never change.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use camino::{Utf8Path, Utf8PathBuf};
use dashmap::DashMap;

use crate::build_cache::{BuildCache, Digest};

/// The dirs the compiler searches, in order, from its arguments.
#[derive(Debug, Default, PartialEq)]
pub struct SearchPath {
    /// `-iquote`: only for `#include "x"`, before `angle`.
    pub quote: Vec<Utf8PathBuf>,
    /// `-I`, then `-isystem`, then `-idirafter`: for both forms.
    pub angle: Vec<Utf8PathBuf>,
}

impl SearchPath {
    pub fn from_args(args: &[String]) -> Self {
        const FLAGS: [&str; 4] = ["-iquote", "-isystem", "-idirafter", "-I"];
        let (mut quote, mut i, mut system, mut after) = (vec![], vec![], vec![], vec![]);
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let Some(flag) = FLAGS.iter().find(|f| arg.starts_with(**f)) else { continue };
            let dir = match &arg[flag.len()..] {
                "" => match it.next() {
                    Some(d) => d.clone(),
                    None => break,
                },
                joined => joined.to_owned(),
            };
            let list = match *flag {
                "-iquote" => &mut quote,
                "-isystem" => &mut system,
                "-idirafter" => &mut after,
                _ => &mut i,
            };
            list.push(Utf8PathBuf::from(dir));
        }
        i.extend(system);
        i.extend(after);
        SearchPath { quote, angle: i }
    }
}

/// One `#include`-like name in a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Directive {
    pub name: String,
    /// `"x"` rather than `<x>`.
    pub quote: bool,
    /// `#include_next` / `__has_include_next`: search after the including
    /// file's own search dir.
    pub next: bool,
}

/// Finds include names, including ones in comments or `#if 0` (harmless:
/// at worst an extra "must not exist" path).
pub fn scan(text: &str) -> Vec<Directive> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix('#') {
            let rest = rest.trim_start();
            for (kw, next) in [("include_next", true), ("include", false), ("import", false)] {
                if let Some(rest) = rest.strip_prefix(kw) {
                    out.extend(quoted_name(rest, next));
                    break;
                }
            }
        }
        let mut rest = line;
        while let Some(i) = rest.find("__has_include") {
            rest = &rest[i + "__has_include".len()..];
            let next = rest.starts_with("_next");
            let after = rest.trim_start_matches("_next").trim_start();
            if let Some(args) = after.strip_prefix('(') {
                out.extend(quoted_name(args, next));
            }
        }
    }
    out
}

/// `"name"` or `<name>` at the start of `s` (after spaces).
fn quoted_name(s: &str, next: bool) -> Option<Directive> {
    let s = s.trim_start();
    let (quote, close) = match s.chars().next()? {
        '"' => (true, '"'),
        '<' => (false, '>'),
        _ => return None,
    };
    let name = &s[1..1 + s[1..].find(close)?];
    (!name.is_empty()).then(|| Directive { name: name.to_owned(), quote, next })
}

/// Paths that must not exist for a compile of `src` that read `inputs`
/// (from the `.d` file) to stay valid. Sorted, without duplicates.
pub fn absent_headers(
    cache: &BuildCache,
    search: &SearchPath,
    src: &Utf8Path,
    inputs: &[Utf8PathBuf],
) -> Vec<Utf8PathBuf> {
    // Scans are memoized by content for the process: most headers are
    // read by many compiles.
    static SCANS: OnceLock<DashMap<Digest, Arc<Vec<Directive>>>> = OnceLock::new();
    let scans = SCANS.get_or_init(DashMap::new);

    let src_dir = src.parent().unwrap_or(Utf8Path::new(""));
    let mut absent = BTreeSet::new();
    for file in inputs {
        let dir = file.parent().unwrap_or(Utf8Path::new(""));
        if cache.is_natron_dir(dir) {
            continue; // toolchain headers: their lookups stay inside the toolchain
        }
        let Ok(Some(digest)) = cache.file_digest(file) else { continue };
        let directives = match scans.get(&digest) {
            Some(d) => d.clone(),
            None => {
                let Ok(text) = std::fs::read(file) else { continue };
                let d = Arc::new(scan(&String::from_utf8_lossy(&text)));
                scans.insert(digest, d.clone());
                d
            }
        };
        for d in directives.iter() {
            let mut bases: Vec<&Utf8Path> = Vec::new();
            if d.next {
                // Search the dirs after the one the including file was found in.
                let angle = &search.angle;
                let start = angle.iter().position(|b| file.starts_with(b)).map_or(0, |i| i + 1);
                bases.extend(angle[start..].iter().map(|p| p.as_path()));
            } else {
                if d.quote {
                    bases.push(dir);
                    bases.push(src_dir);
                    bases.extend(search.quote.iter().map(|p| p.as_path()));
                }
                bases.extend(search.angle.iter().map(|p| p.as_path()));
            }
            for base in bases {
                let candidate = base.join(&d.name);
                if cache.is_file(&candidate) {
                    break;
                }
                if !cache.is_natron_dir(base) {
                    absent.insert(candidate);
                }
            }
        }
    }
    absent.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_include_forms() {
        let text = r#"
#include "a.h"
  #  include <b/c.h>
#include_next <d.h>
#import "e.h"
#if __has_include(<f.h>) && __has_include_next ( "g.h" )
#include MACRO_H
#include ""
"#;
        let got: Vec<_> = scan(text).into_iter().map(|d| (d.name, d.quote, d.next)).collect();
        let want = [("a.h", true, false), ("b/c.h", false, false), ("d.h", false, true), ("e.h", true, false), ("f.h", false, false), ("g.h", true, true)];
        assert_eq!(got, want.map(|(n, q, x)| (n.to_owned(), q, x)));
    }

    #[test]
    fn search_path_order_and_joined_forms() {
        let args: Vec<String> =
            ["-isystem", "s1", "-Ii1", "-iquote", "q1", "-idirafterA", "-I", "i2", "-isystems2", "-c", "x.c"]
                .map(String::from)
                .into();
        let sp = SearchPath::from_args(&args);
        assert_eq!(sp.quote, ["q1"].map(Utf8PathBuf::from));
        assert_eq!(sp.angle, ["i1", "i2", "s1", "s2", "A"].map(Utf8PathBuf::from));
    }
}
