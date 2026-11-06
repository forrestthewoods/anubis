//! Tests for `src/build_cache.rs`.

use super::*;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

fn utf8(p: &std::path::Path) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap()
}

fn setup() -> (TempDir, Utf8PathBuf, BuildCache) {
    let tmp = TempDir::new().unwrap();
    let ws = utf8(tmp.path()).join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let backend = LocalDiskBackend::new(utf8(tmp.path()).join("cache")).unwrap();
    let cache = BuildCache::new(&ws, Box::new(backend));
    (tmp, ws, cache)
}

#[test]
fn depfile_basic() {
    let d = "/ws/out/main.obj: /ws/src/main.cpp \\\n  /ws/inc/a.h /ws/inc/b.h \\\n  /ws/inc/c.h\n";
    assert_eq!(
        parse_depfile(d),
        vec!["/ws/src/main.cpp", "/ws/inc/a.h", "/ws/inc/b.h", "/ws/inc/c.h"]
    );
}

#[test]
fn depfile_first_dependency_on_target_line() {
    // The old line-based parser skipped the whole first line and lost this.
    let d = "main.o: main.c foo.h\n";
    assert_eq!(parse_depfile(d), vec!["main.c", "foo.h"]);
}

#[test]
fn depfile_escapes_and_drive_letters() {
    let d = "C:/out/a\\ b.obj: C:\\src\\my\\ file.cpp C:/inc/x$$y.h \\\r\n  C:/inc/h\\#1.h\r\n";
    assert_eq!(
        parse_depfile(d),
        vec!["C:\\src\\my file.cpp", "C:/inc/x$y.h", "C:/inc/h#1.h"]
    );
}

#[test]
fn normalize_rewrites_workspace_and_out_dir() {
    let (_tmp, ws, cache) = setup();
    let out = ws.join(".anubis-out/m/t/cc_build/main.c");
    assert_eq!(cache.normalize_arg(&format!("-I{ws}/inc"), None), "-Iws:inc");
    assert_eq!(cache.normalize_arg(&format!("/LIBPATH:{ws}/lib"), None), "/LIBPATH:ws:lib");
    assert_eq!(cache.normalize_arg(&format!("{out}/main.obj"), Some(&out)), "out:main.obj");
    assert_eq!(cache.normalize_arg("-O2", Some(&out)), "-O2");
}

#[test]
fn key_depends_on_args_and_input_content() {
    let (_tmp, ws, cache) = setup();
    let tool = ws.join("tool");
    std::fs::write(&tool, b"tool v1").unwrap();
    let src = ws.join("a.c");
    std::fs::write(&src, b"int a;").unwrap();

    let key = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut kb = cache.key_for_command("t", &tool, &args, None).unwrap();
        assert!(cache.key_add_files(&mut kb, &[src.clone()]).unwrap());
        kb.finish()
    };
    let k1 = key(&["-O2"]);
    assert_eq!(k1, key(&["-O2"]));
    assert_ne!(k1, key(&["-O0"]));
    // Field boundaries matter.
    assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));

    std::fs::write(&src, b"int b;").unwrap();
    cache.invalidate(&src);
    assert_ne!(k1, key(&["-O2"]));
}

#[test]
fn key_is_independent_of_workspace_location() {
    let mk = || {
        let (tmp, ws, cache) = setup();
        let tool = ws.join("tool");
        std::fs::write(&tool, b"tool").unwrap();
        let src = ws.join("a.c");
        std::fs::write(&src, b"int a;").unwrap();
        let mut kb = cache
            .key_for_command("t", &tool, &[format!("-I{ws}/inc"), src.to_string()], None)
            .unwrap();
        cache.key_add_files(&mut kb, &[src]).unwrap();
        (tmp, kb.finish())
    };
    let (_a, k1) = mk();
    let (_b, k2) = mk();
    assert_eq!(k1, k2);
}

#[test]
fn store_and_restore_round_trip() {
    let (_tmp, ws, cache) = setup();
    let out = ws.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let obj = out.join("main.obj");
    std::fs::write(&obj, b"object bytes").unwrap();
    let key = Digest::of_bytes(b"some action");

    assert!(!cache.restore(&key, &out).unwrap());
    cache
        .store(
            &key,
            &out,
            &[
                ActionOutput { path: obj.clone(), optional: false },
                ActionOutput { path: out.join("main.pdb"), optional: true },
            ],
        )
        .unwrap();

    std::fs::remove_file(&obj).unwrap();
    cache.invalidate(&obj);
    assert!(cache.restore(&key, &out).unwrap());
    assert_eq!(std::fs::read(&obj).unwrap(), b"object bytes");

    // Restoring into a different directory also works (outputs are relative).
    let other = ws.join("other");
    assert!(cache.restore(&key, &other).unwrap());
    assert_eq!(std::fs::read(other.join("main.obj")).unwrap(), b"object bytes");
}

#[test]
fn manifest_matches_only_when_all_inputs_match() {
    let (_tmp, ws, cache) = setup();
    let h = ws.join("a.h");
    std::fs::write(&h, b"v1").unwrap();
    let mk = Digest::of_bytes(b"manifest");

    let (rk1, rec1) = cache.compile_result_key(&mk, &[h.clone()]).unwrap().unwrap();
    cache.record_manifest(&mk, rec1, &rk1).unwrap();
    assert_eq!(cache.lookup_manifest(&mk).unwrap(), Some(rk1));

    std::fs::write(&h, b"v2").unwrap();
    cache.invalidate(&h);
    assert_eq!(cache.lookup_manifest(&mk).unwrap(), None);

    let (rk2, rec2) = cache.compile_result_key(&mk, &[h.clone()]).unwrap().unwrap();
    cache.record_manifest(&mk, rec2, &rk2).unwrap();
    assert_eq!(cache.lookup_manifest(&mk).unwrap(), Some(rk2));

    // Reverting the header finds the older entry again.
    std::fs::write(&h, b"v1").unwrap();
    cache.invalidate(&h);
    assert_eq!(cache.lookup_manifest(&mk).unwrap(), Some(rk1));

    // A deleted input is a miss, not an error.
    std::fs::remove_file(&h).unwrap();
    cache.invalidate(&h);
    assert_eq!(cache.lookup_manifest(&mk).unwrap(), None);
}

fn age(path: &Utf8Path, secs: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(secs)).unwrap();
}

#[test]
fn stat_cache_reuses_unchanged_files_across_runs() {
    let (_tmp, ws, _cache) = setup();
    let f = ws.join("a.h");
    std::fs::write(&f, b"v1").unwrap();
    age(&f, 60);
    let sc_path = ws.join("statcache");

    let sc = StatCache::load(sc_path.clone());
    let d1 = sc.digest(&f).unwrap().unwrap();
    assert_eq!(sc.rehashed.load(Ordering::Relaxed), 1);
    sc.save().unwrap();

    // New "run": reused without hashing.
    let sc = StatCache::load(sc_path.clone());
    assert_eq!(sc.digest(&f).unwrap(), Some(d1));
    assert_eq!(sc.reused.load(Ordering::Relaxed), 1);
    assert_eq!(sc.rehashed.load(Ordering::Relaxed), 0);

    // Edit between runs, even keeping the same size and an old mtime:
    // ctime/mtime change, so it rehashes.
    std::fs::write(&f, b"v2").unwrap();
    age(&f, 30);
    let sc = StatCache::load(sc_path.clone());
    let d2 = sc.digest(&f).unwrap().unwrap();
    assert_ne!(d1, d2);
    assert_eq!(sc.rehashed.load(Ordering::Relaxed), 1);

    // Deleted file is reported missing, not served from the cache.
    std::fs::remove_file(&f).unwrap();
    assert_eq!(sc.digest(&f).unwrap(), None);
}

#[test]
fn stat_cache_does_not_trust_racily_clean_entries() {
    let (_tmp, ws, _cache) = setup();
    let f = ws.join("fresh.h");
    std::fs::write(&f, b"just written").unwrap(); // mtime ~ now
    let sc_path = ws.join("statcache");

    let sc = StatCache::load(sc_path.clone());
    sc.digest(&f).unwrap();
    sc.save().unwrap();

    // The entry was recorded within the racy window of the file's mtime, so
    // the next run must rehash rather than trust it.
    let sc = StatCache::load(sc_path);
    sc.digest(&f).unwrap();
    assert_eq!(sc.reused.load(Ordering::Relaxed), 0);
    assert_eq!(sc.rehashed.load(Ordering::Relaxed), 1);
}

#[test]
fn stat_cache_ignores_garbage_file() {
    let (_tmp, ws, _cache) = setup();
    let p = ws.join("statcache");
    std::fs::write(&p, "not a stat cache\n1 2 3").unwrap();
    let sc = StatCache::load(p);
    let f = ws.join("x");
    std::fs::write(&f, b"x").unwrap();
    assert!(sc.digest(&f).unwrap().is_some());
}

#[test]
fn replace_all_case_insensitive_for_windows_paths() {
    let s = "-Ic:/work/anubis/inc C:/Work/Anubis/src/a.cpp";
    assert_eq!(
        replace_all(s, "C:/Work/anubis/", "ws:", true),
        "-Iws:inc ws:src/a.cpp"
    );
    // Case-sensitive mode leaves other spellings alone.
    assert_eq!(
        replace_all(s, "C:/Work/anubis/", "ws:", false),
        s
    );
}

#[cfg(unix)]
#[test]
fn store_and_restore_hard_link_read_only_outputs() {
    use std::os::unix::fs::MetadataExt;
    let (_tmp, ws, cache) = setup();
    let out = ws.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let obj = out.join("main.obj");
    std::fs::write(&obj, b"object bytes").unwrap();
    let key = Digest::of_bytes(b"hard link action");
    cache.store(&key, &out, &[ActionOutput { path: obj.clone(), optional: false }]).unwrap();

    // The output and its blob are one read-only file.
    let meta = std::fs::metadata(&obj).unwrap();
    assert_eq!(meta.nlink(), 2);
    assert!(meta.permissions().readonly());
    // (root ignores permission bits, so only check this as a normal user.)
    if unsafe { libc_geteuid() } != 0 {
        assert!(std::fs::write(&obj, b"edited in place").is_err());
    }

    // Tools replace outputs by rename, which still works and leaves the
    // cached blob untouched.
    let tmp_obj = out.join("main.obj.tmp");
    std::fs::write(&tmp_obj, b"new object").unwrap();
    std::fs::rename(&tmp_obj, &obj).unwrap();
    cache.invalidate(&obj);

    let other = ws.join("other");
    assert!(cache.restore(&key, &other).unwrap());
    let restored = other.join("main.obj");
    assert_eq!(std::fs::read(&restored).unwrap(), b"object bytes");
    assert_eq!(std::fs::metadata(&restored).unwrap().nlink(), 2);
}

#[cfg(unix)]
unsafe fn libc_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    geteuid()
}

/// The flow every action uses, on every platform: outputs are read-only
/// links to blobs; before re-running, the old outputs are removed, so the
/// tool writes a fresh file and the blob is untouched. Restoring over an
/// existing read-only output also works (Windows can't rename onto one).
#[test]
fn remove_outputs_then_rebuild_keeps_blob_and_restores_over_read_only() {
    let (_tmp, ws, cache) = setup();
    let out = ws.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let obj = out.join("main.obj");
    std::fs::write(&obj, b"object v1").unwrap();
    let key1 = Digest::of_bytes(b"action v1");
    cache.store(&key1, &out, &[ActionOutput { path: obj.clone(), optional: false }]).unwrap();
    assert!(std::fs::metadata(&obj).unwrap().permissions().readonly());

    // Re-run: remove the read-only output, then the "tool" writes in place.
    remove_outputs([obj.as_path()]).unwrap();
    remove_outputs([obj.as_path()]).unwrap(); // already gone: fine
    std::fs::write(&obj, b"object v2").unwrap();
    let key2 = Digest::of_bytes(b"action v2");
    cache.store(&key2, &out, &[ActionOutput { path: obj.clone(), optional: false }]).unwrap();

    // Switching back restores v1 over the read-only v2 output.
    assert!(cache.restore(&key1, &out).unwrap());
    assert_eq!(std::fs::read(&obj).unwrap(), b"object v1");
    assert!(cache.restore(&key2, &out).unwrap());
    assert_eq!(std::fs::read(&obj).unwrap(), b"object v2");
}

/// The manifest key's include-dir part, as a fresh run (new cache) sees it.
fn listing_key(ws: &Utf8Path, args: &[String], src: &Utf8Path) -> Digest {
    let backend = LocalDiskBackend::new(ws.join("../cache2")).unwrap();
    let cache = BuildCache::new(ws, Box::new(backend));
    let mut kb = KeyBuilder::new("t");
    cache.key_add_include_listings(&mut kb, args, src).unwrap();
    kb.finish()
}

#[test]
fn include_listing_changes_when_a_header_appears_or_moves() {
    let (_tmp, ws, _cache) = setup();
    let write = |rel: &str| {
        let p = ws.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, rel).unwrap();
    };
    write("pkg/main.cpp");
    write("inc_b/foo.h");
    let src = ws.join("pkg/main.cpp");
    let args = vec![format!("-I{ws}/inc_a"), "-I".into(), format!("{ws}/inc_b"), "-o".into(), "x".into()];
    let k0 = listing_key(&ws, &args, &src);

    // Edits and new sources keep the key: contents are checked via the .d file.
    std::fs::write(ws.join("inc_b/foo.h"), "edited").unwrap();
    write("pkg/other.cpp");
    write("pkg/sub/ANUBIS"); // another package: not part of pkg's listing
    write("pkg/sub/x.h");
    write("pkg/.git/HEAD");
    write("pkg/notes.txt"); // not a header
    write("inc_b/foo.h~"); // editor backup
    write("inc_b/.foo.h.swp"); // editor swap file
    assert_eq!(listing_key(&ws, &args, &src), k0);

    // -Iinc_a didn't exist; creating it with a header that shadows inc_b/foo.h is a change.
    write("inc_a/foo.h");
    let k1 = listing_key(&ws, &args, &src);
    assert_ne!(k1, k0);
    // So is a new header in a subdir (a `__has_include(<sys/opt.h>)` target)...
    write("inc_b/sys/opt.h");
    let k2 = listing_key(&ws, &args, &src);
    assert_ne!(k2, k1);
    // ...and one next to the source (searched first for #include "foo.h")...
    write("pkg/foo.h");
    let k3 = listing_key(&ws, &args, &src);
    assert_ne!(k3, k2);
    // ...and an extensionless one (like <vector>).
    write("pkg/version");
    assert_ne!(listing_key(&ws, &args, &src), k3);
    // A project dir passed with -isystem is listed too.
    let mut sys_args = args.clone();
    sys_args.extend(["-isystem".to_owned(), format!("{ws}/sys")]);
    let k5 = listing_key(&ws, &sys_args, &src);
    write("sys/opt.h");
    assert_ne!(listing_key(&ws, &sys_args, &src), k5);
}

#[test]
fn invalidate_forgets_listings_of_parent_dirs() {
    let (_tmp, ws, cache) = setup();
    std::fs::create_dir_all(ws.join("inc/gen")).unwrap();
    let before = cache.dir_listing(&ws.join("inc")).unwrap();
    std::fs::write(ws.join("inc/gen/config.h"), "").unwrap();
    assert_eq!(cache.dir_listing(&ws.join("inc")).unwrap(), before, "memoized for the run");
    cache.invalidate(&ws.join("inc/gen/config.h"));
    assert_ne!(cache.dir_listing(&ws.join("inc")).unwrap(), before);
}
