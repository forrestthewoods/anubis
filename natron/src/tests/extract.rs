//! Tests for `src/extract.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use std::io::Write as _;
use tempfile::TempDir;

fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let f = File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::FileOptions::default();
    for (name, data) in entries {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(data).unwrap();
    }
    zw.finish().unwrap();
}

fn build_tar_xz(path: &Path, entries: &[(&str, &[u8])]) {
    let f = File::create(path).unwrap();
    let enc = liblzma::write::XzEncoder::new(f, 0);
    let mut tar = tar::Builder::new(enc);
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, *data).unwrap();
    }
    tar.finish().unwrap();
    // Drop the builder to flush the encoder.
}

#[test]
fn extracts_zip_no_strip() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.zip");
    build_zip(
        &archive,
        &[
            ("a.txt", b"AAA"),
            ("d/b.txt", b"BBB"),
        ],
    );
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::Zip, &dest, None).unwrap();
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"AAA");
    assert_eq!(std::fs::read(dest.join("d").join("b.txt")).unwrap(), b"BBB");
}

#[test]
fn extracts_zip_with_strip_prefix() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.zip");
    build_zip(
        &archive,
        &[
            ("clang+llvm-21/bin/clang.exe", b"BIN"),
            ("clang+llvm-21/LICENSE", b"LIC"),
            ("README", b"discard-me"),
        ],
    );
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::Zip, &dest, Some("clang+llvm-21")).unwrap();
    assert_eq!(std::fs::read(dest.join("bin").join("clang.exe")).unwrap(), b"BIN");
    assert_eq!(std::fs::read(dest.join("LICENSE")).unwrap(), b"LIC");
    // Out-of-prefix entries are dropped silently.
    assert!(!dest.join("README").exists());
}

#[test]
fn extracts_tar_xz() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    build_tar_xz(
        &archive,
        &[
            ("clang+llvm/bin/clang", b"!!"),
            ("clang+llvm/LICENSE", b"L"),
        ],
    );
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::TarXz, &dest, Some("clang+llvm")).unwrap();
    assert_eq!(std::fs::read(dest.join("bin").join("clang")).unwrap(), b"!!");
    assert_eq!(std::fs::read(dest.join("LICENSE")).unwrap(), b"L");
}

#[test]
fn rejects_zip_slip_in_zip() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.zip");
    let f = File::create(&archive).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::FileOptions::default();
    // The `zip` crate normalizes names on writing; simulate slip by
    // using a name with backslash on a non-Windows system. On Windows
    // it'll likely be rejected by enclosed_name() too.
    zw.start_file("../escape.txt", opts).unwrap();
    zw.write_all(b"x").unwrap();
    zw.finish().unwrap();

    let dest = tmp.path().join("out");
    let res = extract_archive(&archive, ArchiveKind::Zip, &dest, None);
    // Either: extraction errored (zip-slip detected), or the entry's
    // sanitized form landed inside dest. Either is safe.
    if let Ok(()) = res {
        // Verify nothing escaped above dest.
        let parent = dest.parent().unwrap();
        assert!(!parent.join("escape.txt").exists(), "zip-slip succeeded!");
    }
}

// Note: testing tar-slip end-to-end would require manually constructing
// raw tar bytes — the `tar` crate's Builder refuses to write `..` paths.
// The defense in `extract_tar_inner` (rejecting absolute paths and
// `ParentDir` components) is still load-bearing for hand-crafted tars
// produced by external tools.

#[test]
fn apply_strip_prefix_basic() {
    // Match
    assert_eq!(
        apply_strip_prefix(Path::new("foo/bar/baz"), Some("foo")),
        Some(PathBuf::from("bar/baz"))
    );
    // No prefix (None) returns input unchanged
    assert_eq!(
        apply_strip_prefix(Path::new("foo/bar"), None),
        Some(PathBuf::from("foo/bar"))
    );
    // Mismatch -> None (entry dropped)
    assert_eq!(
        apply_strip_prefix(Path::new("other/file"), Some("foo")),
        None
    );
    // Multi-segment prefix
    assert_eq!(
        apply_strip_prefix(Path::new("a/b/c/d.txt"), Some("a/b")),
        Some(PathBuf::from("c/d.txt"))
    );
}

#[test]
fn vsix_strips_contents_prefix() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.vsix");
    build_zip(
        &archive,
        &[
            ("Contents/bin/cl.exe", b"CL"),
            ("[Content_Types].xml", b"meta"),
            ("Contents/lib/foo.lib", b"FOO"),
        ],
    );
    let dest = tmp.path().join("out");
    extract_vsix(&archive, &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("bin").join("cl.exe")).unwrap(), b"CL");
    assert_eq!(std::fs::read(dest.join("lib").join("foo.lib")).unwrap(), b"FOO");
    assert!(!dest.join("[Content_Types].xml").exists());
}

/// One tar entry for `build_tar`: regular file, symlink, or hard link.
enum TarEntry<'a> {
    File(&'a str, &'a [u8], u32),
    Symlink(&'a str, &'a str),
    HardLink(&'a str, &'a str),
}

fn build_tar<W: std::io::Write>(w: W, entries: &[TarEntry]) -> W {
    let mut tar = tar::Builder::new(w);
    for e in entries {
        let mut header = tar::Header::new_gnu();
        match e {
            TarEntry::File(name, data, mode) => {
                header.set_size(data.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                tar.append_data(&mut header, name, *data).unwrap();
            }
            TarEntry::Symlink(name, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                tar.append_link(&mut header, name, target).unwrap();
            }
            TarEntry::HardLink(name, target) => {
                header.set_entry_type(tar::EntryType::Link);
                header.set_size(0);
                header.set_mode(0o755);
                tar.append_link(&mut header, name, target).unwrap();
            }
        }
    }
    tar.into_inner().unwrap()
}

fn write_tar_xz(path: &Path, entries: &[TarEntry]) {
    let enc = liblzma::write::XzEncoder::new(File::create(path).unwrap(), 0);
    build_tar(enc, entries).finish().unwrap();
}

#[test]
fn tar_gz_extracts() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.gz");
    let enc = flate2::write::GzEncoder::new(File::create(&archive).unwrap(), flate2::Compression::fast());
    build_tar(enc, &[TarEntry::File("pkg/bin/tool", b"TOOL", 0o755)]).finish().unwrap();
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::TarGz, &dest, Some("pkg")).unwrap();
    assert_eq!(std::fs::read(dest.join("bin/tool")).unwrap(), b"TOOL");
}

#[cfg(unix)]
#[test]
fn tar_keeps_exec_bits() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    // A big file goes through the inline write path, a small one through the
    // writer pool; both must keep their mode.
    let big = vec![7u8; (TAR_INLINE_WRITE_THRESHOLD + 1) as usize];
    write_tar_xz(
        &archive,
        &[
            TarEntry::File("bin/clang", b"ELF", 0o755),
            TarEntry::File("bin/lld", &big, 0o755),
            TarEntry::File("share/doc.txt", b"doc", 0o644),
        ],
    );
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::TarXz, &dest, None).unwrap();
    let mode = |p: &str| std::fs::metadata(dest.join(p)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("bin/clang"), 0o755);
    assert_eq!(mode("bin/lld"), 0o755);
    assert_eq!(mode("share/doc.txt"), 0o644);
}

#[cfg(unix)]
#[test]
fn zip_keeps_exec_bits() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.zip");
    let mut zw = zip::ZipWriter::new(File::create(&archive).unwrap());
    zw.start_file("bin/zig", zip::write::FileOptions::default().unix_permissions(0o755)).unwrap();
    zw.write_all(b"ZIG").unwrap();
    zw.finish().unwrap();
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::Zip, &dest, None).unwrap();
    let mode = std::fs::metadata(dest.join("bin/zig")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
}

#[test]
fn tar_hard_links_get_their_target_bytes() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    write_tar_xz(
        &archive,
        &[
            TarEntry::File("pkg/bin/clang-21", b"CLANG", 0o755),
            TarEntry::HardLink("pkg/bin/clang++", "pkg/bin/clang-21"),
        ],
    );
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::TarXz, &dest, Some("pkg")).unwrap();
    assert_eq!(std::fs::read(dest.join("bin/clang++")).unwrap(), b"CLANG");
}

#[cfg(unix)]
#[test]
fn tar_symlink_inside_root_is_kept() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    write_tar_xz(
        &archive,
        &[
            TarEntry::File("bin/clang-21", b"CLANG", 0o755),
            TarEntry::Symlink("bin/clang", "clang-21"),
        ],
    );
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::TarXz, &dest, None).unwrap();
    assert_eq!(std::fs::read(dest.join("bin/clang")).unwrap(), b"CLANG");
}

#[cfg(unix)]
#[test]
fn tar_symlink_escape_is_refused() {
    let tmp = TempDir::new().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let archive = tmp.path().join("a.tar.xz");
    // `esc -> <outside>` then `esc/pwned`: before the fix, pwned landed in
    // `outside`.
    write_tar_xz(
        &archive,
        &[
            TarEntry::Symlink("esc", outside.to_str().unwrap()),
            TarEntry::File("esc/pwned", b"x", 0o644),
        ],
    );
    let dest = tmp.path().join("out");
    assert!(extract_archive(&archive, ArchiveKind::TarXz, &dest, None).is_err());
    assert!(!outside.join("pwned").exists());
}

#[cfg(unix)]
#[test]
fn tar_relative_symlink_escape_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    write_tar_xz(&archive, &[TarEntry::Symlink("a/up", "../../etc")]);
    let dest = tmp.path().join("out");
    assert!(extract_archive(&archive, ArchiveKind::TarXz, &dest, None).is_err());
}

#[cfg(unix)]
#[test]
fn tar_symlink_under_symlink_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    // `a -> .` then `a/x -> ../y`: lexically inside, physically outside.
    write_tar_xz(
        &archive,
        &[TarEntry::Symlink("a", "."), TarEntry::Symlink("a/x", "../y")],
    );
    let dest = tmp.path().join("out");
    assert!(extract_archive(&archive, ArchiveKind::TarXz, &dest, None).is_err());
}
