//! Case-insensitive library lookup for Windows links on case-sensitive hosts.
//!
//! Windows sources and build files name import libraries with any casing
//! (`#pragma comment(lib, "Ole32.lib")`, `"WSOCK32.LIB"`), which Windows'
//! filesystems accept. lld-link looks each name up exactly as written, so
//! on Linux such a link fails with `could not open 'Ole32.lib'` although
//! `Ole32.Lib` is right there.
//!
//! The fix is lld-link's `/vfsoverlay:<file>`: an LLVM VFS overlay with
//! `case-sensitive: false`. That setting only applies to files the overlay
//! itself lists, so the file names every file in every library dir of the
//! link. Anubis writes it (lld-link honors only one overlay, and a link's
//! library dirs come from several places: the MSVC CRT, the Windows SDK,
//! user targets), once per distinct list of library dirs:
//!
//!   `.anubis-out/lib_overlay/<hash of contents>.yaml`
//!
//! The contents are in the file name, so the `/vfsoverlay:` argument is all
//! a link's cache key needs. Paths are relative to the overlay, so the same
//! tree gives the same file in any workspace location.
//!
//! Only directory listings are read, never a library. Two different files
//! whose names differ only by case can't exist on Windows and fail the
//! build; aliases of one file (Natron's lowercase symlinks) are fine.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest as _, Sha256};

use crate::util::SlashFix;
use crate::{bail_loc, function_name};

/// A written overlay and the listing it was made from.
#[derive(Debug)]
pub struct LibOverlay {
    /// The overlay file, for `/vfsoverlay:`.
    pub path: Utf8PathBuf,
    /// Per library dir, in search order: lowercase name -> name on disk.
    dirs: Vec<(Utf8PathBuf, BTreeMap<String, String>)>,
}

impl LibOverlay {
    /// The file a linker using this overlay opens for `name`: the first
    /// library dir with a file of that name, in any casing.
    pub fn find(&self, name: &str) -> Option<Utf8PathBuf> {
        let lower = name.to_lowercase();
        self.dirs.iter().find_map(|(dir, names)| names.get(&lower).map(|actual| dir.join(actual)))
    }
}

/// Files of `dir` by lowercase name. Aliases of one file collapse to the
/// name that isn't a symlink; different files with one lowercase name are
/// an error.
fn list_dir(dir: &Utf8Path) -> anyhow::Result<BTreeMap<String, String>> {
    let mut by_lower: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing library dir {dir}"))? {
        let entry = entry.with_context(|| format!("listing library dir {dir}"))?;
        let Ok(name) = entry.file_name().into_string() else { continue };
        // Follows symlinks: an alias of a file is a file.
        if std::fs::metadata(entry.path()).map(|m| m.is_file()).unwrap_or(false) {
            by_lower.entry(name.to_lowercase()).or_default().push(name);
        }
    }

    let mut out = BTreeMap::new();
    for (lower, mut names) in by_lower {
        names.sort();
        if names.len() > 1 {
            let real = |n: &String| std::fs::canonicalize(dir.join(n)).ok();
            let first = real(&names[0]);
            if first.is_none() || names.iter().any(|n| real(n) != first) {
                bail_loc!(
                    "Library dir [{dir}] has different files whose names differ only by case: {}. \
                     A Windows link can't tell them apart; remove or rename all but one.",
                    names.join(", ")
                );
            }
            let is_link = |n: &String| std::fs::symlink_metadata(dir.join(n)).map(|m| m.is_symlink()).unwrap_or(false);
            if let Some(i) = names.iter().position(|n| !is_link(n)) {
                names.swap(0, i);
            }
        }
        out.insert(lower, names.swap_remove(0));
    }
    Ok(out)
}

/// The overlay's text for `dirs`, as seen from `overlay_dir`. JSON, which
/// is YAML. Relative to the overlay when every dir is under `root`.
fn overlay_text(root: &Utf8Path, overlay_dir: &Utf8Path, dirs: &[(Utf8PathBuf, BTreeMap<String, String>)]) -> String {
    let up = overlay_dir
        .strip_prefix(root)
        .ok()
        .map(|rel| "../".repeat(rel.components().count()));
    let relative = up.is_some() && dirs.iter().all(|(d, _)| d.starts_with(root));
    let spell = |dir: &Utf8Path| -> String {
        match (relative, dir.strip_prefix(root)) {
            (true, Ok(rel)) => format!("{}{}", up.as_deref().unwrap_or(""), rel.to_owned().slash_fix()),
            _ => dir.to_owned().slash_fix().to_string(),
        }
    };

    let roots: Vec<serde_json::Value> = dirs
        .iter()
        .map(|(dir, names)| {
            let dir = spell(dir);
            let contents: Vec<serde_json::Value> = names
                .values()
                .map(|name| {
                    serde_json::json!({
                        "name": name,
                        "type": "file",
                        "external-contents": format!("{dir}/{name}"),
                    })
                })
                .collect();
            serde_json::json!({ "name": dir, "type": "directory", "contents": contents })
        })
        .collect();

    let mut doc = serde_json::json!({
        "version": 0,
        "case-sensitive": false,
        "roots": roots,
    });
    if relative {
        // Root names and external-contents are relative to the overlay's dir.
        doc["root-relative"] = "overlay-dir".into();
        doc["overlay-relative"] = true.into();
    }
    serde_json::to_string_pretty(&doc).expect("overlay JSON") + "\n"
}

/// List `library_dirs` and write their overlay under `out_dir`.
fn write_overlay(root: &Utf8Path, out_dir: &Utf8Path, library_dirs: &[Utf8PathBuf]) -> anyhow::Result<LibOverlay> {
    let mut dirs = Vec::new();
    for dir in library_dirs {
        if !dirs.iter().any(|(d, _)| d == dir) {
            dirs.push((dir.clone(), list_dir(dir)?));
        }
    }
    let text = overlay_text(root, out_dir, &dirs);
    let hash: String = Sha256::digest(text.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let path = out_dir.join(format!("{hash}.yaml"));
    if std::fs::read(&path).ok().as_deref() != Some(text.as_bytes()) {
        std::fs::create_dir_all(out_dir).with_context(|| format!("creating {out_dir}"))?;
        // Rename into place: another Anubis may be writing the same file.
        let tmp = out_dir.join(format!("{hash}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &text).with_context(|| format!("writing {tmp}"))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("renaming {tmp} to {path}"))?;
    }
    Ok(LibOverlay { path, dirs })
}

/// The overlay for a link that searches `library_dirs`, in that order.
/// Written on first use in this process; later links with the same dirs
/// share it.
pub fn lib_overlay(root: &Utf8Path, library_dirs: &[Utf8PathBuf]) -> anyhow::Result<Arc<LibOverlay>> {
    type Key = (Utf8PathBuf, Vec<Utf8PathBuf>);
    static MEMO: LazyLock<Mutex<HashMap<Key, Arc<LibOverlay>>>> = LazyLock::new(Default::default);

    let key = (root.to_owned(), library_dirs.to_vec());
    let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(found) = memo.get(&key) {
        return Ok(found.clone());
    }
    let overlay = Arc::new(write_overlay(root, &root.join(".anubis-out").join("lib_overlay"), library_dirs)?);
    memo.insert(key, overlay.clone());
    Ok(overlay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf8(p: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap()
    }

    #[test]
    fn lists_every_file_and_finds_any_casing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = utf8(tmp.path());
        let (a, b) = (root.join("sdk/a"), root.join("sdk/b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(b.join("subdir")).unwrap();
        std::fs::write(a.join("Ole32.Lib"), b"a").unwrap();
        std::fs::write(b.join("psapi.lib"), b"b").unwrap();
        std::fs::write(b.join("Ole32.Lib"), b"shadowed").unwrap();

        let out = root.join(".anubis-out/lib_overlay");
        let overlay = write_overlay(&root, &out, &[a.clone(), b.clone(), a.clone()]).unwrap();
        assert_eq!(overlay.find("OLE32.LIB"), Some(a.join("Ole32.Lib")));
        assert_eq!(overlay.find("pSaPi.LiB"), Some(b.join("psapi.lib")));
        assert_eq!(overlay.find("subdir"), None);
        assert_eq!(overlay.find("missing.lib"), None);

        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&overlay.path).unwrap()).unwrap();
        assert_eq!(doc["case-sensitive"], false);
        assert_eq!(doc["root-relative"], "overlay-dir");
        assert_eq!(doc["overlay-relative"], true);
        assert_eq!(doc["roots"].as_array().unwrap().len(), 2);
        assert_eq!(doc["roots"][0]["name"], "../../sdk/a");
        assert_eq!(doc["roots"][0]["contents"][0]["name"], "Ole32.Lib");
        assert_eq!(doc["roots"][0]["contents"][0]["external-contents"], "../../sdk/a/Ole32.Lib");

        // Same listing, same file; a new library, a new file.
        assert_eq!(write_overlay(&root, &out, &[a.clone(), b.clone()]).unwrap().path, overlay.path);
        std::fs::write(b.join("new.lib"), b"n").unwrap();
        assert_ne!(write_overlay(&root, &out, &[a.clone(), b.clone()]).unwrap().path, overlay.path);
    }

    #[test]
    fn dirs_outside_the_workspace_are_absolute() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = utf8(tmp.path()).join("ws");
        let outside = utf8(tmp.path()).join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.lib"), b"x").unwrap();

        let overlay = write_overlay(&root, &root.join(".anubis-out/lib_overlay"), &[outside.clone()]).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&overlay.path).unwrap()).unwrap();
        assert_eq!(doc["roots"][0]["name"], outside.as_str().replace('\\', "/"));
        assert!(doc.get("overlay-relative").is_none());
        assert!(doc.get("root-relative").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn aliases_collapse_and_case_conflicts_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = utf8(tmp.path());
        // Whether the temp dir is case-sensitive decides if this test can run.
        std::fs::write(root.join("probe"), b"").unwrap();
        if root.join("PROBE").exists() {
            return;
        }
        let dir = root.join("lib");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Kernel32.Lib"), b"k").unwrap();
        std::os::unix::fs::symlink("Kernel32.Lib", dir.join("kernel32.lib")).unwrap();
        std::os::unix::fs::symlink("Kernel32.Lib", dir.join("KERNEL32.LIB")).unwrap();

        let names = list_dir(&dir).unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names["kernel32.lib"], "Kernel32.Lib");

        std::fs::write(dir.join("KERNEL32.lib"), b"other").unwrap();
        let err = list_dir(&dir).unwrap_err().to_string();
        assert!(err.contains("differ only by case"), "{err}");
        assert!(err.contains("KERNEL32.lib"), "{err}");
    }
}
