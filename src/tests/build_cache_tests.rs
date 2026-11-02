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
