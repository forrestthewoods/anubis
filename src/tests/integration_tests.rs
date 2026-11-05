//! Integration tests that perform actual builds using the Anubis build system.
//!
//! These tests build only the modes whose toolchains are installed
//! (`cargo run -- install-toolchains`) and skip when none are.

use std::sync::Arc;

use crate::anubis::*;
use crate::progress::{DisplayMode, ProgressDisplay};
use crate::util::SlashFix;

/// Integration tests share .anubis-out; run their builds one at a time.
static BUILD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Helper: find the project root, load config, and create an Anubis instance.
fn setup_anubis() -> (Arc<Anubis>, AnubisConfig) {
    let cwd = std::env::current_dir().expect("Failed to get current directory");
    let anubis_root_file = find_anubis_root(&cwd).expect("Failed to find .anubis_root");
    let project_root = anubis_root_file
        .parent()
        .expect("Failed to get parent of .anubis_root")
        .to_owned();

    let config = load_anubis_config(&anubis_root_file).expect("Failed to load .anubis_root config");
    let anubis = Anubis::new(project_root.clone(), false, &config).expect("Failed to create Anubis instance");


    (Arc::new(anubis), config)
}

/// Build all basic samples across all build modes (win_dev, win_release, linux_dev, linux_release).
///
/// Equivalent to:
///   cargo run --release -- build \
///     -m //mode:win_dev //mode:win_release //mode:linux_dev //mode:linux_release \
///     -t //samples/basic/...
#[test]
fn build_all_basic_samples_all_modes() {
    let _lock = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (anubis, _config) = setup_anubis();

    // Only build modes whose toolchains are installed, so `cargo test` passes
    // on a fresh clone. Windows targets need the MSVC CRT and Windows SDK;
    // every target needs LLVM, and Linux targets need Zig's libc.
    let toolchains = anubis.root.join("toolchains");
    let have = |dir: &str| toolchains.join(dir).exists();
    let mut modes = Vec::new();
    if have("llvm") && have("msvc") && have("windows_kits") {
        modes.push(AnubisTarget::new("//mode:win_dev").unwrap());
        modes.push(AnubisTarget::new("//mode:win_release").unwrap());
    }
    if have("llvm") && have("zig") {
        modes.push(AnubisTarget::new("//mode:linux_dev").unwrap());
        modes.push(AnubisTarget::new("//mode:linux_release").unwrap());
    }
    if modes.is_empty() {
        eprintln!("skipping: no toolchains installed (run `anubis install-toolchains`)");
        return;
    }

    // Expand //samples/basic/... to all concrete targets
    let expanded = anubis.expand_targets(&["//samples/basic/...".to_string()]).unwrap();

    assert!(
        !expanded.is_empty(),
        "Expected at least one target under //samples/basic/..."
    );

    let targets: Vec<AnubisTarget> = expanded
        .iter()
        .map(|t| AnubisTarget::new(t).unwrap())
        .collect();
    let toolchain = AnubisTarget::new("//toolchains:default").unwrap();

    let num_workers = num_cpus::get_physical();
    let progress = ProgressDisplay::new(DisplayMode::Quiet, num_workers, false);

    let artifacts = build_targets(
        anubis,
        &modes,
        &toolchain,
        &targets,
        num_workers,
        progress.sender(),
    )
    .expect("Build failed for all basic samples across all modes");

    let expected_count = modes.len() * targets.len();
    assert_eq!(
        artifacts.len(),
        expected_count,
        "Expected {} artifacts ({} modes x {} targets), got {}",
        expected_count,
        modes.len(),
        targets.len(),
        artifacts.len()
    );
}

/// Build all samples across all build modes (win_dev, win_release, linux_dev, linux_release).
///
/// Equivalent to:
///   cargo run --release -- build \
///     -m //mode:win_dev //mode:win_release //mode:linux_dev //mode:linux_release \
///     -t //samples/...
#[test]
#[ignore]
fn build_all_samples_all_modes() {
    let (anubis, _config) = setup_anubis();

    // Only build modes whose toolchains are installed, so `cargo test` passes
    // on a fresh clone. Windows targets need the MSVC CRT and Windows SDK;
    // every target needs LLVM, and Linux targets need Zig's libc.
    let toolchains = anubis.root.join("toolchains");
    let have = |dir: &str| toolchains.join(dir).exists();
    let mut modes = Vec::new();
    if have("llvm") && have("msvc") && have("windows_kits") {
        modes.push(AnubisTarget::new("//mode:win_dev").unwrap());
        modes.push(AnubisTarget::new("//mode:win_release").unwrap());
    }
    if have("llvm") && have("zig") {
        modes.push(AnubisTarget::new("//mode:linux_dev").unwrap());
        modes.push(AnubisTarget::new("//mode:linux_release").unwrap());
    }
    if modes.is_empty() {
        eprintln!("skipping: no toolchains installed (run `anubis install-toolchains`)");
        return;
    }

    // Expand //samples/... to all concrete targets
    let expanded = anubis.expand_targets(&["//samples/...".to_string()]).unwrap();

    assert!(
        !expanded.is_empty(),
        "Expected at least one target under //samples/..."
    );

    let targets: Vec<AnubisTarget> = expanded
        .iter()
        .map(|t| AnubisTarget::new(t).unwrap())
        .collect();
    let toolchain = AnubisTarget::new("//toolchains:default").unwrap();

    let num_workers = num_cpus::get_physical();
    let progress = ProgressDisplay::new(DisplayMode::Quiet, num_workers, false);

    let artifacts = build_targets(
        anubis,
        &modes,
        &toolchain,
        &targets,
        num_workers,
        progress.sender(),
    )
    .expect("Build failed for all basic samples across all modes");

    let expected_count = modes.len() * targets.len();
    assert_eq!(
        artifacts.len(),
        expected_count,
        "Expected {} artifacts ({} modes x {} targets), got {}",
        expected_count,
        modes.len(),
        targets.len(),
        artifacts.len()
    );
}

/// A second build with a warm cache runs no actions, even after the output
/// directory is deleted.
#[test]
fn build_cache_second_build_is_all_hits() {
    let _lock = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (probe, config) = setup_anubis();
    let root = probe.root.clone();
    let toolchains = root.join("toolchains");
    let have = |dir: &str| toolchains.join(dir).exists();
    let mode = if have("llvm") && have("zig") && cfg!(target_os = "linux") {
        "//mode:linux_dev"
    } else if have("llvm") && have("msvc") && have("windows_kits") {
        "//mode:win_dev"
    } else {
        eprintln!("skipping: no toolchains installed (run `anubis install-toolchains`)");
        return;
    };

    let cache_dir = tempfile::TempDir::new().unwrap();
    let cache_root = camino::Utf8PathBuf::from_path_buf(cache_dir.path().to_path_buf()).unwrap();

    let build = || -> (u64, u64) {
        let mut anubis = Anubis::new(root.clone(), false, &config).unwrap();
        let backend = crate::build_cache::LocalDiskBackend::new(cache_root.clone()).unwrap();
        anubis.build_cache = Some(crate::build_cache::BuildCache::new(&root, Box::new(backend)));
        let anubis = Arc::new(anubis);
        let targets = vec![
            AnubisTarget::new("//samples/basic/nested_staticlib_cpp:main").unwrap(),
            AnubisTarget::new("//samples/basic/trivial_c:main").unwrap(),
        ];
        let toolchain = AnubisTarget::new("//toolchains:default").unwrap();
        let modes = vec![AnubisTarget::new(mode).unwrap()];
        let progress = ProgressDisplay::new(DisplayMode::Quiet, 4, false);
        build_targets(anubis.clone(), &modes, &toolchain, &targets, 4, progress.sender())
            .expect("build failed");
        let cache = anubis.build_cache.as_ref().unwrap();
        (cache.hits(), cache.misses())
    };

    let (_, first_misses) = build();
    assert!(first_misses > 0, "a cold cache should miss");

    // Delete this mode's outputs for the two samples; everything must come
    // back from the cache.
    let mode_dir = probe.out_dir(&Some(probe.get_mode(&AnubisTarget::new(mode).unwrap()).unwrap()), &AnubisTarget::new("//samples/basic/trivial_c:main").unwrap(), "bin");
    let _ = std::fs::remove_dir_all(&mode_dir);

    let (hits, misses) = build();
    assert_eq!(misses, 0, "warm cache should not miss");
    assert!(hits >= first_misses, "expected every action to hit");
    assert!(mode_dir.join(if mode.contains("win") { "main.exe" } else { "main" }).exists());
}
