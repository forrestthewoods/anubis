//! Staged directories: a directory assembled from files that already exist
//! (an executable next to the shared libraries it loads).
//!
//! A stage is a cached step. Its key is the wanted list of (name, source,
//! source digest), written to a stamp file next to the directory once the
//! stage is complete. When the key matches the stamp and every file is
//! still there, the stage is valid and nothing is touched. Otherwise the
//! whole directory is deleted and staged again with hard links (copies
//! where a hard link fails), so it never holds a file it didn't place:
//! remove a dependency and its library disappears on the next build.

use crate::build_cache::{BuildCache, KeyBuilder};
use crate::{anyhow_loc, bail_loc, function_name};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashMap;

/// One file in a stage: `dir/name` is a hard link to (or copy of) `src`.
/// `name` is a file name or a relative path (`assets/cube.obj`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageEntry {
    pub name: String,
    pub src: Utf8PathBuf,
}

/// What [`stage_dir`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageResult {
    /// The previous stage was still valid; nothing was touched.
    Valid,
    /// The directory was deleted and staged again.
    Restaged,
}

/// The stamp file for a stage directory: `<dir>.stage`, next to it, so the
/// directory holds only staged files.
pub fn stamp_path(dir: &Utf8Path) -> Utf8PathBuf {
    let name = dir.file_name().unwrap_or("stage");
    dir.with_file_name(format!("{name}.stage"))
}

/// Make `dir` hold exactly `entries`. `dir` is owned by the stage: anything
/// else in it is deleted when the stage is redone. Without a build cache
/// (`--no-cache`) there are no digests to key on, so the stage is always
/// redone.
pub fn stage_dir(cache: Option<&BuildCache>, dir: &Utf8Path, entries: &[StageEntry]) -> anyhow::Result<StageResult> {
    let entries = dedupe(entries)?;
    let stamp = stamp_path(dir);

    let key = match cache {
        Some(cache) => {
            let mut kb = KeyBuilder::new("stage");
            kb.field((entries.len() as u64).to_le_bytes());
            for e in &entries {
                let digest = cache
                    .file_digest(&e.src)?
                    .ok_or_else(|| anyhow_loc!("Staging {}: source {} does not exist", dir, e.src))?;
                kb.field(&e.name).field(e.src.as_str()).digest(&digest);
            }
            Some(kb.finish().to_string())
        }
        None => None,
    };

    if let Some(key) = &key {
        let stamped = std::fs::read_to_string(&stamp).ok();
        // The files are checked by size only: they are hard links to
        // read-only build outputs, and this keeps a valid stage to a stat
        // per file.
        let complete = || {
            entries.iter().all(|e| {
                matches!(
                    (std::fs::metadata(dir.join(&e.name)), std::fs::metadata(&e.src)),
                    (Ok(d), Ok(s)) if d.is_file() && d.len() == s.len()
                )
            })
        };
        if stamped.as_deref() == Some(key.as_str()) && complete() {
            return Ok(StageResult::Valid);
        }
    }

    // Redo it: the stamp goes first, so an interrupted stage is never
    // mistaken for a complete one.
    remove_file_if_exists(&stamp)?;
    remove_dir_if_exists(dir)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    for e in &entries {
        let dst = dir.join(&e.name);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {parent}"))?;
        }
        if std::fs::hard_link(&e.src, &dst).is_err() {
            std::fs::copy(&e.src, &dst).with_context(|| format!("copying {} to {dst}", e.src))?;
        }
        if let Some(cache) = cache {
            cache.invalidate(&dst);
        }
    }
    if let Some(key) = &key {
        let tmp = stamp.with_file_name(format!(".{}.{}.tmp", stamp.file_name().unwrap_or("stage"), std::process::id()));
        std::fs::write(&tmp, key).with_context(|| format!("writing {tmp}"))?;
        std::fs::rename(&tmp, &stamp).with_context(|| format!("writing {stamp}"))?;
    }
    Ok(StageResult::Restaged)
}

/// Drop exact repeats; two different sources for one name are an error
/// (they would overwrite each other). Names compare ignoring ASCII case,
/// since Windows and macOS file systems do.
fn dedupe(entries: &[StageEntry]) -> anyhow::Result<Vec<StageEntry>> {
    let mut seen: HashMap<String, &StageEntry> = HashMap::new();
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        // A name may have directories (`assets/cube.obj`, a `resources`
        // file), never a way out of the stage.
        if e.name.contains('\\') || e.name.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
            bail_loc!("Bad staged file name [{}] for {}", e.name, e.src);
        }
        match seen.get(&e.name.to_ascii_lowercase()) {
            Some(prev) if prev.src == e.src && prev.name == e.name => continue,
            Some(prev) => bail_loc!(
                "Two files want the same name next to an executable: {} ({}) and {} ({})",
                prev.name,
                prev.src,
                e.name,
                e.src
            ),
            None => {
                seen.insert(e.name.to_ascii_lowercase(), e);
                out.push(e.clone());
            }
        }
    }
    Ok(out)
}

fn remove_file_if_exists(p: &Utf8Path) -> anyhow::Result<()> {
    crate::build_cache::remove_outputs([p])
}

/// Delete a directory and everything in it. Files go through
/// `remove_outputs`, which deletes read-only files (staged hard links to
/// cache blobs) without clearing the attribute the blob shares. A symlink
/// is removed, never followed.
fn remove_dir_if_exists(dir: &Utf8Path) -> anyhow::Result<()> {
    let md = match std::fs::symlink_metadata(dir) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("reading {dir}"))),
    };
    if !md.is_dir() {
        return crate::build_cache::remove_outputs([dir]);
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {dir}"))? {
        let entry = entry?;
        let path = Utf8PathBuf::try_from(entry.path())?;
        if entry.file_type()?.is_dir() {
            remove_dir_if_exists(&path)?;
        } else if std::fs::remove_file(&path).is_err() {
            // A directory symlink on Windows is removed as a directory.
            std::fs::remove_dir(&path).with_context(|| format!("removing {path}"))?;
        }
    }
    std::fs::remove_dir(dir).with_context(|| format!("removing {dir}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_cache::LocalDiskBackend;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: Utf8PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::try_from(tmp.path().to_path_buf()).unwrap();
            Fixture { _tmp: tmp, root }
        }

        /// A fresh cache per call, like a fresh `anubis build`.
        fn cache(&self) -> BuildCache {
            let backend = LocalDiskBackend::new(self.root.join(".anubis-cache")).unwrap();
            BuildCache::new(&self.root, Box::new(backend))
        }

        fn src(&self, name: &str, contents: &str) -> Utf8PathBuf {
            let p = self.root.join("src").join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, contents).unwrap();
            p
        }

        fn dir(&self) -> Utf8PathBuf {
            self.root.join(".anubis-out/t/bin")
        }
    }

    fn entry(name: &str, src: &Utf8Path) -> StageEntry {
        StageEntry { name: name.into(), src: src.to_owned() }
    }

    fn listing(dir: &Utf8Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    }

    #[test]
    fn second_stage_is_valid_and_touches_nothing() {
        let f = Fixture::new();
        let a = f.src("app", "exe");
        let b = f.src("libb.so", "lib");
        let entries = [entry("app", &a), entry("libb.so", &b)];
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &entries).unwrap(), StageResult::Restaged);
        assert_eq!(listing(&f.dir()), ["app", "libb.so"]);
        assert_eq!(std::fs::read_to_string(f.dir().join("libb.so")).unwrap(), "lib");
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &entries).unwrap(), StageResult::Valid);
    }

    #[test]
    fn removed_dependency_disappears() {
        let f = Fixture::new();
        let a = f.src("app", "exe");
        let b = f.src("libb.so", "lib");
        stage_dir(Some(&f.cache()), &f.dir(), &[entry("app", &a), entry("libb.so", &b)]).unwrap();
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &[entry("app", &a)]).unwrap(), StageResult::Restaged);
        assert_eq!(listing(&f.dir()), ["app"]);
    }

    #[test]
    fn changed_source_restages() {
        let f = Fixture::new();
        let a = f.src("app", "exe");
        let entries = [entry("app", &a)];
        stage_dir(Some(&f.cache()), &f.dir(), &entries).unwrap();
        // A tool replaces its output with a new file, it doesn't edit it.
        std::fs::remove_file(&a).unwrap();
        std::fs::write(&a, "new exe!").unwrap();
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &entries).unwrap(), StageResult::Restaged);
        assert_eq!(std::fs::read_to_string(f.dir().join("app")).unwrap(), "new exe!");
    }

    #[test]
    fn deleted_or_extra_files_are_handled() {
        let f = Fixture::new();
        let a = f.src("app", "exe");
        let b = f.src("libb.so", "lib");
        let entries = [entry("app", &a), entry("libb.so", &b)];
        stage_dir(Some(&f.cache()), &f.dir(), &entries).unwrap();
        std::fs::remove_file(f.dir().join("libb.so")).unwrap();
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &entries).unwrap(), StageResult::Restaged);
        assert_eq!(listing(&f.dir()), ["app", "libb.so"]);

        // A deleted directory comes back too.
        std::fs::remove_dir_all(f.dir()).unwrap();
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &entries).unwrap(), StageResult::Restaged);
        assert_eq!(listing(&f.dir()), ["app", "libb.so"]);
    }

    #[test]
    fn stale_directory_from_before_stamps_is_cleared() {
        let f = Fixture::new();
        let a = f.src("app", "exe");
        std::fs::create_dir_all(f.dir().join("sub")).unwrap();
        std::fs::write(f.dir().join("old.dll"), "stale").unwrap();
        std::fs::write(f.dir().join("sub/x"), "stale").unwrap();
        stage_dir(Some(&f.cache()), &f.dir(), &[entry("app", &a)]).unwrap();
        assert_eq!(listing(&f.dir()), ["app"]);
    }

    #[test]
    fn read_only_staged_files_are_replaced() {
        let f = Fixture::new();
        let a = f.src("app", "exe");
        let mut perms = std::fs::metadata(&a).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&a, perms).unwrap();
        let b = f.src("other", "other");
        stage_dir(Some(&f.cache()), &f.dir(), &[entry("app", &a)]).unwrap();
        stage_dir(Some(&f.cache()), &f.dir(), &[entry("app", &b)]).unwrap();
        assert_eq!(std::fs::read_to_string(f.dir().join("app")).unwrap(), "other");
    }

    #[test]
    fn no_cache_always_restages() {
        let f = Fixture::new();
        let a = f.src("app", "exe");
        assert_eq!(stage_dir(None, &f.dir(), &[entry("app", &a)]).unwrap(), StageResult::Restaged);
        assert_eq!(stage_dir(None, &f.dir(), &[entry("app", &a)]).unwrap(), StageResult::Restaged);
        assert!(!stamp_path(&f.dir()).exists());
    }

    #[test]
    fn duplicate_names() {
        let f = Fixture::new();
        let a = f.src("a/Foo.dll", "1");
        let b = f.src("b/foo.dll", "2");
        // The same file twice is fine.
        stage_dir(Some(&f.cache()), &f.dir(), &[entry("Foo.dll", &a), entry("Foo.dll", &a)]).unwrap();
        // Two files for one name (ignoring case) are not.
        assert!(stage_dir(Some(&f.cache()), &f.dir(), &[entry("Foo.dll", &a), entry("foo.dll", &b)]).is_err());
        assert!(stage_dir(Some(&f.cache()), &f.dir(), &[entry("../x", &a)]).is_err());
    }

    #[test]
    fn missing_source_is_an_error() {
        let f = Fixture::new();
        let missing = f.root.join("src/nope");
        assert!(stage_dir(Some(&f.cache()), &f.dir(), &[entry("nope", &missing)]).is_err());
    }
}
