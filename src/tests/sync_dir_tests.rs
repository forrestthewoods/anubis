//! Tests for `src/rules/sync_dir.rs`.

use super::*;
use crate::build_cache::LocalDiskBackend;
use tempfile::TempDir;

struct Ws {
    _tmp: TempDir,
    ws: Utf8PathBuf,
    cache: BuildCache,
}

fn setup() -> Ws {
    let tmp = TempDir::new().unwrap();
    // Canonical, so the owned-root check compares like with like (macOS
    // temp dirs sit behind a symlink).
    // Not on Windows, where `canonicalize` gives a `\\?\` path.
    let base = if cfg!(windows) { tmp.path().to_path_buf() } else { tmp.path().canonicalize().unwrap() };
    let base = Utf8PathBuf::from_path_buf(base).unwrap();
    let ws = base.join("ws").slash_fix();
    std::fs::create_dir_all(ws.join(".anubis-out")).unwrap();
    let backend = LocalDiskBackend::new(base.join("cache")).unwrap();
    let cache = BuildCache::new(&ws, Box::new(backend)).with_stat_cache(base.join("statcache"));
    Ws { _tmp: tmp, ws, cache }
}

impl Ws {
    fn file(&self, rel: &str, content: &str) -> Utf8PathBuf {
        let p = self.ws.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        self.cache.invalidate(&p);
        p
    }

    fn bundle(&self) -> Utf8PathBuf {
        self.ws.join(".anubis-out/mode/pkg/app/Hello.app")
    }

    fn sync(&self, entries: &[Entry]) -> anyhow::Result<SyncReport> {
        sync_dir(&self.ws, Some(&self.cache), &self.bundle(), entries, true)
    }
}

fn entry(dst: &str, source: Source) -> Entry {
    Entry { dst: dst.into(), source }
}

/// (path, modification time, inode) of every file under `dir`.
fn snapshot(dir: &Utf8Path) -> Vec<(String, std::time::SystemTime, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let p = Utf8PathBuf::from_path_buf(e.path()).unwrap();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                stack.push(p);
            } else {
                #[cfg(unix)]
                let ino = std::os::unix::fs::MetadataExt::ino(&m);
                #[cfg(not(unix))]
                let ino = 0;
                out.push((p.to_string(), m.modified().unwrap(), ino));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn places_links_copies_and_bytes() {
    let w = setup();
    let exe = w.file(".anubis-out/mode/pkg/bin/main", "binary");
    let res = w.file("pkg/resources/a.txt", "resource");
    let report = w
        .sync(&[
            entry("Contents/MacOS/main", Source::Link(exe.clone())),
            entry("Contents/Resources/resources/a.txt", Source::Copy(res.clone())),
            entry("Contents/Info.plist", Source::Bytes(b"plist".to_vec())),
        ])
        .unwrap();
    assert_eq!((report.written, report.removed), (3, 0));
    let b = w.bundle();
    assert_eq!(std::fs::read_to_string(b.join("Contents/MacOS/main")).unwrap(), "binary");
    assert_eq!(std::fs::read_to_string(b.join("Contents/Resources/resources/a.txt")).unwrap(), "resource");
    assert_eq!(std::fs::read_to_string(b.join("Contents/Info.plist")).unwrap(), "plist");
    assert_eq!(report.files[0].path, b.join("Contents/MacOS/main"));
    assert_eq!(report.files[0].digest, Digest::of_bytes(b"binary"));
    assert_eq!(report.files[2].digest, Digest::of_bytes(b"plist"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ino = |p: &Utf8Path| std::fs::metadata(p).unwrap().ino();
        assert_eq!(ino(&exe), ino(&b.join("Contents/MacOS/main")), "build outputs are hard links");
        assert_ne!(ino(&res), ino(&b.join("Contents/Resources/resources/a.txt")), "sources are copies");
    }
    // Nothing is left in the temp dir.
    assert_eq!(std::fs::read_dir(w.ws.join(".anubis-temp/place")).unwrap().count(), 0);
}

#[test]
fn second_sync_writes_nothing() {
    let w = setup();
    let exe = w.file(".anubis-out/mode/pkg/bin/main", "binary");
    let res = w.file("pkg/a.txt", "resource");
    let entries = [
        entry("MacOS/main", Source::Link(exe)),
        entry("Resources/a.txt", Source::Copy(res)),
        entry("Info.plist", Source::Bytes(b"plist".to_vec())),
    ];
    w.sync(&entries).unwrap();
    let before = snapshot(&w.ws);
    let report = w.sync(&entries).unwrap();
    assert_eq!((report.written, report.removed), (0, 0));
    assert_eq!(report.files.len(), 3);
    assert_eq!(snapshot(&w.ws), before);
}

#[test]
fn changed_and_damaged_files_are_replaced() {
    let w = setup();
    let res = w.file("pkg/a.txt", "one");
    let other = w.file("pkg/b.txt", "same");
    let entries = [entry("a.txt", Source::Copy(res.clone())), entry("b.txt", Source::Copy(other))];
    w.sync(&entries).unwrap();

    // The source changes: only that file is rewritten.
    w.file("pkg/a.txt", "two!");
    assert_eq!(w.sync(&entries).unwrap().written, 1);
    assert_eq!(std::fs::read_to_string(w.bundle().join("a.txt")).unwrap(), "two!");

    // Someone edits the output (same size): it is put back.
    let placed = w.bundle().join("a.txt");
    std::fs::write(&placed, "XXXX").unwrap();
    w.cache.invalidate(&placed);
    assert_eq!(w.sync(&entries).unwrap().written, 1);
    assert_eq!(std::fs::read_to_string(&placed).unwrap(), "two!");

    // Someone deletes it.
    std::fs::remove_file(&placed).unwrap();
    w.cache.invalidate(&placed);
    assert_eq!(w.sync(&entries).unwrap().written, 1);
    assert_eq!(std::fs::read_to_string(&placed).unwrap(), "two!");
}

#[test]
fn owned_sync_removes_what_is_not_wanted() {
    let w = setup();
    let a = w.file("pkg/a.txt", "a");
    let b = w.file("pkg/b.txt", "b");
    w.sync(&[entry("Resources/a.txt", Source::Copy(a.clone())), entry("Resources/sub/b.txt", Source::Copy(b))])
        .unwrap();
    std::fs::write(w.bundle().join("stray.txt"), "x").unwrap();

    let report = w.sync(&[entry("Resources/a.txt", Source::Copy(a))]).unwrap();
    assert_eq!((report.written, report.removed), (0, 2));
    assert!(!w.bundle().join("stray.txt").exists());
    assert!(!w.bundle().join("Resources/sub").exists(), "empty directories go too");
    assert!(w.bundle().join("Resources/a.txt").exists());
}

#[test]
fn unowned_sync_leaves_other_files() {
    let w = setup();
    let a = w.file("pkg/liba.so", "a");
    let bin = w.ws.join(".anubis-out/mode/pkg/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("main"), "exe").unwrap();
    let report = sync_dir(&w.ws, Some(&w.cache), &bin, &[entry("liba.so", Source::Link(a))], false).unwrap();
    assert_eq!((report.written, report.removed), (1, 0));
    assert!(bin.join("main").exists());
}

#[test]
fn a_file_and_a_directory_can_swap_places() {
    let w = setup();
    let a = w.file("pkg/a.txt", "a");
    w.sync(&[entry("x/a.txt", Source::Copy(a.clone()))]).unwrap();
    // `x` was a directory, now it is a file.
    w.sync(&[entry("x", Source::Copy(a.clone()))]).unwrap();
    assert!(w.bundle().join("x").is_file());
    // And back.
    w.sync(&[entry("x/a.txt", Source::Copy(a))]).unwrap();
    assert!(w.bundle().join("x/a.txt").is_file());
}

#[test]
fn name_differing_only_by_case_is_renamed() {
    let w = setup();
    let a = w.file("pkg/a.txt", "a");
    w.sync(&[entry("Icon.txt", Source::Copy(a.clone()))]).unwrap();
    w.sync(&[entry("icon.txt", Source::Copy(a))]).unwrap();
    let names: Vec<String> =
        std::fs::read_dir(w.bundle()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, ["icon.txt"]);
}

#[test]
fn rejects_bad_lists() {
    let w = setup();
    let a = w.file("pkg/a.txt", "a");
    let b = w.file("pkg/b.txt", "b");
    let err = |entries: &[Entry]| format!("{:#}", w.sync(entries).unwrap_err());

    // Two sources for one destination, also when only the case differs.
    let e = err(&[entry("lib/Foo.dll", Source::Link(a.clone())), entry("lib/foo.dll", Source::Link(b.clone()))]);
    assert!(e.contains("two files would be placed") && e.contains("a.txt") && e.contains("b.txt"), "{e}");
    // Paths that leave the directory.
    for bad in ["../x", "/abs", "a/../../x", ""] {
        assert!(err(&[entry(bad, Source::Link(a.clone()))]).contains("not a plain relative path"), "{bad}");
    }
    // A file that is also a directory.
    let e = err(&[entry("x", Source::Link(a.clone())), entry("x/y", Source::Link(b))]);
    assert!(e.contains("both a file and a directory"), "{e}");
    // A missing source fails before anything is written or removed.
    std::fs::create_dir_all(w.bundle()).unwrap();
    std::fs::write(w.bundle().join("keep.txt"), "k").unwrap();
    let e = err(&[entry("a.txt", Source::Link(a)), entry("gone", Source::Link(w.ws.join("pkg/gone")))]);
    assert!(e.contains("pkg/gone"), "{e}");
    assert!(w.bundle().join("keep.txt").exists());
    assert!(!w.bundle().join("a.txt").exists());
}

#[test]
fn owned_root_must_be_under_anubis_out() {
    let w = setup();
    let a = w.file("pkg/a.txt", "a");
    let precious = w.file("pkg/precious.txt", "keep me");
    let entries = [entry("a.txt", Source::Copy(a))];
    for root in [w.ws.join("pkg"), w.ws.join(".anubis-out"), w.ws.join(".anubis-out/mode/../../pkg")] {
        let e = format!("{:#}", sync_dir(&w.ws, Some(&w.cache), &root, &entries, true).unwrap_err());
        assert!(e.contains("refusing to sync"), "{e}");
    }
    assert!(precious.exists());
}

#[cfg(unix)]
#[test]
fn sweep_does_not_follow_symlinks() {
    let w = setup();
    let a = w.file("pkg/a.txt", "a");
    let precious = w.file("pkg/assets/precious.txt", "keep me");
    w.sync(&[entry("a.txt", Source::Copy(a.clone()))]).unwrap();
    // A link to a source directory inside the bundle, and one at a wanted path.
    std::os::unix::fs::symlink(w.ws.join("pkg/assets"), w.bundle().join("assets")).unwrap();
    std::fs::remove_file(w.bundle().join("a.txt")).unwrap();
    std::os::unix::fs::symlink(&a, w.bundle().join("a.txt")).unwrap();
    w.cache.invalidate(&w.bundle().join("a.txt"));

    let report = w.sync(&[entry("a.txt", Source::Copy(a.clone()))]).unwrap();
    assert_eq!((report.written, report.removed), (1, 1));
    assert!(precious.exists(), "the link's target is untouched");
    assert!(!w.bundle().join("assets").exists());
    assert!(std::fs::symlink_metadata(w.bundle().join("a.txt")).unwrap().is_file(), "a link is never in place");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "a");

    // A bundle root that is itself a link is refused, wherever it points:
    // out of `.anubis-out`, or at another target's output inside it.
    let other = w.file(".anubis-out/mode/other/bin/main", "other target");
    for target in [w.ws.join("pkg"), w.ws.join(".anubis-out/mode/other/bin")] {
        std::fs::remove_dir_all(w.bundle()).ok();
        std::fs::remove_file(w.bundle()).ok();
        std::os::unix::fs::symlink(&target, w.bundle()).unwrap();
        let e = format!("{:#}", w.sync(&[entry("a.txt", Source::Copy(a.clone()))]).unwrap_err());
        assert!(e.contains("is a symlink"), "{e}");
    }
    // So is one behind a linked parent directory.
    std::fs::remove_file(w.bundle()).unwrap();
    std::fs::remove_dir_all(w.ws.join(".anubis-out/mode/pkg")).unwrap();
    std::os::unix::fs::symlink(w.ws.join(".anubis-out/mode/other"), w.ws.join(".anubis-out/mode/pkg")).unwrap();
    let e = format!("{:#}", w.sync(&[entry("a.txt", Source::Copy(a))]).unwrap_err());
    assert!(e.contains("is a symlink"), "{e}");
    assert!(precious.exists() && other.exists());
}

#[cfg(unix)]
#[test]
fn executable_bit_follows_the_source() {
    use std::os::unix::fs::PermissionsExt;
    let w = setup();
    let a = w.file("pkg/run.sh", "#!/bin/sh\n");
    let entries = [entry("run.sh", Source::Copy(a.clone()))];
    w.sync(&entries).unwrap();
    let mode = |p: &Utf8Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&w.bundle().join("run.sh")), 0o644);

    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o755)).unwrap();
    let report = w.sync(&entries).unwrap();
    assert_eq!(report.written, 1);
    assert!(report.files[0].executable);
    assert_eq!(mode(&w.bundle().join("run.sh")), 0o755);
}

#[cfg(unix)]
#[test]
fn a_copy_is_never_a_hard_link_to_its_source() {
    let w = setup();
    let a = w.file("pkg/a.txt", "a");
    std::fs::create_dir_all(w.bundle()).unwrap();
    std::fs::hard_link(&a, w.bundle().join("a.txt")).unwrap();
    assert_eq!(w.sync(&[entry("a.txt", Source::Copy(a.clone()))]).unwrap().written, 1);
    std::fs::write(w.bundle().join("a.txt"), "edited").unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "a");
}

#[test]
fn works_without_a_cache() {
    let w = setup();
    let a = w.file("pkg/a.txt", "a");
    let entries = [entry("a.txt", Source::Copy(a)), entry("b", Source::Bytes(b"b".to_vec()))];
    assert_eq!(sync_dir(&w.ws, None, &w.bundle(), &entries, true).unwrap().written, 2);
    assert_eq!(sync_dir(&w.ws, None, &w.bundle(), &entries, true).unwrap().written, 0);
}
