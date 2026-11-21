//! Staged directories: a directory assembled from files that already exist
//! (an executable next to the shared libraries it loads, an application
//! bundle).
//!
//! A stage is a cached step. Its key is the wanted list of (name, source,
//! source digest), written to a stamp file next to the directory once the
//! stage is complete. When the key matches the stamp and every file is
//! still there, the stage is valid and nothing is touched. Otherwise the
//! whole directory is deleted and staged again, so it never holds a file it
//! didn't place: remove a dependency and its library disappears on the next
//! build.

use crate::build_cache::{BuildCache, KeyBuilder};
use crate::{anyhow_loc, bail_loc, function_name};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashMap;

/// Where a staged file's content comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageSource {
    /// A hard link to a build output (a copy where a hard link fails).
    Link(Utf8PathBuf),
    /// A copy, never a hard link. For a file in the workspace, so an edit
    /// inside the staged directory can't change the source; and for Mach-O
    /// images on a Mac (see [`StageEntry::image`]).
    Copy(Utf8PathBuf),
    /// Content generated in memory (an `Info.plist`).
    Bytes(Vec<u8>),
}

/// One file in a stage, at `dir/name`. `name` is a relative path with `/`
/// separators (`app.exe`, `Contents/MacOS/app`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageEntry {
    pub name: String,
    pub source: StageSource,
}

impl StageEntry {
    pub fn link(name: impl Into<String>, src: impl Into<Utf8PathBuf>) -> Self {
        StageEntry { name: name.into(), source: StageSource::Link(src.into()) }
    }

    /// An executable or shared library that is loaded from the staged
    /// directory. A hard link, except on a Mac: dyld resolves `@loader_path`
    /// from the file's path as the kernel reports it, and for a file with
    /// several hard links that can be any of its names (the link dir, a
    /// cache blob), so libraries next to the staged name aren't found. A
    /// copy there is an APFS clone, which costs no space.
    pub fn image(name: impl Into<String>, src: impl Into<Utf8PathBuf>) -> Self {
        let source = if cfg!(target_os = "macos") { StageSource::Copy(src.into()) } else { StageSource::Link(src.into()) };
        StageEntry { name: name.into(), source }
    }

    /// For error messages.
    fn origin(&self) -> &str {
        match &self.source {
            StageSource::Link(src) | StageSource::Copy(src) => src.as_str(),
            StageSource::Bytes(_) => "generated",
        }
    }
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
                kb.field(&e.name);
                match &e.source {
                    StageSource::Link(src) | StageSource::Copy(src) => {
                        let digest = cache
                            .file_digest(src)?
                            .ok_or_else(|| anyhow_loc!("Staging {}: source {} does not exist", dir, src))?;
                        let kind = if matches!(e.source, StageSource::Link(_)) { "link" } else { "copy" };
                        kb.field(kind).field(src.as_str()).digest(&digest);
                    }
                    StageSource::Bytes(data) => {
                        kb.field("bytes").field(data);
                    }
                }
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
                let want = match &e.source {
                    StageSource::Link(src) | StageSource::Copy(src) => std::fs::metadata(src).map(|s| s.len()).ok(),
                    StageSource::Bytes(data) => Some(data.len() as u64),
                };
                matches!((std::fs::metadata(dir.join(&e.name)), want), (Ok(d), Some(len)) if d.is_file() && d.len() == len)
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
        // `dedupe` passed and the directory is new, so a file already here is
        // a name the disk folds together with an earlier one that `dedupe`
        // doesn't (non-ASCII case, Unicode normalization). Writing it would
        // write through that file's hard link into its source.
        if std::fs::symlink_metadata(&dst).is_ok() {
            bail_loc!(
                "Two files want the same name in a staged directory: {} ({}) is the same file on this disk as a name staged before it",
                e.name,
                e.origin()
            );
        }
        match &e.source {
            StageSource::Link(src) => {
                // A hard link to a symlink is the symlink itself, whose
                // relative target dangles here; copy follows it.
                let is_symlink = std::fs::symlink_metadata(src).is_ok_and(|m| m.is_symlink());
                if is_symlink || std::fs::hard_link(src, &dst).is_err() {
                    std::fs::copy(src, &dst).with_context(|| format!("copying {src} to {dst}"))?;
                }
                // The link changed the source's ctime; tell the stat cache,
                // or the next build hashes it again.
                if let Some(cache) = cache {
                    if let Some(digest) = cache.file_digest(src)? {
                        cache.set_known_digest(src, digest);
                    }
                }
            }
            StageSource::Copy(src) => {
                std::fs::copy(src, &dst).with_context(|| format!("copying {src} to {dst}"))?;
            }
            StageSource::Bytes(data) => {
                std::fs::write(&dst, data).with_context(|| format!("writing {dst}"))?;
            }
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
/// (they would overwrite each other), and so is a name that is both a file
/// and a directory (`a` and `a/b`). Names compare ignoring ASCII case, since
/// Windows and macOS file systems do.
fn dedupe(entries: &[StageEntry]) -> anyhow::Result<Vec<StageEntry>> {
    let mut seen: HashMap<String, &StageEntry> = HashMap::new();
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let plain = |c: &str| !c.is_empty() && c != "." && c != ".." && !c.contains(['\\', ':']);
        if !e.name.split('/').all(plain) {
            bail_loc!("Bad staged file name [{}] for {}", e.name, e.origin());
        }
        match seen.get(&e.name.to_ascii_lowercase()) {
            Some(prev) if **prev == *e => continue,
            Some(prev) => bail_loc!(
                "Two files want the same name in a staged directory: {} ({}) and {} ({})",
                prev.name,
                prev.origin(),
                e.name,
                e.origin()
            ),
            None => {
                seen.insert(e.name.to_ascii_lowercase(), e);
                out.push(e.clone());
            }
        }
    }
    for e in &out {
        let lower = e.name.to_ascii_lowercase();
        let mut at = 0;
        while let Some(i) = lower[at..].find('/') {
            at += i;
            if let Some(file) = seen.get(&lower[..at]) {
                bail_loc!("Staged name {} is a file, but {} needs it to be a directory", file.name, e.name);
            }
            at += 1;
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
        } else if let Err(e) = std::fs::remove_file(&path) {
            // A directory symlink on Windows is removed as a directory.
            if std::fs::remove_dir(&path).is_err() {
                let hint = if cfg!(windows) { " (is the program still running?)" } else { "" };
                return Err(anyhow::Error::from(e).context(format!("removing {path}{hint}")));
            }
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
        StageEntry::link(name, src)
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
    fn nested_names_copies_and_bytes() {
        let f = Fixture::new();
        let exe = f.src("app", "exe");
        let res = f.src("hello.txt", "hello");
        let plist = |text: &str| StageEntry { name: "Contents/Info.plist".into(), source: StageSource::Bytes(text.into()) };
        let entries = |text: &str| {
            vec![
                plist(text),
                entry("Contents/MacOS/app", &exe),
                StageEntry { name: "Contents/Resources/hello.txt".into(), source: StageSource::Copy(res.clone()) },
            ]
        };
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &entries("v1")).unwrap(), StageResult::Restaged);
        assert_eq!(std::fs::read_to_string(f.dir().join("Contents/Info.plist")).unwrap(), "v1");
        assert_eq!(std::fs::read_to_string(f.dir().join("Contents/MacOS/app")).unwrap(), "exe");
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &entries("v1")).unwrap(), StageResult::Valid);

        // A copy is its own file: writing to it leaves the source alone.
        let staged = f.dir().join("Contents/Resources/hello.txt");
        std::fs::write(&staged, "edited").unwrap();
        assert_eq!(std::fs::read_to_string(&res).unwrap(), "hello");

        // Generated content of the same length still changes the key.
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &entries("v2")).unwrap(), StageResult::Restaged);
        assert_eq!(std::fs::read_to_string(f.dir().join("Contents/Info.plist")).unwrap(), "v2");
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "hello");

        // A file can't also be a directory, and names stay inside the stage.
        assert!(stage_dir(Some(&f.cache()), &f.dir(), &[entry("a", &exe), entry("A/b", &exe)]).is_err());
        for bad in ["/abs", "a//b", "a/../b", "a/", "c:x", "a\\b"] {
            assert!(stage_dir(Some(&f.cache()), &f.dir(), &[entry(bad, &exe)]).is_err(), "{bad}");
        }
    }

    /// A versioned library (`libfoo.so -> libfoo.so.1`) stages as the file,
    /// not as a symlink whose relative target doesn't exist in `bin/`.
    #[cfg(unix)]
    #[test]
    fn symlinked_source_stages_the_file() {
        let f = Fixture::new();
        f.src("libfoo.so.1", "lib");
        let link = f.root.join("src/libfoo.so");
        std::os::unix::fs::symlink("libfoo.so.1", &link).unwrap();
        let entries = [entry("libfoo.so", &link)];
        stage_dir(Some(&f.cache()), &f.dir(), &entries).unwrap();
        let staged = f.dir().join("libfoo.so");
        assert!(std::fs::symlink_metadata(&staged).unwrap().is_file());
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "lib");
        assert_eq!(stage_dir(Some(&f.cache()), &f.dir(), &entries).unwrap(), StageResult::Valid);
    }

    /// Names that differ only by non-ASCII case pass `dedupe` but are one
    /// file on Windows and macOS disks. The second must not write through
    /// the first, a hard link to its source (a cache blob).
    #[test]
    fn non_ascii_case_collision_never_writes_through() {
        let f = Fixture::new();
        let a = f.src("a/É.dll", "first");
        let b = f.src("b/é.dll", "second");
        match stage_dir(Some(&f.cache()), &f.dir(), &[entry("É.dll", &a), entry("é.dll", &b)]) {
            // A case-sensitive disk: two files.
            Ok(_) => {
                assert_eq!(std::fs::read_to_string(f.dir().join("É.dll")).unwrap(), "first");
                assert_eq!(std::fs::read_to_string(f.dir().join("é.dll")).unwrap(), "second");
            }
            Err(e) => assert!(format!("{e:#}").contains("same name"), "{e:#}"),
        }
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "first");
    }

    /// Staging hard-links the sources, which changes their ctime. The stat
    /// cache is told, so the next build doesn't hash them again.
    #[test]
    fn restage_does_not_cause_a_rehash() {
        let f = Fixture::new();
        let a = f.src("app", "exe");
        // Old enough for the stat cache to trust.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(&a).unwrap().set_modified(old).unwrap();
        let cache = || f.cache().with_stat_cache(f.root.join(".anubis-cache/statcache"));
        let entries = [entry("app", &a)];
        // So the link lands in a later ctime tick than the write.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let first = cache();
        assert_eq!(stage_dir(Some(&first), &f.dir(), &entries).unwrap(), StageResult::Restaged);
        first.save();
        let second = cache();
        assert_eq!(stage_dir(Some(&second), &f.dir(), &entries).unwrap(), StageResult::Valid);
        assert_eq!(second.stat_counts().0, 0, "files rehashed");
    }

    #[test]
    fn missing_source_is_an_error() {
        let f = Fixture::new();
        let missing = f.root.join("src/nope");
        assert!(stage_dir(Some(&f.cache()), &f.dir(), &[entry("nope", &missing)]).is_err());
    }
}
