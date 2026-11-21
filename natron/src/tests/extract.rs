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

#[test]
fn vsix_rejects_unsafe_entry_paths() {
    // `C:foo` is a drive-relative prefix on Windows; elsewhere it is a plain
    // name, but it's never a legitimate VSIX path, so it fails everywhere
    // that the path parser sees a prefix, and `..` fails everywhere.
    let mut names = vec!["Contents/../escape.txt", "Contents/a/../../escape.txt"];
    if cfg!(windows) {
        names.push("Contents/C:escape.txt");
    }
    for name in names {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("a.vsix");
        build_zip(&archive, &[(name, b"pwned")]);
        let dest = tmp.path().join("out");
        assert!(extract_vsix(&archive, &dest).is_err(), "{name} should be rejected");
        assert!(!tmp.path().join("escape.txt").exists());
    }
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

#[cfg(unix)]
#[test]
fn tar_chained_symlink_escape_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    // `sub/x -> ..` is the root and `a -> sub/x/..` is lexically `sub`, but
    // physically `a` is the root's parent. Neither link sits inside the other.
    write_tar_xz(
        &archive,
        &[
            TarEntry::File("sub/keep", b"k", 0o644),
            TarEntry::Symlink("sub/x", ".."),
            TarEntry::Symlink("a", "sub/x/.."),
        ],
    );
    let dest = tmp.path().join("out");
    assert!(extract_archive(&archive, ArchiveKind::TarXz, &dest, None).is_err());
}

#[cfg(unix)]
#[test]
fn tar_dangling_symlink_escape_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    // `a` names a file that doesn't exist yet, in the root's parent; a later
    // write to `a` would create it there.
    write_tar_xz(
        &archive,
        &[
            TarEntry::File("sub/keep", b"k", 0o644),
            TarEntry::Symlink("sub/x", ".."),
            TarEntry::Symlink("a", "sub/x/../missing"),
        ],
    );
    let dest = tmp.path().join("out");
    assert!(extract_archive(&archive, ArchiveKind::TarXz, &dest, None).is_err());
}

#[cfg(unix)]
#[test]
fn tar_symlink_from_earlier_archive_is_not_written_through() {
    // The deb provider extracts several packages into one directory. A link
    // the first plants must not let the second write outside it.
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("out");
    let first = tmp.path().join("first.tar.xz");
    write_tar_xz(
        &first,
        &[
            TarEntry::File("usr/keep", b"k", 0o644),
            TarEntry::Symlink("usr/up", ".."),
            TarEntry::Symlink("usr/x", "up/.."),
        ],
    );
    let second = tmp.path().join("second.tar.xz");
    write_tar_xz(&second, &[TarEntry::File("usr/x/pwned", b"x", 0o644)]);
    let first_ok = extract_archive(&first, ArchiveKind::TarXz, &dest, None).is_ok();
    let second_ok = first_ok && extract_archive(&second, ArchiveKind::TarXz, &dest, None).is_ok();
    assert!(!tmp.path().join("pwned").exists(), "second archive wrote outside dest");
    assert!(!second_ok);
}

#[cfg(unix)]
#[test]
fn tar_dangling_symlink_inside_root_is_kept() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    write_tar_xz(
        &archive,
        &[
            TarEntry::File("lib/libfoo.so.1", b"ELF", 0o755),
            TarEntry::Symlink("lib/libfoo.so", "libfoo.so.1"),
            TarEntry::Symlink("lib/libbar.so", "libbar.so.1"),
            TarEntry::Symlink("lib64", "lib"),
        ],
    );
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::TarXz, &dest, None).unwrap();
    assert_eq!(std::fs::read(dest.join("lib64/libfoo.so")).unwrap(), b"ELF");
}

#[cfg(unix)]
#[test]
fn tar_link_through_earlier_archive_link_cannot_act_outside() {
    // The first archive plants `a/d -> .`. In the second, `a/d/d/d/d/e` is
    // lexically `a/e -> ..` (the root), but on disk it is `a/e`, three levels
    // above the root; the next link would then create directories outside.
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("one/two/three");
    let dest = base.join("out");
    let first = tmp.path().join("first.tar.xz");
    write_tar_xz(&first, &[TarEntry::File("a/keep", b"k", 0o644), TarEntry::Symlink("a/d", ".")]);
    extract_archive(&first, ArchiveKind::TarXz, &dest, None).unwrap();
    let second = tmp.path().join("second.tar.xz");
    write_tar_xz(
        &second,
        &[
            TarEntry::Symlink("a/d/d/d/d/e", "../../../.."),
            TarEntry::Symlink("a/e/made/link", "x"),
        ],
    );
    assert!(extract_archive(&second, ArchiveKind::TarXz, &dest, None).is_err());
    assert!(!tmp.path().join("one/made").exists(), "created a directory outside dest");
    assert!(std::fs::symlink_metadata(dest.join("a/e")).is_err(), "escaping link left behind");
}

#[cfg(unix)]
#[test]
fn tar_replacing_a_checked_link_is_refused() {
    // `a -> b/..` is `sub` while `b -> sub/deep`; re-pointing `b -> .` would
    // make `a` the root's parent without `a` being checked again.
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    write_tar_xz(
        &archive,
        &[
            TarEntry::File("sub/deep/keep", b"k", 0o644),
            TarEntry::Symlink("b", "sub/deep"),
            TarEntry::Symlink("a", "b/.."),
            TarEntry::Symlink("b", "."),
        ],
    );
    let dest = tmp.path().join("out");
    assert!(extract_archive(&archive, ArchiveKind::TarXz, &dest, None).is_err());
}

#[cfg(unix)]
#[test]
fn tar_link_may_replace_a_file_or_an_equivalent_link() {
    // What a case-insensitive filesystem does with python-build-standalone's
    // terminfo (`E/Eterm-color` link over `e/eterm-color` file, `P/P12 ->
    // ../p/prism12` over `p/p12 -> prism12`), spelled with one case.
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("a.tar.xz");
    write_tar_xz(
        &archive,
        &[
            TarEntry::File("t/eterm", b"ETERM", 0o644),
            TarEntry::File("t/eterm-color", b"OLD", 0o644),
            TarEntry::File("t/prism12", b"PRISM", 0o644),
            TarEntry::Symlink("t/eterm-color", "eterm"),
            TarEntry::Symlink("t/p12", "prism12"),
            TarEntry::Symlink("t/p12", "../t/prism12"),
        ],
    );
    let dest = tmp.path().join("out");
    extract_archive(&archive, ArchiveKind::TarXz, &dest, None).unwrap();
    assert_eq!(std::fs::read(dest.join("t/eterm-color")).unwrap(), b"ETERM");
    assert_eq!(std::fs::read(dest.join("t/p12")).unwrap(), b"PRISM");
}

#[cfg(unix)]
#[test]
fn tar_repeated_identical_link_is_kept() {
    // Two packages shipping the same link into one directory is fine.
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("out");
    let archive = tmp.path().join("a.tar.xz");
    write_tar_xz(
        &archive,
        &[TarEntry::File("usr/lib/libz.so.1", b"ELF", 0o755), TarEntry::Symlink("usr/lib/libz.so", "libz.so.1")],
    );
    extract_archive(&archive, ArchiveKind::TarXz, &dest, None).unwrap();
    extract_archive(&archive, ArchiveKind::TarXz, &dest, None).unwrap();
    assert_eq!(std::fs::read(dest.join("usr/lib/libz.so")).unwrap(), b"ELF");
}
