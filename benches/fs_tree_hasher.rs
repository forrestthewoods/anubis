use anubis::fs_tree_hasher::{FsTreeHasher, HashMode};
use camino::{Utf8Path, Utf8PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the project root by walking up from cwd looking for `.anubis_root`.
fn find_project_root() -> Utf8PathBuf {
    let mut dir = std::env::current_dir().expect("no cwd");
    loop {
        if dir.join(".anubis_root").is_file() {
            return Utf8PathBuf::try_from(dir).expect("non-UTF-8 project root");
        }
        if !dir.pop() {
            panic!("Could not find .anubis_root in any parent directory");
        }
    }
}

/// Walk a directory and print stats: total files, dirs, size, max depth.
fn print_dir_stats(path: &Utf8Path) {
    let mut files: u64 = 0;
    let mut dirs: u64 = 0;
    let mut total_size: u64 = 0;
    let mut max_depth: usize = 0;

    for entry in jwalk::WalkDir::new(path.as_std_path())
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            files += 1;
            if let Ok(meta) = std::fs::metadata(entry.path()) {
                total_size += meta.len();
            }
        } else if entry.file_type().is_dir() && entry.depth() > 0 {
            dirs += 1;
        }
        if entry.depth() > max_depth {
            max_depth = entry.depth();
        }
    }

    let size_str = if total_size >= 1_000_000_000 {
        format!("{:.2} GB", total_size as f64 / 1_000_000_000.0)
    } else if total_size >= 1_000_000 {
        format!("{:.2} MB", total_size as f64 / 1_000_000.0)
    } else if total_size >= 1_000 {
        format!("{:.2} KB", total_size as f64 / 1_000.0)
    } else {
        format!("{total_size} B")
    };

    println!("  Files: {files} | Dirs: {dirs} | Total size: {size_str} | Max depth: {max_depth}");
}

/// Run a closure for N iterations and print timing stats.
fn bench_run(name: &str, iterations: usize, mut f: impl FnMut() -> Duration) {
    println!("\n--- {name} ---");

    let mut timings = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let elapsed = f();
        println!("  Run {}: {:.3}s", i + 1, elapsed.as_secs_f64());
        timings.push(elapsed);
    }

    if timings.len() > 1 {
        timings.sort();
        let min = timings.first().unwrap();
        let max = timings.last().unwrap();
        let avg = timings.iter().sum::<Duration>() / timings.len() as u32;
        let median = &timings[timings.len() / 2];
        println!(
            "  Min: {:.3}s | Avg: {:.3}s | Median: {:.3}s | Max: {:.3}s",
            min.as_secs_f64(),
            avg.as_secs_f64(),
            median.as_secs_f64(),
            max.as_secs_f64(),
        );
    }
}

// ---------------------------------------------------------------------------
// Benchmark: hash_dir with cold cache (new hasher each time)
// ---------------------------------------------------------------------------

fn bench_hash_dir_cold(dir: &Utf8Path, mode: HashMode, iterations: usize) {
    let mode_name = match mode {
        HashMode::Full => "Full",
        HashMode::Fast => "Fast",
    };
    let dir_name = dir.file_name().unwrap_or_else(|| dir.as_str());

    bench_run(
        &format!("hash_dir {dir_name}/ [{mode_name} mode, cold cache]"),
        iterations,
        || {
            let hasher = FsTreeHasher::new(mode).expect("failed to create hasher");
            let start = Instant::now();
            let _hash = hasher.hash_dir(dir).expect("hash_dir failed");
            start.elapsed()
        },
    );
}

// ---------------------------------------------------------------------------
// Benchmark: hash_dir cache hit
// ---------------------------------------------------------------------------

fn bench_hash_dir_cache_hit(dir: &Utf8Path, mode: HashMode) {
    let mode_name = match mode {
        HashMode::Full => "Full",
        HashMode::Fast => "Fast",
    };
    let dir_name = dir.file_name().unwrap_or_else(|| dir.as_str());

    println!("\n--- hash_dir {dir_name}/ [{mode_name} mode, cache hit] ---");

    let hasher = FsTreeHasher::new(mode).expect("failed to create hasher");

    // Cold run
    let start = Instant::now();
    let hash1 = hasher.hash_dir(dir).expect("hash_dir failed (cold)");
    let cold = start.elapsed();
    println!("  Cold:   {:.3}s", cold.as_secs_f64());

    // Cached run
    let start = Instant::now();
    let hash2 = hasher.hash_dir(dir).expect("hash_dir failed (cached)");
    let cached = start.elapsed();

    assert_eq!(hash1, hash2, "cached hash should match cold hash");

    let speedup = if cached.as_nanos() > 0 {
        cold.as_nanos() as f64 / cached.as_nanos() as f64
    } else {
        f64::INFINITY
    };
    println!(
        "  Cached: {:.6}s  ({:.0}x speedup)",
        cached.as_secs_f64(),
        speedup
    );
}

// ---------------------------------------------------------------------------
// Main — runs the full benchmark suite
// ---------------------------------------------------------------------------

fn main() {
    let root = find_project_root();

    println!("\n=== FsTreeHasher Benchmark ===");

    let dirs: Vec<(&str, Utf8PathBuf)> = ["toolchains", "samples"]
        .iter()
        .filter_map(|name| {
            let p = root.join(name);
            if p.is_dir() {
                Some((*name, p))
            } else {
                println!("\nSKIP: {name}/ not found");
                None
            }
        })
        .collect();

    // Print stats for all directories
    for (name, path) in &dirs {
        println!("\n--- Directory Stats: {name}/ ---");
        print_dir_stats(path);
    }

    // Run benchmarks for each directory
    for (_, path) in &dirs {
        bench_hash_dir_cold(path, HashMode::Full, 3);
        bench_hash_dir_cold(path, HashMode::Fast, 3);
        bench_hash_dir_cache_hit(path, HashMode::Full);
    }

    println!("\n=== Benchmark Complete ===");
}
