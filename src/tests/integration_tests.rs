//! Integration tests that perform actual builds using the Anubis build system.
//!
//! These tests require toolchains to be installed (`cargo run -- install-toolchains`)
//! and are marked `#[ignore]` by default. Run them with:
//!
//!   cargo test integration -- --ignored
//!   cargo test -- --ignored

use std::sync::Arc;

use crate::anubis::*;
use crate::logging::LogLevel;
use crate::progress::ProgressDisplay;
use crate::util::SlashFix;

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

    // Hash pre-hash dirs so file-change detection works
    for dir_name in &config.pre_hash_dirs {
        let dir = project_root.join(dir_name).slash_fix();
        if !anubis.fs_tree_hasher.has_dir_cached(&dir) {
            anubis
                .fs_tree_hasher
                .hash_dir(&dir)
                .unwrap_or_else(|e| panic!("Failed to hash dir {}: {}", dir_name, e));
        }
    }

    (Arc::new(anubis), config)
}

/// Helper: expand a target pattern (e.g., "//samples/basic/...") into concrete target paths.
fn expand_pattern(anubis: &Anubis, pattern: &str) -> Vec<String> {
    let tp = TargetPattern::parse(pattern).unwrap_or_else(|| panic!("Invalid target pattern: {}", pattern));
    expand_target_pattern(anubis.root.as_std_path(), &tp, &anubis.rule_typeinfos)
        .unwrap_or_else(|e| panic!("Failed to expand pattern {}: {}", pattern, e))
}


/// Build all basic samples across all build modes (win_dev, win_release, linux_dev, linux_release).
///
/// Equivalent to:
///   cargo run --release -- build \
///     -m //mode:win_dev //mode:win_release //mode:linux_dev //mode:linux_release \
///     -t //samples/basic/...
#[test]
#[ignore]
fn build_all_basic_samples_all_modes() {
    let (anubis, _config) = setup_anubis();

    let modes = vec![
        AnubisTarget::new("//mode:win_dev").unwrap(),
        AnubisTarget::new("//mode:win_release").unwrap(),
        AnubisTarget::new("//mode:linux_dev").unwrap(),
        AnubisTarget::new("//mode:linux_release").unwrap(),
    ];

    // Expand //samples/basic/... to all concrete targets
    let expanded = expand_pattern(&anubis, "//samples/basic/...");

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
    let progress = ProgressDisplay::new(num_workers, false, true, LogLevel::Info);

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
