//! Case-insensitive filename aliases for the MSVC CRT and Windows SDK.
//!
//! Microsoft ships those trees with arbitrary filename casing
//! (`kernel32.Lib`, `Uuid.Lib`, `driverspecs.h`) and, because Windows
//! filesystems are case-insensitive, nothing in them agrees on one spelling:
//! `kernelspecs.h` says `#include <DriverSpecs.h>`, linkers ask for
//! `kernel32.lib`, user code writes `<windows.h>`. On a case-sensitive host
//! (Linux) that all fails.
//!
//! The fix, the same one `xwin` uses, is a symlink farm next to the real
//! files:
//!
//!   1. every regular file whose name is not all lowercase gets a lowercase alias
//!      (`kernel32.lib -> kernel32.Lib`); and
//!   2. every header that some other header `#include`s under a different
//!      spelling gets an alias with that exact spelling
//!      (`DriverSpecs.h -> driverspecs.h`); and
//!   3. every directory some header `#include`s under a different spelling
//!      gets an alias too (`<GL/gl.h>` with only `um/gl/` on disk); and
//!   4. every header gets a capitalized alias (`Basetsd.h -> basetsd.h`).
//!      Pass 2 only sees spellings inside the SDK itself; third-party code
//!      (Filament: `<Basetsd.h>`, `<Winsock2.h>`, `<Wingdi.h>`) most often
//!      writes the first letter in capitals, which this covers. Any other
//!      spelling still fails; the complete fix is a case-insensitive
//!      `-ivfsoverlay`.
//!
//! Aliases are relative symlinks to a sibling, so they survive the CAS pass
//! (which reproduces symlinks verbatim) and the install tree's relocation.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// True on hosts whose filesystems are (normally) case-sensitive, where
/// the Microsoft trees need aliases.
pub fn host_needs_aliases() -> bool {
    !cfg!(windows)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CaseFoldReport {
    pub lowercase_aliases: usize,
    pub include_aliases: usize,
    pub capitalized_aliases: usize,
    pub dir_aliases: usize,
}

fn is_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".h", ".hpp", ".hxx", ".hh", ".inl", ".idl", ".tlh", ".tli"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Directory components (`GL` in `<GL/gl.h>`) of every `#include` in `text`.
fn include_dirnames(text: &str, out: &mut BTreeSet<String>) {
    let mut paths = BTreeSet::new();
    include_paths(text, &mut paths);
    for p in paths {
        let parts: Vec<&str> = p.split(['/', '\\']).collect();
        for d in &parts[..parts.len().saturating_sub(1)] {
            if !d.is_empty() && *d != "." && *d != ".." {
                out.insert(d.to_string());
            }
        }
    }
}

/// Pull the filename component of every `#include <x>` / `#include "x"` in
/// `text`. Only the last path component is returned; directory casing in
/// includes is not aliased.
fn include_basenames(text: &str, out: &mut BTreeSet<String>) {
    let mut paths = BTreeSet::new();
    include_paths(text, &mut paths);
    for path in paths {
        let base = path.rsplit(['/', '\\']).next().unwrap_or(&path);
        if !base.is_empty() {
            out.insert(base.to_string());
        }
    }
}

/// Every `#include <x>` / `#include "x"` path in `text`, verbatim.
fn include_paths(text: &str, out: &mut BTreeSet<String>) {
    for line in text.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix('#') else { continue };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix("include") else { continue };
        let rest = rest.trim_start();
        let close = match rest.chars().next() {
            Some('<') => '>',
            Some('"') => '"',
            _ => continue,
        };
        let inner = &rest[1..];
        let Some(end) = inner.find(close) else { continue };
        let path = &inner[..end];
        if !path.is_empty() {
            out.insert(path.to_string());
        }
    }
}

/// Add case aliases under `root`. Idempotent: an alias whose name already
/// exists (as a file, directory or symlink) is skipped.
pub fn add_case_aliases(root: &Path) -> Result<CaseFoldReport> {
    // dir -> set of names present in it (exact case)
    let mut names_by_dir: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    // lowercase header basename -> [(dir, actual name)]
    let mut headers_by_lower: HashMap<String, Vec<(PathBuf, String)>> = HashMap::new();
    let mut header_paths: Vec<PathBuf> = Vec::new();
    // regular files whose name has uppercase letters: (dir, name)
    let mut mixed_case_files: Vec<(PathBuf, String)> = Vec::new();
    // lowercase directory name -> [(parent, actual name)]
    let mut dirs_by_lower: HashMap<String, Vec<(PathBuf, String)>> = HashMap::new();

    for entry in jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .sort(true)
    {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        let path = entry.path();
        if path == root {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let dir = path.parent().unwrap_or(root).to_path_buf();
        names_by_dir.entry(dir.clone()).or_default().insert(name.clone());
        let is_file = entry.file_type().is_file();
        if entry.file_type().is_dir() {
            dirs_by_lower
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push((dir.clone(), name.clone()));
        }
        if is_file && name != name.to_ascii_lowercase() {
            mixed_case_files.push((dir.clone(), name.clone()));
        }
        if is_file && is_header(&name) {
            headers_by_lower
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push((dir, name));
            header_paths.push(path);
        }
    }

    // Planned aliases: (dir, alias name) -> target sibling name.
    let mut plan: BTreeMap<(PathBuf, String), String> = BTreeMap::new();
    let mut lowercase_aliases = 0usize;

    for (dir, name) in &mixed_case_files {
        let lower = name.to_ascii_lowercase();
        if !names_by_dir[dir].contains(&lower) {
            let key = (dir.clone(), lower);
            if !plan.contains_key(&key) {
                plan.insert(key, name.clone());
                lowercase_aliases += 1;
            }
        }
    }

    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut referenced_dirs: BTreeSet<String> = BTreeSet::new();
    for p in &header_paths {
        let bytes = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        include_basenames(&text, &mut referenced);
        include_dirnames(&text, &mut referenced_dirs);
    }
    let mut dir_aliases = 0usize;
    for spelled in &referenced_dirs {
        let Some(candidates) = dirs_by_lower.get(&spelled.to_ascii_lowercase()) else {
            continue;
        };
        for (parent, actual) in candidates {
            if actual == spelled || names_by_dir[parent].contains(spelled) {
                continue;
            }
            let key = (parent.clone(), spelled.clone());
            if !plan.contains_key(&key) {
                plan.insert(key, actual.clone());
                dir_aliases += 1;
            }
        }
    }
    let mut include_aliases = 0usize;
    for spelled in &referenced {
        let Some(candidates) = headers_by_lower.get(&spelled.to_ascii_lowercase()) else {
            continue;
        };
        for (dir, actual) in candidates {
            if actual == spelled || names_by_dir[dir].contains(spelled) {
                continue;
            }
            let key = (dir.clone(), spelled.clone());
            if !plan.contains_key(&key) {
                plan.insert(key, actual.clone());
                include_aliases += 1;
            }
        }
    }

    let mut capitalized_aliases = 0usize;
    for candidates in headers_by_lower.values() {
        for (dir, actual) in candidates {
            let lower = actual.to_ascii_lowercase();
            let mut cap = lower.clone();
            cap[..1].make_ascii_uppercase();
            if cap == *actual || names_by_dir[dir].contains(&cap) {
                continue;
            }
            let key = (dir.clone(), cap);
            if !plan.contains_key(&key) {
                plan.insert(key, actual.clone());
                capitalized_aliases += 1;
            }
        }
    }

    for ((dir, alias), target) in &plan {
        let link = dir.join(alias);
        if std::fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        crate::fs_util::symlink_any(Path::new(target), &link)
            .with_context(|| format!("aliasing {} -> {}", link.display(), target))?;
    }

    Ok(CaseFoldReport {
        lowercase_aliases,
        include_aliases,
        capitalized_aliases,
        dir_aliases,
    })
}

#[cfg(test)]
#[path = "tests/case_fold.rs"]
mod tests;
