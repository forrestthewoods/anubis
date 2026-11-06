//! Negative dependencies for cached C/C++ compiles (#172).
//!
//! The `.d` file names the headers a compile read, so editing one is a cache
//! miss. It can't name the files the preprocessor looked for and didn't
//! find, so creating one of those must also be a miss:
//!
//! - a header that now shadows one found later on the search path, e.g.
//!   `inc_a/x.h` when `inc_b/x.h` was used;
//! - the target of a `__has_include` that was false.
//!
//! So after a compile, every project file it read is scanned for
//! `#include`, `#include_next`, `#import` and `__has_include` names (after
//! removing comments and `#if 0` blocks). Each name is looked up the way the
//! compiler does, and every candidate path before the one that exists is
//! recorded in the manifest entry as "must not exist". A lookup then checks
//! those paths with one memoized stat each. The manifest key stays tool +
//! argv + source, so it's stable across machines (for a remote cache).
//!
//! The scan is checked against the `.d`, and a compile it can't explain is
//! not cached rather than cached unsafely: if the first existing candidate
//! for a name isn't a file the compile read but a later one is, the model of
//! the search order was wrong (or the file system changed during the build).
//! Also not cached: a search flag this module doesn't model (`@file`, `-F`,
//! `-include`, ...), `__has_include(MACRO)`, `#pragma include_alias` and
//! `#embed`. For `#include MACRO` the names come from the `.d` instead.
//!
//! Best effort. Known gaps, where a new file can still give a stale hit:
//! MSVC-compatible lookup in the dirs of every file on the include stack
//! (only the including file's dir and the source's dir are searched, see
//! #172's follow-ups), a project header that shadows a name a toolchain
//! header includes (Natron headers aren't scanned: it would add thousands of
//! paths per compile), and NASM's `%include`. Names in `#if` branches that
//! weren't taken are recorded too, so creating one of those is a needless
//! recompile.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, OnceLock};

use camino::{Utf8Path, Utf8PathBuf};
use dashmap::DashMap;

use crate::build_cache::{BuildCache, Digest};

/// The dirs the compiler searches, in order, from its arguments. Paths are
/// in [`lexical`] form.
#[derive(Debug, Default, PartialEq)]
pub struct SearchPath {
    /// `-iquote`: only for `#include "x"`, before `angle`.
    pub quote: Vec<String>,
    /// `-I`, then `-isystem`, then `-idirafter`: for both forms.
    pub angle: Vec<String>,
    /// clang's MSVC compatibility (the default for `*-windows-msvc`): a quote
    /// include also searches the dirs of the files that include the
    /// includer. Modelled as the source's dir only.
    pub ms_compat: bool,
}

impl SearchPath {
    /// Err names a flag that changes header search in a way this module
    /// doesn't model.
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        // Longest first where one is a prefix of another.
        const UNMODELLED: [&str; 12] = [
            "-isystem-after", "-iprefix", "-iwith", "-iframework", "-ivfsoverlay", "-imsvc",
            "-include", "-imacros", "-cxx-isystem", "--embed-dir", "-I-", "-F",
        ];
        const FLAGS: [&str; 6] =
            ["--include-directory-after=", "--include-directory=", "-iquote", "-isystem", "-idirafter", "-I"];
        let (mut quote, mut user, mut system, mut after) = (vec![], vec![], vec![], vec![]);
        let mut ms_target = false;
        let mut ms_flag = None;
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if arg.starts_with('@') {
                return Err(arg.clone());
            }
            if let Some(f) = UNMODELLED.iter().find(|f| arg.starts_with(**f)) {
                return Err(f.to_string());
            }
            match arg.as_str() {
                "-fms-compatibility" => ms_flag = Some(true),
                "-fno-ms-compatibility" => ms_flag = Some(false),
                "-target" => ms_target = it.clone().next().is_some_and(|t| t.contains("windows-msvc")),
                a if a.starts_with("--target=") => ms_target = a.contains("windows-msvc"),
                _ => {}
            }
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
                "-idirafter" | "--include-directory-after=" => &mut after,
                _ => &mut user,
            };
            list.push(lexical(&dir));
        }
        // Like clang: an -I dir that is also a system dir counts as a system
        // dir, and only the first of duplicates is searched.
        let sys: HashSet<&String> = system.iter().chain(&after).collect();
        let mut seen = HashSet::new();
        let angle = user
            .iter()
            .filter(|d| !sys.contains(d))
            .chain(&system)
            .chain(&after)
            .filter(|d| seen.insert(*d))
            .cloned()
            .collect();
        let mut seen = HashSet::new();
        quote.retain(|d| seen.insert(d.clone()));
        Ok(SearchPath { quote, angle, ms_compat: ms_flag.unwrap_or(ms_target) })
    }
}

/// A path with `/` separators, no `.` or `x/..` components, and lowercase on
/// Windows (like [`crate::util::normalize_path`]), so spellings compare
/// equal.
pub fn lexical(p: &str) -> String {
    let p = p.replace('\\', "/");
    let abs = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." if out.last().is_some_and(|l| *l != "..") => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    let s = format!("{}{}", if abs { "/" } else { "" }, out.join("/"));
    if cfg!(windows) { s.to_lowercase() } else { s }
}

fn join(base: &str, name: &str) -> String {
    lexical(&format!("{base}/{name}"))
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

#[derive(Debug, Default, PartialEq)]
pub struct Scan {
    pub directives: Vec<Directive>,
    /// `#include MACRO`: the name isn't in the text.
    pub macro_include: bool,
}

/// Include names in a file, after translation phases 1-3 (line splices,
/// comments) and without `#if 0` blocks. Err says why the file can't be
/// modelled.
pub fn scan(text: &str) -> Result<Scan, &'static str> {
    let text = strip_comments(text);
    let mut out = Scan::default();
    let mut skip_depth = 0usize; // inside `#if 0` when > 0
    for line in text.lines() {
        let t = line.trim_start();
        let directive = t.strip_prefix('#').or_else(|| t.strip_prefix("%:")).map(str::trim_start);
        if let Some(rest) = directive {
            let word: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            let after = rest[word.len()..].trim_start();
            if skip_depth > 0 {
                match word.as_str() {
                    "if" | "ifdef" | "ifndef" => skip_depth += 1,
                    "endif" => skip_depth -= 1,
                    "else" | "elif" | "elifdef" | "elifndef" if skip_depth == 1 => skip_depth = 0,
                    _ => {}
                }
                continue;
            }
            match word.as_str() {
                "if" if after.trim_end() == "0" => {
                    skip_depth = 1;
                    continue;
                }
                "include" | "import" | "include_next" => match quoted_name(after, word == "include_next") {
                    Some(d) => out.directives.push(d),
                    None => out.macro_include = true,
                },
                "embed" => return Err("#embed"),
                "pragma" if after.starts_with("include_alias") => return Err("#pragma include_alias"),
                _ => {}
            }
        } else if skip_depth > 0 {
            continue;
        }
        if line.contains("__has_embed") {
            return Err("__has_embed");
        }
        let mut rest = line;
        while let Some(i) = rest.find("__has_include") {
            rest = &rest[i + "__has_include".len()..];
            let next = rest.starts_with("_next");
            let after = rest.trim_start_matches("_next").trim_start();
            if let Some(args) = after.strip_prefix('(') {
                out.directives.push(quoted_name(args, next).ok_or("__has_include(MACRO)")?);
            }
        }
    }
    Ok(out)
}

/// Translation phases 1-3, roughly: drop a BOM, accept `\r\n` and `\r`, join
/// backslash-newlines, and turn each comment into a space. String and
/// character literals are skipped so `"//"` isn't a comment.
fn strip_comments(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let text = text.replace("\r\n", "\n").replace('\r', "\n").replace("\\\n", "");
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1)) {
            (b'/', Some(b'/')) => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                out.push(b' ');
            }
            (b'/', Some(b'*')) => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
                out.push(b' ');
            }
            (q @ (b'"' | b'\''), _) => {
                // A literal ends on the same line; if it doesn't (a digit
                // separator, an apostrophe in `#error`), it isn't one.
                let mut j = i + 1;
                while j < b.len() && b[j] != q && b[j] != b'\n' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = if j < b.len() && b[j] == q { j + 1 } else { i + 1 };
                out.extend_from_slice(&b[i..end.min(b.len())]);
                i = end;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// Paths that must not exist for a compile of `src` with `args` that read
/// `inputs` (from the `.d` file) to stay valid, sorted. Err says why the
/// compile can't be cached safely.
pub fn absent_headers(
    cache: &BuildCache,
    args: &[String],
    src: &Utf8Path,
    inputs: &[Utf8PathBuf],
) -> Result<Vec<Utf8PathBuf>, String> {
    // Scans are memoized by content for the process: most headers are
    // read by many compiles.
    static SCANS: OnceLock<DashMap<Digest, Arc<Scan>>> = OnceLock::new();
    let scans = SCANS.get_or_init(DashMap::new);

    let search = SearchPath::from_args(args).map_err(|f| format!("unmodelled search flag {f}"))?;
    let natron = |dirs: &[String]| -> Vec<bool> {
        dirs.iter().map(|d| cache.is_natron_dir(Utf8Path::new(d))).collect()
    };
    let (quote_natron, angle_natron) = (natron(&search.quote), natron(&search.angle));
    let read: HashSet<String> = inputs.iter().map(|p| lexical(p.as_str())).collect();
    let src_dir = lexical(src.parent().map_or("", |p| p.as_str()));

    let mut walker = Walker { cache, read: &read, absent: BTreeSet::new(), walked: HashSet::new() };
    let mut macro_include = false;
    let mut project_files = Vec::new();
    for file in inputs {
        let dir = file.parent().unwrap_or(Utf8Path::new(""));
        if cache.is_natron_dir(dir) {
            continue; // see the module docs
        }
        let file_lex = lexical(file.as_str());
        let dir_lex = lexical(dir.as_str());
        project_files.push(file_lex.clone());
        let Ok(Some(digest)) = cache.file_digest(file) else { return Err(format!("{file} vanished")) };
        let scan = match scans.get(&digest) {
            Some(s) => s.clone(),
            None => {
                let bytes = std::fs::read(file).map_err(|e| format!("reading {file}: {e}"))?;
                if Digest::of_bytes(&bytes) != digest {
                    return Err(format!("{file} changed during the build"));
                }
                let s = Arc::new(scan(&String::from_utf8_lossy(&bytes)).map_err(|e| format!("{file}: {e}"))?);
                scans.insert(digest, s.clone());
                s
            }
        };
        macro_include |= scan.macro_include;
        for d in &scan.directives {
            let mut bases: Vec<(&str, bool)> = Vec::new();
            let mut own = None;
            if d.next {
                // Search the angle dirs after the one the file was found in:
                // the first that gives this file under the same name, else
                // the first that contains it. Found by neither, it was
                // found relative to its includer; clang then treats
                // `include_next` as `include`.
                let angle = &search.angle;
                let pos = angle
                    .iter()
                    .position(|b| join(b, &d.name) == file_lex)
                    .or_else(|| angle.iter().position(|b| file_lex.starts_with(&format!("{b}/"))));
                if let Some(i) = pos {
                    bases.extend(angle[i + 1..].iter().zip(&angle_natron[i + 1..]).map(|(b, n)| (b.as_str(), *n)));
                    own = Some(file_lex.as_str());
                }
            }
            if own.is_none() {
                if d.quote {
                    bases.push((&dir_lex, false));
                    if search.ms_compat {
                        bases.push((&src_dir, false));
                    }
                    bases.extend(search.quote.iter().zip(&quote_natron).map(|(b, n)| (b.as_str(), *n)));
                }
                bases.extend(search.angle.iter().zip(&angle_natron).map(|(b, n)| (b.as_str(), *n)));
            }
            walker.walk(&d.name, &bases, own)?;
        }
    }
    if macro_include {
        // The names `#include MACRO` used are unknown, but the files it
        // found are in the .d: walk each one's name relative to every angle
        // dir it is under.
        let bases: Vec<(&str, bool)> =
            search.angle.iter().zip(&angle_natron).map(|(b, n)| (b.as_str(), *n)).collect();
        for f in &project_files {
            for b in &search.angle {
                if let Some(name) = f.strip_prefix(&format!("{b}/")) {
                    walker.walk(name, &bases, None)?;
                }
            }
        }
    }
    Ok(walker.absent.into_iter().map(Utf8PathBuf::from).collect())
}

struct Walker<'a> {
    cache: &'a BuildCache,
    read: &'a HashSet<String>,
    absent: BTreeSet<String>,
    /// Lookups already done for this compile, by their candidate list.
    walked: HashSet<(String, String)>,
}

impl Walker<'_> {
    /// Look `name` up in `bases` (dir, is in a Natron install). Candidates
    /// before the first existing file must not exist; `own` (the including
    /// file, for `include_next`) is skipped.
    fn walk(&mut self, name: &str, bases: &[(&str, bool)], own: Option<&str>) -> Result<(), String> {
        let key = bases.first().map_or("", |b| b.0).to_owned() + "|" + own.unwrap_or("");
        if !self.walked.insert((key + "|" + &bases.len().to_string(), name.to_owned())) {
            return Ok(());
        }
        let candidates: Vec<(String, bool)> = bases
            .iter()
            .map(|(b, n)| (join(b, name), *n))
            .filter(|(c, _)| Some(c.as_str()) != own)
            .collect();
        for (i, (c, natron)) in candidates.iter().enumerate() {
            if self.cache.is_file(Utf8Path::new(c)) {
                // The compiler would have used this one. If it didn't read
                // it but read a later one, the model is wrong: don't cache.
                if !self.read.contains(c) {
                    if let Some((later, _)) = candidates[i + 1..].iter().find(|(l, _)| self.read.contains(l)) {
                        return Err(format!("expected {c} to be used for {name}, but the compile read {later}"));
                    }
                }
                return Ok(());
            }
            if !natron {
                self.absent.insert(c.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<(String, bool, bool)> {
        scan(text).unwrap().directives.into_iter().map(|d| (d.name, d.quote, d.next)).collect()
    }

    #[test]
    fn scans_include_forms() {
        let text = r#"
#include "a.h"
  #  include <b/c.h>
#include_next <d.h>
#import "e.h"
#if __has_include(<f.h>) && __has_include_next ( "g.h" )
#include ""
%:include "h.h"
#include /* comment */ "i.h"
"#;
        let want = [
            ("a.h", true, false),
            ("b/c.h", false, false),
            ("d.h", false, true),
            ("e.h", true, false),
            ("f.h", false, false),
            ("g.h", true, true),
            ("h.h", true, false),
            ("i.h", true, false),
        ];
        assert_eq!(names(text), want.map(|(n, q, x)| (n.to_owned(), q, x)));
    }

    #[test]
    fn scan_skips_comments_and_if_0_but_not_else() {
        let text = "\u{feff}// #include \"c1.h\"\r\n/* #include \"c2.h\"\n*/\n#if 0\n#include \"z.h\"\n#if X\n#endif\n#else\n#include \"live.h\"\n#endif\nconst char* s = \"// not a comment\"; int n = 1'000;\n#inc\\\nlude \"spliced.h\"\r#include \"cr.h\"\n";
        let got: Vec<_> = names(text).into_iter().map(|d| d.0).collect();
        assert_eq!(got, ["live.h", "spliced.h", "cr.h"]);
    }

    #[test]
    fn scan_flags_what_it_cannot_model() {
        assert!(scan("#include FT_FREETYPE_H\n").unwrap().macro_include);
        assert!(scan("#if __has_include(HDR)\n#endif\n").is_err());
        assert!(scan("#pragma include_alias(\"a.h\", \"b.h\")\n").is_err());
        assert!(scan("#embed \"x.bin\"\n").is_err());
        // Not a call: fine.
        assert!(scan("#if defined(__has_include)\n#endif\n").is_ok());
    }

    #[test]
    fn search_path_order_dedup_and_joined_forms() {
        let args: Vec<String> = [
            "-isystem", "s1", "-Ii1", "-iquote", "q1", "-idirafterA", "-I", "i2", "-isystems2", "-Is1",
            "-I./i1", "--include-directory=i3", "-target", "x86_64-linux-gnu", "-c", "x.c",
        ]
        .map(String::from)
        .into();
        let sp = SearchPath::from_args(&args).unwrap();
        assert_eq!(sp.quote, ["q1"]);
        assert_eq!(sp.angle, ["i1", "i2", "i3", "s1", "s2", "A"]);
        assert!(!sp.ms_compat);
    }

    #[test]
    fn search_path_ms_compat_and_unmodelled_flags() {
        let sp = |a: &[&str]| SearchPath::from_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(sp(&["-target", "x86_64-pc-windows-msvc"]).unwrap().ms_compat);
        assert!(!sp(&["-target", "x86_64-pc-windows-msvc", "-fno-ms-compatibility"]).unwrap().ms_compat);
        assert!(sp(&["-fms-compatibility"]).unwrap().ms_compat);
        for bad in ["@args.rsp", "-include", "-imacrosx.h", "-Fframeworks", "-iframework", "-isystem-after", "-I-"] {
            assert!(sp(&[bad, "x"]).is_err(), "{bad}");
        }
    }

    #[test]
    fn lexical_forms() {
        assert_eq!(lexical("/a/./b//c/../d.h"), "/a/b/d.h");
        assert_eq!(lexical("a/../../b"), "../b");
        assert_eq!(lexical("C:\\x\\y.h"), if cfg!(windows) { "c:/x/y.h" } else { "C:/x/y.h" });
    }
}
