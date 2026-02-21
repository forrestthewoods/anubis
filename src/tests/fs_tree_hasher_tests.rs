use crate::fs_tree_hasher::{ContentHash, FsTreeHasher, HashMode};
use camino::Utf8Path;
use std::collections::HashSet;
use std::fs;
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn tmp_utf8(dir: &tempfile::TempDir) -> &Utf8Path {
    Utf8Path::from_path(dir.path()).unwrap()
}

/// Wait for filesystem watcher events to propagate.
fn settle() {
    thread::sleep(Duration::from_millis(250));
}

/// Write a file then sleep briefly so mtime definitely advances (NTFS quantum
/// is 100ns, but rounding can make two rapid writes share a timestamp).
fn write_and_settle(path: &Utf8Path, content: &[u8]) {
    fs::write(path.as_std_path(), content).unwrap();
    settle();
}

// ===========================================================================
// ContentHash type tests
// ===========================================================================

#[test]
fn content_hash_display_format() {
    let h = ContentHash(0);
    assert_eq!(format!("{h}"), "00000000000000000000000000000000");
    assert_eq!(format!("{h}").len(), 32);

    let h = ContentHash(0xDEAD_BEEF);
    assert_eq!(format!("{h}"), "000000000000000000000000deadbeef");

    let h = ContentHash(u128::MAX);
    assert_eq!(format!("{h}"), "ffffffffffffffffffffffffffffffff");
}

#[test]
fn content_hash_as_u128() {
    let h = ContentHash(42);
    assert_eq!(h.as_u128(), 42);

    let h = ContentHash(u128::MAX);
    assert_eq!(h.as_u128(), u128::MAX);
}

#[test]
fn content_hash_equality() {
    assert_eq!(ContentHash(100), ContentHash(100));
    assert_ne!(ContentHash(100), ContentHash(101));
}

#[test]
fn content_hash_clone_copy() {
    let a = ContentHash(999);
    let b = a; // Copy
    let c = a.clone(); // Clone
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn content_hash_usable_as_hashmap_key() {
    let mut set = HashSet::new();
    set.insert(ContentHash(1));
    set.insert(ContentHash(2));
    set.insert(ContentHash(1)); // duplicate
    assert_eq!(set.len(), 2);
}

#[test]
fn content_hash_debug_format() {
    let h = ContentHash(0xABCD);
    let debug = format!("{h:?}");
    assert!(debug.contains("ContentHash"));
    assert!(debug.contains("43981")); // 0xABCD in decimal
}

// ===========================================================================
// Construction & mode
// ===========================================================================

#[test]
fn hasher_reports_mode() {
    let fast = FsTreeHasher::new(HashMode::Fast).unwrap();
    assert_eq!(fast.mode(), HashMode::Fast);

    let full = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_eq!(full.mode(), HashMode::Full);
}

#[test]
fn hasher_starts_with_empty_cache() {
    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_eq!(hasher.cached_count(), (0, 0));
}

// ===========================================================================
// File hashing — Full mode
// ===========================================================================

#[test]
fn full_file_hash_deterministic() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("test.txt");
    fs::write(file.as_std_path(), b"hello world").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_file(&file).unwrap();
    let h2 = hasher.hash_file(&file).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn full_different_content_different_hash() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let f1 = root.join("a.txt");
    let f2 = root.join("b.txt");
    fs::write(f1.as_std_path(), b"alpha").unwrap();
    fs::write(f2.as_std_path(), b"beta").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_ne!(hasher.hash_file(&f1).unwrap(), hasher.hash_file(&f2).unwrap());
}

#[test]
fn full_same_content_same_hash() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let f1 = root.join("a.txt");
    let f2 = root.join("b.txt");
    fs::write(f1.as_std_path(), b"identical content").unwrap();
    fs::write(f2.as_std_path(), b"identical content").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_eq!(hasher.hash_file(&f1).unwrap(), hasher.hash_file(&f2).unwrap());
}

#[test]
fn full_empty_file() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("empty.txt");
    fs::write(file.as_std_path(), b"").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h = hasher.hash_file(&file).unwrap();
    // Empty file should produce a valid, deterministic hash
    assert_eq!(h, hasher.hash_file(&file).unwrap());
}

#[test]
fn full_empty_file_differs_from_nonempty() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let empty = root.join("empty.txt");
    let nonempty = root.join("nonempty.txt");
    fs::write(empty.as_std_path(), b"").unwrap();
    fs::write(nonempty.as_std_path(), b" ").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_ne!(
        hasher.hash_file(&empty).unwrap(),
        hasher.hash_file(&nonempty).unwrap()
    );
}

#[test]
fn full_binary_content() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("binary.bin");
    let content: Vec<u8> = (0..=255).collect();
    fs::write(file.as_std_path(), &content).unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_file(&file).unwrap();
    let h2 = hasher.hash_file(&file).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn full_large_file() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("large.bin");
    // 1 MB of repeating pattern
    let content: Vec<u8> = (0..1_000_000).map(|i| (i % 251) as u8).collect();
    fs::write(file.as_std_path(), &content).unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_file(&file).unwrap();
    let h2 = hasher.hash_file(&file).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn full_single_byte_difference() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let f1 = root.join("a.bin");
    let f2 = root.join("b.bin");
    fs::write(f1.as_std_path(), &[0u8; 1024]).unwrap();
    let mut modified = [0u8; 1024];
    modified[512] = 1;
    fs::write(f2.as_std_path(), &modified).unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_ne!(
        hasher.hash_file(&f1).unwrap(),
        hasher.hash_file(&f2).unwrap()
    );
}

#[test]
fn full_content_change_detected_via_watcher() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("test.txt");
    fs::write(file.as_std_path(), b"version1").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_file(&file).unwrap();
    settle();

    fs::write(file.as_std_path(), b"version2").unwrap();
    settle();

    let h2 = hasher.hash_file(&file).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn full_two_independent_hashers_agree() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("test.txt");
    fs::write(file.as_std_path(), b"shared content").unwrap();

    let h1 = FsTreeHasher::new(HashMode::Full).unwrap();
    let h2 = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_eq!(h1.hash_file(&file).unwrap(), h2.hash_file(&file).unwrap());
}

// ===========================================================================
// File hashing — Fast mode
// ===========================================================================

#[test]
fn fast_file_hash_deterministic() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("test.txt");
    fs::write(file.as_std_path(), b"hello").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Fast).unwrap();
    let h1 = hasher.hash_file(&file).unwrap();
    let h2 = hasher.hash_file(&file).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn fast_different_size_different_hash() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let f1 = root.join("short.txt");
    let f2 = root.join("long.txt");
    fs::write(f1.as_std_path(), b"abc").unwrap();
    fs::write(f2.as_std_path(), b"abcdef").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Fast).unwrap();
    assert_ne!(
        hasher.hash_file(&f1).unwrap(),
        hasher.hash_file(&f2).unwrap()
    );
}

#[test]
fn fast_change_detected_via_watcher() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("test.txt");
    fs::write(file.as_std_path(), b"original").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Fast).unwrap();
    let h1 = hasher.hash_file(&file).unwrap();
    settle();

    // Different size → different mtime+size hash
    fs::write(file.as_std_path(), b"modified content that is longer").unwrap();
    settle();

    let h2 = hasher.hash_file(&file).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn fast_empty_file() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("empty.txt");
    fs::write(file.as_std_path(), b"").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Fast).unwrap();
    let h = hasher.hash_file(&file).unwrap();
    assert_eq!(h, hasher.hash_file(&file).unwrap());
}

// ===========================================================================
// Cross-mode: Full vs Fast produce distinct hashes
// ===========================================================================

#[test]
fn full_and_fast_produce_different_hashes() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("test.txt");
    fs::write(file.as_std_path(), b"content").unwrap();

    let full = FsTreeHasher::new(HashMode::Full).unwrap();
    let fast = FsTreeHasher::new(HashMode::Fast).unwrap();

    // They use fundamentally different inputs (content vs mtime+size),
    // so they will almost certainly differ.
    let hf = full.hash_file(&file).unwrap();
    let hs = fast.hash_file(&file).unwrap();
    // We can't strictly guarantee they differ for all inputs, but for any
    // realistic content they will. If this ever flakes, the hash algorithm
    // has a remarkable coincidence.
    assert_ne!(hf, hs);
}

// ===========================================================================
// Error handling
// ===========================================================================

#[test]
fn hash_nonexistent_file_returns_error() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("does_not_exist.txt");

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert!(hasher.hash_file(&file).is_err());

    let hasher = FsTreeHasher::new(HashMode::Fast).unwrap();
    assert!(hasher.hash_file(&file).is_err());
}

#[test]
fn hash_nonexistent_dir_returns_error() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let missing = root.join("no_such_dir");

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert!(hasher.hash_dir(&missing).is_err());
}

// ===========================================================================
// Directory hashing — Full mode
// ===========================================================================

#[test]
fn dir_hash_empty_directory() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn dir_hash_single_file() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("only.txt").as_std_path(), b"content").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn dir_hash_multiple_files() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("a.txt").as_std_path(), b"aaa").unwrap();
    fs::write(root.join("b.txt").as_std_path(), b"bbb").unwrap();
    fs::write(root.join("c.txt").as_std_path(), b"ccc").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn dir_hash_differs_from_empty() {
    let empty_dir = tmp_dir();
    let file_dir = tmp_dir();

    let empty_root = tmp_utf8(&empty_dir);
    let file_root = tmp_utf8(&file_dir);
    fs::write(file_root.join("a.txt").as_std_path(), b"content").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_ne!(
        hasher.hash_dir(empty_root).unwrap(),
        hasher.hash_dir(file_root).unwrap()
    );
}

#[test]
fn dir_hash_add_file_changes_hash() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("a.txt").as_std_path(), b"a").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    settle();

    fs::write(root.join("b.txt").as_std_path(), b"b").unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn dir_hash_delete_file_changes_hash() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("victim.txt");
    fs::write(file.as_std_path(), b"doomed").unwrap();
    fs::write(root.join("survivor.txt").as_std_path(), b"safe").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    settle();

    fs::remove_file(file.as_std_path()).unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn dir_hash_modify_file_changes_hash() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("data.txt");
    fs::write(file.as_std_path(), b"original").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    settle();

    fs::write(file.as_std_path(), b"modified").unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn dir_hash_rename_file_changes_hash() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let old = root.join("old_name.txt");
    fs::write(old.as_std_path(), b"content").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    settle();

    let new = root.join("new_name.txt");
    fs::rename(old.as_std_path(), new.as_std_path()).unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2, "Renaming a file should change the dir hash (path is part of the hash)");
}

#[test]
fn dir_hash_nested_subdirectories() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let sub = root.join("sub");
    fs::create_dir(sub.as_std_path()).unwrap();
    fs::write(root.join("top.txt").as_std_path(), b"top").unwrap();
    fs::write(sub.join("nested.txt").as_std_path(), b"nested").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn dir_hash_deeply_nested() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);

    // Create a/b/c/d/e/file.txt
    let mut path = root.to_owned();
    for segment in &["a", "b", "c", "d", "e"] {
        path = path.join(segment);
        fs::create_dir(path.as_std_path()).unwrap();
    }
    fs::write(path.join("deep.txt").as_std_path(), b"deep content").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn dir_hash_modify_deeply_nested_file() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let sub = root.join("a").join("b");
    fs::create_dir_all(sub.as_std_path()).unwrap();
    let file = sub.join("deep.txt");
    fs::write(file.as_std_path(), b"v1").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    settle();

    fs::write(file.as_std_path(), b"v2").unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn dir_hash_add_empty_subdir_no_change() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("file.txt").as_std_path(), b"data").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    settle();

    // Adding an empty directory shouldn't change the hash — only files matter
    fs::create_dir(root.join("empty_sub").as_std_path()).unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2, "Adding an empty subdirectory should not change the hash");
}

#[test]
fn dir_hash_many_files() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);

    for i in 0..100 {
        let name = format!("file_{i:03}.txt");
        fs::write(root.join(&name).as_std_path(), format!("content {i}").as_bytes()).unwrap();
    }

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn dir_hash_file_order_independent() {
    // Two directories with the same files (same names + content) should
    // produce the same hash, regardless of creation order.
    let dir1 = tmp_dir();
    let dir2 = tmp_dir();
    let r1 = tmp_utf8(&dir1);
    let r2 = tmp_utf8(&dir2);

    // Create in forward order in dir1
    for name in &["alpha.txt", "beta.txt", "gamma.txt"] {
        fs::write(r1.join(name).as_std_path(), name.as_bytes()).unwrap();
    }
    // Create in reverse order in dir2
    for name in &["gamma.txt", "beta.txt", "alpha.txt"] {
        fs::write(r2.join(name).as_std_path(), name.as_bytes()).unwrap();
    }

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_eq!(
        hasher.hash_dir(r1).unwrap(),
        hasher.hash_dir(r2).unwrap(),
        "Same files should produce same dir hash regardless of creation order"
    );
}

#[test]
fn dir_hash_different_filenames_same_content() {
    // Two dirs with same content but different filenames → different hash
    let dir1 = tmp_dir();
    let dir2 = tmp_dir();
    let r1 = tmp_utf8(&dir1);
    let r2 = tmp_utf8(&dir2);

    fs::write(r1.join("foo.txt").as_std_path(), b"same").unwrap();
    fs::write(r2.join("bar.txt").as_std_path(), b"same").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_ne!(
        hasher.hash_dir(r1).unwrap(),
        hasher.hash_dir(r2).unwrap(),
        "Different filenames should produce different dir hashes"
    );
}

// ===========================================================================
// Directory hashing — Fast mode
// ===========================================================================

#[test]
fn dir_hash_fast_deterministic() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("a.txt").as_std_path(), b"a").unwrap();
    fs::write(root.join("b.txt").as_std_path(), b"b").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Fast).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn dir_hash_fast_add_file_changes_hash() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("a.txt").as_std_path(), b"a").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Fast).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    settle();

    fs::write(root.join("b.txt").as_std_path(), b"b").unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn dir_hash_fast_vs_full_differ() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("test.txt").as_std_path(), b"data").unwrap();

    let fast = FsTreeHasher::new(HashMode::Fast).unwrap();
    let full = FsTreeHasher::new(HashMode::Full).unwrap();

    assert_ne!(
        fast.hash_dir(root).unwrap(),
        full.hash_dir(root).unwrap(),
        "Fast and Full modes use different inputs, hashes should differ"
    );
}

// ===========================================================================
// Caching behavior
// ===========================================================================

#[test]
fn file_hash_populates_cache() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("cached.txt");
    fs::write(file.as_std_path(), b"cache me").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_eq!(hasher.cached_count(), (0, 0));

    hasher.hash_file(&file).unwrap();
    let (files, dirs) = hasher.cached_count();
    assert_eq!(files, 1);
    assert_eq!(dirs, 0);
}

#[test]
fn dir_hash_populates_cache() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("a.txt").as_std_path(), b"a").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_eq!(hasher.cached_count(), (0, 0));

    hasher.hash_dir(root).unwrap();
    let (files, dirs) = hasher.cached_count();
    assert_eq!(files, 0); // dir hash doesn't cache individual files
    assert_eq!(dirs, 1);
}

#[test]
fn multiple_files_cached_independently() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let f1 = root.join("a.txt");
    let f2 = root.join("b.txt");
    let f3 = root.join("c.txt");
    fs::write(f1.as_std_path(), b"a").unwrap();
    fs::write(f2.as_std_path(), b"b").unwrap();
    fs::write(f3.as_std_path(), b"c").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    hasher.hash_file(&f1).unwrap();
    assert_eq!(hasher.cached_count().0, 1);

    hasher.hash_file(&f2).unwrap();
    assert_eq!(hasher.cached_count().0, 2);

    hasher.hash_file(&f3).unwrap();
    assert_eq!(hasher.cached_count().0, 3);
}

#[test]
fn cache_hit_returns_same_value() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("test.txt");
    fs::write(file.as_std_path(), b"test content").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_file(&file).unwrap(); // cache miss
    let h2 = hasher.hash_file(&file).unwrap(); // cache hit
    let h3 = hasher.hash_file(&file).unwrap(); // cache hit
    assert_eq!(h1, h2);
    assert_eq!(h2, h3);
}

// ===========================================================================
// Invalidation
// ===========================================================================

#[test]
fn invalidate_file_forces_recompute() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("test.txt");
    fs::write(file.as_std_path(), b"before").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_file(&file).unwrap();
    assert_eq!(hasher.cached_count().0, 1);

    // Mutate the file without waiting for watcher
    fs::write(file.as_std_path(), b"after").unwrap();
    hasher.invalidate(&file);

    let h2 = hasher.hash_file(&file).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn invalidate_dir_forces_recompute() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("a.txt").as_std_path(), b"original").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    assert_eq!(hasher.cached_count().1, 1);

    fs::write(root.join("a.txt").as_std_path(), b"changed").unwrap();
    hasher.invalidate(root);

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn invalidate_all_clears_files_and_dirs() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("test.txt");
    fs::write(file.as_std_path(), b"test").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    hasher.hash_file(&file).unwrap();
    hasher.hash_dir(root).unwrap();
    assert_eq!(hasher.cached_count(), (1, 1));

    hasher.invalidate_all();
    assert_eq!(hasher.cached_count(), (0, 0));
}

#[test]
fn invalidate_all_then_rehash_gives_same_result() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("stable.txt");
    fs::write(file.as_std_path(), b"unchanged").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_file(&file).unwrap();

    hasher.invalidate_all();

    let h2 = hasher.hash_file(&file).unwrap();
    assert_eq!(h1, h2, "Same content after invalidation should produce same hash");
}

#[test]
fn invalidate_nonexistent_path_is_noop() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("real.txt");
    fs::write(file.as_std_path(), b"exists").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    hasher.hash_file(&file).unwrap();
    assert_eq!(hasher.cached_count().0, 1);

    // Invalidating a path that was never hashed shouldn't panic
    hasher.invalidate(&root.join("nonexistent.txt"));
    // The real file's cache may or may not be evicted depending on
    // implementation, but at minimum it shouldn't panic
}

#[test]
fn invalidate_parent_dir_evicts_child_dir() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let sub = root.join("child");
    fs::create_dir(sub.as_std_path()).unwrap();
    fs::write(sub.join("file.txt").as_std_path(), b"data").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    hasher.hash_dir(&sub).unwrap();
    assert_eq!(hasher.cached_count().1, 1);

    hasher.invalidate(root);
    assert_eq!(hasher.cached_count().1, 0, "Invalidating parent should evict child dir cache");
}

// ===========================================================================
// Watcher-based cache invalidation
// ===========================================================================

#[test]
fn watcher_evicts_file_on_modification() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("watched.txt");
    fs::write(file.as_std_path(), b"original").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_file(&file).unwrap();
    assert_eq!(hasher.cached_count().0, 1);

    settle();
    fs::write(file.as_std_path(), b"modified").unwrap();
    settle();

    // After watcher fires, cache should be evicted and recompute should happen
    let h2 = hasher.hash_file(&file).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn watcher_evicts_dir_on_file_add() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("existing.txt").as_std_path(), b"data").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    assert_eq!(hasher.cached_count().1, 1);

    settle();
    fs::write(root.join("new_file.txt").as_std_path(), b"new").unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn watcher_evicts_dir_on_file_delete() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("to_delete.txt");
    fs::write(file.as_std_path(), b"soon gone").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();

    settle();
    fs::remove_file(file.as_std_path()).unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn watcher_multiple_rapid_changes() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("rapid.txt");
    fs::write(file.as_std_path(), b"v0").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let _h0 = hasher.hash_file(&file).unwrap();
    settle();

    // Rapid successive writes
    for i in 1..=5 {
        fs::write(file.as_std_path(), format!("version{i}").as_bytes()).unwrap();
    }
    settle();

    // Should reflect the final state
    let hfinal = hasher.hash_file(&file).unwrap();

    // Hash a fresh hasher to verify
    let fresh = FsTreeHasher::new(HashMode::Full).unwrap();
    let hfresh = fresh.hash_file(&file).unwrap();
    assert_eq!(hfinal, hfresh, "After rapid changes, hash should match a fresh computation");
}

// ===========================================================================
// Special filenames
// ===========================================================================

#[test]
fn file_with_spaces_in_name() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("file with spaces.txt");
    fs::write(file.as_std_path(), b"spaced").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h = hasher.hash_file(&file).unwrap();
    assert_eq!(h, hasher.hash_file(&file).unwrap());
}

#[test]
fn file_with_unicode_name() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("日本語.txt");
    fs::write(file.as_std_path(), b"unicode filename").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h = hasher.hash_file(&file).unwrap();
    assert_eq!(h, hasher.hash_file(&file).unwrap());
}

#[test]
fn file_with_dots_and_dashes() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("my-file.test.backup.txt");
    fs::write(file.as_std_path(), b"dotty").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h = hasher.hash_file(&file).unwrap();
    assert_eq!(h, hasher.hash_file(&file).unwrap());
}

#[test]
fn dir_with_dotfiles() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join(".hidden").as_std_path(), b"secret").unwrap();
    fs::write(root.join("visible.txt").as_std_path(), b"public").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

// ===========================================================================
// Directory structure variations
// ===========================================================================

#[test]
fn dir_hash_with_empty_subdirectories() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::create_dir(root.join("empty1").as_std_path()).unwrap();
    fs::create_dir(root.join("empty2").as_std_path()).unwrap();
    fs::write(root.join("file.txt").as_std_path(), b"data").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn dir_hash_same_files_different_subdirs() {
    // dir1/sub_a/file.txt vs dir2/sub_b/file.txt — different paths → different hash
    let dir1 = tmp_dir();
    let dir2 = tmp_dir();
    let r1 = tmp_utf8(&dir1);
    let r2 = tmp_utf8(&dir2);

    fs::create_dir(r1.join("sub_a").as_std_path()).unwrap();
    fs::write(r1.join("sub_a/file.txt").as_std_path(), b"content").unwrap();

    fs::create_dir(r2.join("sub_b").as_std_path()).unwrap();
    fs::write(r2.join("sub_b/file.txt").as_std_path(), b"content").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_ne!(
        hasher.hash_dir(r1).unwrap(),
        hasher.hash_dir(r2).unwrap(),
        "Files in different subdirectory names should produce different hashes"
    );
}

#[test]
fn dir_hash_mirror_directories_match() {
    // Two directories with identical structure should hash identically
    let dir1 = tmp_dir();
    let dir2 = tmp_dir();
    let r1 = tmp_utf8(&dir1);
    let r2 = tmp_utf8(&dir2);

    for root in [r1, r2] {
        fs::create_dir(root.join("src").as_std_path()).unwrap();
        fs::write(root.join("src/main.cpp").as_std_path(), b"int main() {}").unwrap();
        fs::write(root.join("src/util.cpp").as_std_path(), b"void util() {}").unwrap();
        fs::create_dir(root.join("include").as_std_path()).unwrap();
        fs::write(root.join("include/util.h").as_std_path(), b"#pragma once").unwrap();
    }

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_eq!(
        hasher.hash_dir(r1).unwrap(),
        hasher.hash_dir(r2).unwrap(),
        "Mirrored directory structures with same content should match"
    );
}

#[test]
fn dir_hash_add_file_to_subdirectory() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let sub = root.join("sub");
    fs::create_dir(sub.as_std_path()).unwrap();
    fs::write(sub.join("existing.txt").as_std_path(), b"exists").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    settle();

    fs::write(sub.join("new.txt").as_std_path(), b"new").unwrap();
    settle();

    let h2 = hasher.hash_dir(root).unwrap();
    assert_ne!(h1, h2, "Adding a file in a subdirectory should change parent dir hash");
}

// ===========================================================================
// Build system scenarios
// ===========================================================================

#[test]
fn simulate_cpp_project_rebuild_detection() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);

    // Create a minimal C++ project structure
    let src = root.join("src");
    let inc = root.join("include");
    fs::create_dir(src.as_std_path()).unwrap();
    fs::create_dir(inc.as_std_path()).unwrap();
    fs::write(src.join("main.cpp").as_std_path(), b"int main() { return 0; }").unwrap();
    fs::write(src.join("util.cpp").as_std_path(), b"int add(int a, int b) { return a + b; }").unwrap();
    fs::write(inc.join("util.h").as_std_path(), b"int add(int, int);").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();

    // Initial hash
    let src_hash1 = hasher.hash_dir(&src).unwrap();
    let inc_hash1 = hasher.hash_dir(&inc).unwrap();
    settle();

    // Modify a source file
    fs::write(
        src.join("util.cpp").as_std_path(),
        b"int add(int a, int b) { return a + b; }\nint sub(int a, int b) { return a - b; }",
    )
    .unwrap();
    settle();

    let src_hash2 = hasher.hash_dir(&src).unwrap();
    let inc_hash2 = hasher.hash_dir(&inc).unwrap();

    assert_ne!(src_hash1, src_hash2, "src/ should need rebuild");
    assert_eq!(inc_hash1, inc_hash2, "include/ should NOT need rebuild");
}

#[test]
fn simulate_header_change_triggers_rebuild() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let inc = root.join("include");
    fs::create_dir(inc.as_std_path()).unwrap();
    fs::write(inc.join("config.h").as_std_path(), b"#define VERSION 1").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(&inc).unwrap();
    settle();

    fs::write(inc.join("config.h").as_std_path(), b"#define VERSION 2").unwrap();
    settle();

    let h2 = hasher.hash_dir(&inc).unwrap();
    assert_ne!(h1, h2, "Header change should be detected");
}

#[test]
fn simulate_incremental_file_tracking() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let src = root.join("src");
    fs::create_dir(src.as_std_path()).unwrap();
    fs::write(src.join("a.cpp").as_std_path(), b"void a() {}").unwrap();
    fs::write(src.join("b.cpp").as_std_path(), b"void b() {}").unwrap();
    fs::write(src.join("c.cpp").as_std_path(), b"void c() {}").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let ha1 = hasher.hash_file(&src.join("a.cpp")).unwrap();
    let hb1 = hasher.hash_file(&src.join("b.cpp")).unwrap();
    let hc1 = hasher.hash_file(&src.join("c.cpp")).unwrap();
    settle();

    // Only modify b.cpp
    fs::write(src.join("b.cpp").as_std_path(), b"void b() { /* changed */ }").unwrap();
    settle();

    let ha2 = hasher.hash_file(&src.join("a.cpp")).unwrap();
    let hb2 = hasher.hash_file(&src.join("b.cpp")).unwrap();
    let hc2 = hasher.hash_file(&src.join("c.cpp")).unwrap();

    assert_eq!(ha1, ha2, "a.cpp unchanged");
    assert_ne!(hb1, hb2, "b.cpp changed");
    assert_eq!(hc1, hc2, "c.cpp unchanged");
}

// ===========================================================================
// Concurrent usage
// ===========================================================================

#[test]
fn concurrent_file_hashing() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);

    for i in 0..20 {
        fs::write(
            root.join(format!("f{i}.txt")).as_std_path(),
            format!("content_{i}").as_bytes(),
        )
        .unwrap();
    }

    let hasher = std::sync::Arc::new(FsTreeHasher::new(HashMode::Full).unwrap());
    let root_owned = root.to_owned();

    let handles: Vec<_> = (0..20)
        .map(|i| {
            let hasher = hasher.clone();
            let root = root_owned.clone();
            thread::spawn(move || {
                let file = root.join(format!("f{i}.txt"));
                hasher.hash_file(&file).unwrap()
            })
        })
        .collect();

    let hashes: Vec<ContentHash> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All hashes should be valid (non-panicking) and each file should
    // have its own distinct hash (since content differs)
    let unique: HashSet<_> = hashes.iter().collect();
    assert_eq!(unique.len(), 20, "20 files with distinct content should produce 20 distinct hashes");
}

#[test]
fn concurrent_dir_hashing() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    fs::write(root.join("data.txt").as_std_path(), b"shared").unwrap();

    let hasher = std::sync::Arc::new(FsTreeHasher::new(HashMode::Full).unwrap());
    let root_owned = root.to_owned();

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let hasher = hasher.clone();
            let root = root_owned.clone();
            thread::spawn(move || hasher.hash_dir(&root).unwrap())
        })
        .collect();

    let hashes: Vec<ContentHash> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All concurrent reads of the same dir should produce the same hash
    for h in &hashes {
        assert_eq!(*h, hashes[0]);
    }
}

// ===========================================================================
// Symlink tests (cross-platform)
// ===========================================================================

/// Create a directory symlink. Works on both Unix and Windows.
/// On Windows, requires Developer Mode or Administrator privileges.
fn create_dir_symlink(target: &Utf8Path, link: &Utf8Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target.as_std_path(), link.as_std_path())
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target.as_std_path(), link.as_std_path())
    }
}

/// Remove a directory symlink. On Windows, directory symlinks are removed
/// with `remove_dir`; on Unix, with `remove_file`.
fn remove_dir_symlink(link: &Utf8Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::remove_file(link.as_std_path())
    }
    #[cfg(windows)]
    {
        fs::remove_dir(link.as_std_path())
    }
}

#[test]
fn symlink_dir_hash_with_internal_symlinked_dir() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let real = root.join("real");
    fs::create_dir(real.as_std_path()).unwrap();
    fs::write(real.join("file.txt").as_std_path(), b"data").unwrap();

    if create_dir_symlink(&real, &root.join("link")).is_err() {
        eprintln!("Skipping symlink test: insufficient privileges");
        return;
    }

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn symlink_external_change_detected() {
    let project = tmp_dir();
    let external = tmp_dir();
    let proj = tmp_utf8(&project);
    let ext = tmp_utf8(&external);

    fs::write(ext.join("lib.h").as_std_path(), b"v1").unwrap();
    if create_dir_symlink(ext, &proj.join("vendor")).is_err() {
        eprintln!("Skipping symlink test: insufficient privileges");
        return;
    }

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(proj).unwrap();
    settle();

    fs::write(ext.join("lib.h").as_std_path(), b"v2").unwrap();
    settle();

    let h2 = hasher.hash_dir(proj).unwrap();
    assert_ne!(h1, h2, "Change in external symlink target should be detected");
}

#[test]
fn symlink_external_file_add_detected() {
    let project = tmp_dir();
    let external = tmp_dir();
    let proj = tmp_utf8(&project);
    let ext = tmp_utf8(&external);

    fs::write(ext.join("existing.h").as_std_path(), b"exists").unwrap();
    if create_dir_symlink(ext, &proj.join("ext")).is_err() {
        eprintln!("Skipping symlink test: insufficient privileges");
        return;
    }

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(proj).unwrap();
    settle();

    fs::write(ext.join("new.h").as_std_path(), b"new file").unwrap();
    settle();

    let h2 = hasher.hash_dir(proj).unwrap();
    assert_ne!(h1, h2, "Adding file in external symlink target should be detected");
}

#[test]
fn symlink_hash_file_through_symlink_dir() {
    let project = tmp_dir();
    let external = tmp_dir();
    let proj = tmp_utf8(&project);
    let ext = tmp_utf8(&external);

    fs::write(ext.join("header.h").as_std_path(), b"original").unwrap();
    if create_dir_symlink(ext, &proj.join("inc")).is_err() {
        eprintln!("Skipping symlink test: insufficient privileges");
        return;
    }

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_file(&proj.join("inc/header.h")).unwrap();
    settle();

    fs::write(ext.join("header.h").as_std_path(), b"modified").unwrap();
    settle();

    let h2 = hasher.hash_file(&proj.join("inc/header.h")).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn symlink_retarget_changes_dir_hash() {
    let project = tmp_dir();
    let target_a = tmp_dir();
    let target_b = tmp_dir();
    let proj = tmp_utf8(&project);
    let ta = tmp_utf8(&target_a);
    let tb = tmp_utf8(&target_b);

    fs::write(ta.join("lib.h").as_std_path(), b"target A").unwrap();
    fs::write(tb.join("lib.h").as_std_path(), b"target B").unwrap();

    let link = proj.join("vendor");
    if create_dir_symlink(ta, &link).is_err() {
        eprintln!("Skipping symlink test: insufficient privileges");
        return;
    }

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(proj).unwrap();
    settle();

    // Retarget symlink
    remove_dir_symlink(&link).unwrap();
    create_dir_symlink(tb, &link).unwrap();
    settle();

    let h2 = hasher.hash_dir(proj).unwrap();
    assert_ne!(h1, h2, "Retargeting a symlink should change the dir hash");
}

// ===========================================================================
// Edge cases
// ===========================================================================

#[test]
fn hash_file_with_only_newlines() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let f1 = root.join("one_newline.txt");
    let f2 = root.join("two_newlines.txt");
    fs::write(f1.as_std_path(), b"\n").unwrap();
    fs::write(f2.as_std_path(), b"\n\n").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    assert_ne!(
        hasher.hash_file(&f1).unwrap(),
        hasher.hash_file(&f2).unwrap(),
    );
}

#[test]
fn hash_file_with_null_bytes() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("nulls.bin");
    fs::write(file.as_std_path(), &[0u8; 64]).unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h = hasher.hash_file(&file).unwrap();
    assert_eq!(h, hasher.hash_file(&file).unwrap());
}

#[test]
fn hash_dir_with_mixed_file_types() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);

    // Source files, headers, build files, hidden files
    fs::write(root.join("main.cpp").as_std_path(), b"int main() {}").unwrap();
    fs::write(root.join("util.h").as_std_path(), b"#pragma once").unwrap();
    fs::write(root.join("Makefile").as_std_path(), b"all: main").unwrap();
    fs::write(root.join(".gitignore").as_std_path(), b"*.o").unwrap();
    fs::write(root.join("README").as_std_path(), b"readme").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn invalidate_then_file_deleted_returns_error() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("temp.txt");
    fs::write(file.as_std_path(), b"temporary").unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    hasher.hash_file(&file).unwrap();

    fs::remove_file(file.as_std_path()).unwrap();
    hasher.invalidate(&file);

    // After invalidation + deletion, re-hashing should return an error
    assert!(hasher.hash_file(&file).is_err());
}

#[test]
fn hash_same_file_from_both_modes() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("dual.txt");
    fs::write(file.as_std_path(), b"test content").unwrap();

    let fast = FsTreeHasher::new(HashMode::Fast).unwrap();
    let full = FsTreeHasher::new(HashMode::Full).unwrap();

    // Both should succeed
    let hfast = fast.hash_file(&file).unwrap();
    let hfull = full.hash_file(&file).unwrap();

    // Both should be deterministic
    assert_eq!(hfast, fast.hash_file(&file).unwrap());
    assert_eq!(hfull, full.hash_file(&file).unwrap());
}

#[test]
fn dir_hash_survives_read_only_file() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);
    let file = root.join("readonly.txt");
    fs::write(file.as_std_path(), b"locked down").unwrap();

    // Make read-only
    let mut perms = fs::metadata(file.as_std_path()).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(file.as_std_path(), perms).unwrap();

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h = hasher.hash_dir(root).unwrap();
    assert_eq!(h, hasher.hash_dir(root).unwrap());

    // Cleanup: restore write permission so tempdir can be deleted
    let mut perms = fs::metadata(file.as_std_path()).unwrap().permissions();
    perms.set_readonly(false);
    fs::set_permissions(file.as_std_path(), perms).unwrap();
}

#[test]
fn dir_hash_with_multiple_subdirectories() {
    let dir = tmp_dir();
    let root = tmp_utf8(&dir);

    for sub in &["src", "include", "tests", "docs", "build"] {
        let s = root.join(sub);
        fs::create_dir(s.as_std_path()).unwrap();
        fs::write(
            s.join(format!("{sub}.txt")).as_std_path(),
            format!("{sub} content").as_bytes(),
        )
        .unwrap();
    }

    let hasher = FsTreeHasher::new(HashMode::Full).unwrap();
    let h1 = hasher.hash_dir(root).unwrap();
    let h2 = hasher.hash_dir(root).unwrap();
    assert_eq!(h1, h2);
}
