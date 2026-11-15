//! Synthetic benchmark for the proposed `anubis gc` (#235).
//!
//! Builds a cache + output tree shaped like Anubis's (`ac/<xx>/<hex>`,
//! read-only `cas/<xx>/<hex>` blobs hard-linked into per-mode,
//! per-target output dirs), then times each operation gc would do.
//! Usage: gc_bench <dir> [ac_entries] [blobs]
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

fn hex(n: u64, salt: u64) -> String {
    // Cheap deterministic 64-hex-char name.
    let mut x = n.wrapping_mul(0x9E3779B97F4A7C15) ^ salt;
    let mut s = String::with_capacity(64);
    for _ in 0..4 {
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        s.push_str(&format!("{x:016x}"));
    }
    s
}

fn shard(root: &Path, kind: &str, h: &str) -> PathBuf {
    root.join(kind).join(&h[..2]).join(h)
}

/// Run `f` over `items` on `threads` threads.
fn par<T: Sync>(items: &[T], threads: usize, f: impl Fn(&T) + Sync) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= items.len() {
                    break;
                }
                f(&items[i]);
            });
        }
    });
}

struct Report {
    rows: Vec<(String, usize, Duration)>,
}
impl Report {
    fn time(&mut self, name: &str, count: usize, f: impl FnOnce()) {
        let t = Instant::now();
        f();
        let d = t.elapsed();
        eprintln!("{name}: {count} in {:.3}s", d.as_secs_f64());
        self.rows.push((name.to_string(), count, d));
    }
}

fn set_read_only(p: &Path) {
    let mut perms = fs::metadata(p).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(p, perms).unwrap();
}

#[cfg(unix)]
fn link_count(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(p).unwrap().nlink()
}

#[cfg(windows)]
fn link_count(p: &Path) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let f = fs::File::open(p).unwrap();
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) };
    assert!(ok != 0);
    info.nNumberOfLinks as u64
}

fn walk_files(dir: &Path, out: &mut Vec<(PathBuf, u64, SystemTime)>) {
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let m = e.metadata().unwrap();
        if m.is_dir() {
            walk_files(&e.path(), out);
        } else {
            out.push((e.path(), m.len(), m.modified().unwrap()));
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).expect("usage: gc_bench <dir> [ac] [blobs]"));
    let n_ac: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(50_000);
    let n_blob: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(40_000);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let _ = fs::remove_dir_all(&root);
    let cache = root.join("cache");
    let out = root.join("out");
    let mut r = Report { rows: vec![] };

    // ---- setup -----------------------------------------------------------
    let ac: Vec<PathBuf> = (0..n_ac as u64).map(|i| shard(&cache, "ac", &hex(i, 1))).collect();
    let blobs: Vec<PathBuf> = (0..n_blob as u64).map(|i| shard(&cache, "cas", &hex(i, 2))).collect();
    for kind in ["ac", "cas"] {
        for b in 0..256 {
            fs::create_dir_all(cache.join(kind).join(format!("{b:02x}"))).unwrap();
        }
    }
    // Mode A links blobs [0, 40%), mode B links [40%, 80%); the last 20% are
    // linked by no output (what a sweep would delete). 200 targets per mode.
    let mode_of = |i: usize| if i < n_blob * 2 / 5 { Some("mode_a") } else if i < n_blob * 4 / 5 { Some("mode_b") } else { None };
    let outputs: Vec<(usize, PathBuf)> = (0..n_blob)
        .filter_map(|i| mode_of(i).map(|m| (i, out.join(m).join(format!("pkg{}", i % 20)).join(format!("t{}", i % 200)).join(format!("o{i}.obj")))))
        .collect();
    let mut target_dirs: Vec<PathBuf> = outputs.iter().map(|(_, p)| p.parent().unwrap().to_path_buf()).collect();
    target_dirs.sort();
    target_dirs.dedup();
    r.time("setup: write ac entries (300 B)", n_ac, || {
        par(&ac, threads, |p| fs::write(p, [7u8; 300]).unwrap())
    });
    let total = AtomicUsize::new(0);
    r.time("setup: write blobs (4-64 KiB, read-only)", n_blob, || {
        let idx: Vec<usize> = (0..n_blob).collect();
        par(&idx, threads, |&i| {
            let size = 4096 + (i * 7919) % (60 * 1024);
            total.fetch_add(size, Ordering::Relaxed);
            let mut f = fs::File::create(&blobs[i]).unwrap();
            f.write_all(&vec![(i % 251) as u8; size]).unwrap();
            drop(f);
            set_read_only(&blobs[i]);
        })
    });
    eprintln!("blob bytes: {:.2} GiB", total.load(Ordering::Relaxed) as f64 / (1u64 << 30) as f64);
    for d in &target_dirs {
        fs::create_dir_all(d).unwrap();
    }
    r.time("setup: hard-link outputs", outputs.len(), || {
        par(&outputs, threads, |(i, p)| fs::hard_link(&blobs[*i], p).unwrap())
    });

    // ---- per-build costs ---------------------------------------------------
    // A cache hit today: read the ac entry. Proposed: also look at its mtime.
    let read_ac = |p: &PathBuf| {
        let mut f = fs::File::open(p).unwrap();
        let mut v = Vec::new();
        f.read_to_end(&mut v).unwrap();
        std::hint::black_box(v);
    };
    r.time("hit: read ac entry (today), 1 thread", n_ac, || ac.iter().for_each(read_ac));
    r.time(&format!("hit: read ac entry (today), {threads} threads"), n_ac, || par(&ac, threads, read_ac));
    let day = Duration::from_secs(86_400);
    let fresh = AtomicUsize::new(0);
    r.time(&format!("hit: read + mtime check, {threads} threads"), n_ac, || {
        par(&ac, threads, |p| {
            let mut f = fs::File::open(p).unwrap();
            let mut v = Vec::new();
            f.read_to_end(&mut v).unwrap();
            let age = f.metadata().unwrap().modified().unwrap().elapsed().unwrap_or_default();
            if age < day {
                fresh.fetch_add(1, Ordering::Relaxed);
            }
        })
    });
    // First build of the day: every hit restamps (worst case).
    let stamp = |p: &PathBuf| {
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now()).unwrap();
    };
    r.time("stamp: set mtime on every ac entry, 1 thread", n_ac, || ac.iter().for_each(stamp));
    r.time(&format!("stamp: set mtime on every ac entry, {threads} threads"), n_ac, || par(&ac, threads, stamp));
    let target_stamps: Vec<PathBuf> = target_dirs.iter().map(|d| d.join(".anubis-used")).collect();
    r.time("stamp: write .anubis-used per target dir, 1 thread", target_stamps.len(), || {
        target_stamps.iter().for_each(|p| fs::write(p, b"").unwrap())
    });

    // ---- gc pass -----------------------------------------------------------
    let mut listed = Vec::new();
    r.time("gc: list ac/ with mtime+size, 1 thread", n_ac, || walk_files(&cache.join("ac"), &mut listed));
    let mut listed_cas = Vec::new();
    r.time("gc: list cas/ with mtime+size, 1 thread", n_blob, || walk_files(&cache.join("cas"), &mut listed_cas));
    let shards: Vec<PathBuf> = (0..256).map(|b| cache.join("cas").join(format!("{b:02x}"))).collect();
    r.time(&format!("gc: list cas/ with mtime+size, {threads} threads"), n_blob, || {
        par(&shards, threads, |d| {
            let mut v = Vec::new();
            walk_files(d, &mut v);
            std::hint::black_box(v);
        })
    });
    let mut listed_out = Vec::new();
    r.time("gc: list out/ with mtime+size, 1 thread", outputs.len() + target_stamps.len(), || walk_files(&out, &mut listed_out));
    // Mark: read every ac entry (decode skipped; it's tiny next to the open).
    r.time("gc: read every ac entry (mark), 1 thread", n_ac, || ac.iter().for_each(read_ac));
    r.time(&format!("gc: read every ac entry (mark), {threads} threads"), n_ac, || par(&ac, threads, read_ac));
    let nl = AtomicUsize::new(0);
    r.time(&format!("report: link count of every blob, {threads} threads"), n_blob, || {
        par(&blobs, threads, |p| {
            nl.fetch_add(link_count(p) as usize, Ordering::Relaxed);
        })
    });
    eprintln!("total links: {}", nl.load(Ordering::Relaxed));
    // Sweep: delete 25% of ac entries and the 20% of blobs no output links.
    let dead_ac: Vec<PathBuf> = ac.iter().step_by(4).cloned().collect();
    r.time(&format!("gc: delete 25% of ac entries, {threads} threads"), dead_ac.len(), || {
        par(&dead_ac, threads, |p| fs::remove_file(p).unwrap())
    });
    let dead_blobs: Vec<PathBuf> = blobs[n_blob * 4 / 5..].to_vec();
    r.time(&format!("gc: delete unlinked read-only blobs, {threads} threads"), dead_blobs.len(), || {
        par(&dead_blobs, threads, |p| fs::remove_file(p).unwrap())
    });
    // Expire one mode's outputs: std remove_dir_all vs per-target in parallel.
    let mode_a_files = outputs.iter().filter(|(_, p)| p.starts_with(out.join("mode_a"))).count();
    r.time("gc: remove_dir_all one mode's outputs (read-only hard links), 1 thread", mode_a_files, || {
        fs::remove_dir_all(out.join("mode_a")).unwrap()
    });
    let mode_b_targets: Vec<PathBuf> = target_dirs.iter().filter(|d| d.starts_with(out.join("mode_b"))).cloned().collect();
    let mode_b_files = outputs.len() - mode_a_files;
    r.time(&format!("gc: remove one mode's outputs per target dir, {threads} threads"), mode_b_files, || {
        par(&mode_b_targets, threads, |d| fs::remove_dir_all(d).unwrap())
    });
    // Blobs mode A/B linked are now unlinked: delete them too.
    let rest: Vec<PathBuf> = blobs[..n_blob * 4 / 5].to_vec();
    r.time(&format!("gc: delete now-unlinked blobs, {threads} threads"), rest.len(), || {
        par(&rest, threads, |p| fs::remove_file(p).unwrap())
    });

    // ---- report --------------------------------------------------------------
    println!("| Operation | Files | Seconds | µs per file |");
    println!("|---|---:|---:|---:|");
    for (name, n, d) in &r.rows {
        println!("| {name} | {n} | {:.3} | {:.1} |", d.as_secs_f64(), d.as_secs_f64() * 1e6 / (*n).max(1) as f64);
    }
    let _ = fs::remove_dir_all(&root);
}
