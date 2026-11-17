//! Tests for `src/gc.rs`.

use crate::build_cache::{ActionOutput, BuildCache, Digest, KeyBuilder};
use crate::gc::{self, GcOptions, GcReport, STAMP_REFRESH, USED_STAMP};
use camino::{Utf8Path, Utf8PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const HOUR: Duration = Duration::from_secs(60 * 60);

struct Ws {
    _tmp: TempDir,
    root: Utf8PathBuf,
    cache: BuildCache,
}

fn ws() -> Ws {
    let tmp = TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let cache = BuildCache::local(&root).unwrap();
    Ws { _tmp: tmp, root, cache }
}

fn key(name: &str) -> Digest {
    let mut kb = KeyBuilder::new("gc_test");
    kb.field(name);
    kb.finish()
}

/// Works on read-only cache blobs too (they can't be opened for writing).
#[allow(clippy::permissions_set_readonly_false)]
fn backdate(path: &Utf8Path, age: Duration) {
    let original = std::fs::metadata(path).unwrap().permissions();
    let mut writable = original.clone();
    writable.set_readonly(false);
    std::fs::set_permissions(path, writable).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - age).unwrap();
    drop(f);
    std::fs::set_permissions(path, original).unwrap();
}

impl Ws {
    fn target_dir(&self, mode: &str, target: &str) -> Utf8PathBuf {
        self.root.join(".anubis-out").join(mode).join(target)
    }

    fn ac_path(&self, key: &Digest) -> Utf8PathBuf {
        let hex = key.hex();
        self.root.join(".anubis-cache/ac").join(&hex[..2]).join(hex)
    }

    fn blob_path(&self, content: &str) -> Utf8PathBuf {
        let hex = Digest::of_bytes(content.as_bytes()).hex();
        self.root.join(".anubis-cache/cas").join(&hex[..2]).join(hex)
    }

    /// Build "an action" of `target` in `mode`: write `lib/out.bin`, store it
    /// under `key(target)`, stamp the directory. Everything is `age` old.
    fn build(&self, mode: &str, target: &str, content: &str, age: Duration) -> Utf8PathBuf {
        let dir = self.target_dir(mode, target);
        let out_dir = dir.join("lib");
        std::fs::create_dir_all(&out_dir).unwrap();
        let out = out_dir.join("out.bin");
        std::fs::write(&out, content).unwrap();
        self.cache.store(&key(target), &out_dir, &[ActionOutput { path: out.clone(), optional: false }]).unwrap();
        gc::touch_stamp(&dir.join(USED_STAMP));
        backdate(&dir.join(USED_STAMP), age);
        backdate(&self.ac_path(&key(target)), age);
        // The blob is the output's inode. Blobs are judged by age only for
        // the one-hour grace period, so make it old enough to sweep.
        backdate(&self.blob_path(content), age.max(2 * HOUR));
        out
    }

    fn gc(&self, max_age: Duration, dry_run: bool) -> GcReport {
        gc::run(&self.root, GcOptions { max_age, dry_run }, SystemTime::now()).unwrap()
    }
}

#[test]
fn stale_outputs_entries_and_blobs_go_together_and_fresh_ones_stay() {
    let w = ws();
    let old = w.build("mode_a", "old_t_1", "old content", 40 * DAY);
    let fresh = w.build("mode_b", "new_t_2", "fresh content!", HOUR / 2);
    let entry_size = std::fs::metadata(w.ac_path(&key("old_t_1"))).unwrap().len();

    let report = w.gc(30 * DAY, false);

    assert!(!old.exists());
    assert!(!w.target_dir("mode_a", "old_t_1").exists());
    assert!(!w.root.join(".anubis-out/mode_a").exists(), "an emptied mode directory is removed");
    assert!(!w.ac_path(&key("old_t_1")).exists());
    assert!(!w.blob_path("old content").exists());
    assert_eq!((report.target_dirs, report.entries, report.blobs), (1, 1, 1));
    // The output and the blob are one file, counted once; the stamp is empty.
    assert_eq!(report.freed_bytes, "old content".len() as u64 + entry_size);
    assert_eq!(report.stale_targets, vec![("mode_a".to_string(), 1)]);

    assert!(fresh.exists());
    assert!(w.ac_path(&key("new_t_2")).exists());
    assert!(w.blob_path("fresh content!").exists());

    // What was deleted is a miss; what was kept still restores.
    let elsewhere = w.root.join("elsewhere");
    assert!(!w.cache.restore(&key("old_t_1"), &elsewhere).unwrap());
    assert!(w.cache.restore(&key("new_t_2"), &elsewhere).unwrap());
    assert_eq!(std::fs::read_to_string(elsewhere.join("out.bin")).unwrap(), "fresh content!");
}

#[test]
fn a_blob_named_by_a_live_entry_survives_its_stale_output() {
    let w = ws();
    let out = w.build("mode_a", "t_1", "shared bytes", 40 * DAY);
    // The entry was hit recently (by another mode, say); the directory wasn't used.
    backdate(&w.ac_path(&key("t_1")), HOUR / 2);

    let report = w.gc(30 * DAY, false);

    assert!(!out.exists());
    assert!(w.blob_path("shared bytes").exists());
    assert_eq!((report.target_dirs, report.entries, report.blobs), (1, 0, 0));
    assert_eq!(report.freed_bytes, 0, "the blob still holds the bytes");
    assert!(w.cache.restore(&key("t_1"), &w.root.join("elsewhere")).unwrap());
}

#[test]
fn a_blob_no_entry_names_is_swept_only_after_the_grace_period() {
    let w = ws();
    w.build("mode_a", "t_1", "old orphan", HOUR / 2);
    w.build("mode_a", "t_2", "young orphan", HOUR / 2);
    std::fs::remove_file(w.ac_path(&key("t_1"))).unwrap();
    std::fs::remove_file(w.ac_path(&key("t_2"))).unwrap();
    backdate(&w.blob_path("young orphan"), HOUR / 2); // a build may be about to write its entry

    let report = w.gc(30 * DAY, false);

    assert!(!w.blob_path("old orphan").exists());
    assert!(w.blob_path("young orphan").exists());
    assert_eq!(report.blobs, 1);
    assert_eq!(report.freed_bytes, 0, "its output is still there");
}

#[test]
fn something_used_within_the_stamp_refresh_is_never_stale() {
    let w = ws();
    // A stamp is up to STAMP_REFRESH older than the last use.
    let kept = w.build("mode_a", "t_1", "one", DAY + STAMP_REFRESH / 2);
    let gone = w.build("mode_a", "t_2", "two", DAY + STAMP_REFRESH * 2);

    w.gc(DAY, false);

    assert!(kept.exists());
    assert!(!gone.exists());
}

#[test]
fn dry_run_deletes_nothing_and_reports_the_same() {
    let w = ws();
    let old = w.build("mode_a", "t_1", "old content", 40 * DAY);
    w.build("mode_a", "t_2", "fresh", HOUR / 2);

    let dry = w.gc(30 * DAY, true);
    assert!(old.exists());
    assert!(w.ac_path(&key("t_1")).exists());
    assert!(w.blob_path("old content").exists());

    let real = w.gc(30 * DAY, false);
    assert!(!old.exists());
    assert_eq!((dry.target_dirs, dry.entries, dry.blobs, dry.freed_bytes), (real.target_dirs, real.entries, real.blobs, real.freed_bytes));
    assert!(real.freed_bytes > "old content".len() as u64);
}

#[test]
fn a_directory_without_a_stamp_is_stamped_not_deleted() {
    let w = ws();
    // Outputs from before stamps existed, in the old nested layout.
    let legacy = w.root.join(".anubis-out/mode_a/samples/basic/x/main/bin/main");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "exe").unwrap();
    backdate(&legacy, 400 * DAY);
    // Files directly in a mode directory are not gc's.
    let loose = w.root.join(".anubis-out/lib_overlay/abc.yaml");
    std::fs::create_dir_all(loose.parent().unwrap()).unwrap();
    std::fs::write(&loose, "yaml").unwrap();
    backdate(&loose, 400 * DAY);

    let report = w.gc(30 * DAY, false);

    assert!(legacy.exists());
    assert!(loose.exists());
    assert!(!report.removed_anything());
    let stamp = w.root.join(".anubis-out/mode_a/samples").join(USED_STAMP);
    assert!(stamp.exists());

    // Unused from then on, it goes once the age limit has passed.
    backdate(&stamp, 40 * DAY);
    assert_eq!(w.gc(30 * DAY, false).target_dirs, 1);
    assert!(!legacy.exists());
}

#[test]
fn compile_manifests_survive_and_point_at_their_results() {
    let w = ws();
    w.build("mode_a", "t_1", "object", HOUR / 2);
    let manifest_key = key("manifest");
    w.cache.record_manifest(&manifest_key, vec![], &key("t_1")).unwrap();

    let report = w.gc(30 * DAY, false);

    assert!(!report.removed_anything(), "a manifest is not an unreadable entry");
    assert_eq!(w.cache.lookup_manifest(&manifest_key).unwrap(), Some(key("t_1")));
    assert!(w.blob_path("object").exists());
}

#[test]
fn an_unreadable_entry_is_deleted() {
    let w = ws();
    w.build("mode_a", "t_1", "object", HOUR / 2);
    std::fs::write(w.ac_path(&key("t_1")), b"not messagepack").unwrap();

    let report = w.gc(30 * DAY, false);

    assert_eq!((report.entries, report.blobs), (1, 1));
    assert!(!w.ac_path(&key("t_1")).exists());
}

#[test]
fn reading_an_entry_refreshes_an_old_stamp_and_leaves_a_young_one() {
    let w = ws();
    w.build("mode_a", "t_1", "one", 3 * DAY);
    w.build("mode_a", "t_2", "two", HOUR / 2);
    let mtime = |k: &str| std::fs::metadata(w.ac_path(&key(k))).unwrap().modified().unwrap();
    let young_before = mtime("t_2");

    let elsewhere = w.root.join("elsewhere");
    assert!(w.cache.restore(&key("t_1"), &elsewhere.join("1")).unwrap());
    assert!(w.cache.restore(&key("t_2"), &elsewhere.join("2")).unwrap());

    assert!(mtime("t_1").elapsed().unwrap() < HOUR / 2, "a hit makes the entry fresh");
    assert_eq!(mtime("t_2"), young_before, "no write for a stamp younger than the refresh");
}

#[test]
fn the_pass_after_a_build_runs_once_a_day() {
    let w = ws();
    let old = w.build("mode_a", "t_1", "one", 40 * DAY);
    gc::run_if_due(&w.root);
    assert!(!old.exists());

    // Within the day: nothing happens, even to something stale.
    let old2 = w.build("mode_a", "t_2", "two", 40 * DAY);
    gc::run_if_due(&w.root);
    assert!(old2.exists());

    backdate(&w.root.join(".anubis-cache/gc-last"), 25 * HOUR);
    gc::run_if_due(&w.root);
    assert!(!old2.exists());
}
