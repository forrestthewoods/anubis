//! Hermetic tests for the pure-Rust MSI extractor.
//!
//! Builds synthetic MSIs in-process via `msi::Package::create` +
//! `cab::CabinetBuilder` so the tests run on any platform — no
//! msiexec required. Validates that the extractor produces the
//! correct on-disk layout for the MSI features SDK installers
//! actually use.

use super::extract_msi;

use anyhow::Result;
use cab::{CabinetBuilder, CompressionType};
use msi::{Column, Insert, PackageType, Value};
use std::fs::File;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ============================================================================
// Helpers
// ============================================================================

/// Build a CAB containing the given files (key, bytes) using MSZIP
/// compression. The CAB entry name is the same as the key — for MSI
/// CABs that's the `File` table's primary key.
fn build_cab(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = CabinetBuilder::new();
    {
        let folder = builder.add_folder(CompressionType::MsZip);
        for (name, _) in files {
            folder.add_file((*name).to_string());
        }
    }
    let mut writer = builder.build(Cursor::new(Vec::<u8>::new())).unwrap();
    while let Some(mut fw) = writer.next_file().unwrap() {
        let name = fw.file_name().to_string();
        let (_, data) = files
            .iter()
            .find(|(n, _)| *n == name)
            .expect("file referenced in builder must be in payload list");
        fw.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// Build a minimal valid MSI at `path` with the given table contents.
///
/// `directories`, `components`, `media`, `files` populate the
/// respective MSI tables. `embedded_streams` is a list of
/// (stream_name, cab_bytes) — the stream_name does NOT include the
/// leading `#`. For tests that need a *missing* stream, just omit it.
fn build_msi(
    path: &Path,
    directories: &[(&str, Option<&str>, &str)],
    components: &[(&str, &str)],
    media: &[(i32, &str)],
    files: &[(&str, &str, &str, i32)],
    embedded_streams: &[(&str, &[u8])],
) -> Result<()> {
    // Package::create needs Read + Write + Seek; File::create gives
    // write-only on Windows, which the underlying CFB writer rejects.
    let f = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let mut pkg = msi::Package::create(PackageType::Installer, f)?;

    pkg.create_table(
        "Directory",
        vec![
            Column::build("Directory").primary_key().id_string(72),
            Column::build("Directory_Parent").nullable().id_string(72),
            Column::build("DefaultDir").text_string(255),
        ],
    )?;
    for (key, parent, default_dir) in directories {
        pkg.insert_rows(Insert::into("Directory").row(vec![
            Value::Str((*key).to_string()),
            match parent {
                Some(p) => Value::Str((*p).to_string()),
                None => Value::Null,
            },
            Value::Str((*default_dir).to_string()),
        ]))?;
    }

    pkg.create_table(
        "Component",
        vec![
            Column::build("Component").primary_key().id_string(72),
            Column::build("Directory_").id_string(72),
        ],
    )?;
    for (comp_key, dir_key) in components {
        pkg.insert_rows(Insert::into("Component").row(vec![
            Value::Str((*comp_key).to_string()),
            Value::Str((*dir_key).to_string()),
        ]))?;
    }

    pkg.create_table(
        "Media",
        vec![
            Column::build("DiskId").primary_key().int16(),
            Column::build("LastSequence").int16(),
            Column::build("Cabinet").nullable().text_string(255),
        ],
    )?;
    for (idx, (last_seq, cabinet)) in media.iter().enumerate() {
        pkg.insert_rows(Insert::into("Media").row(vec![
            Value::Int((idx + 1) as i32),
            Value::Int(*last_seq),
            Value::Str((*cabinet).to_string()),
        ]))?;
    }

    pkg.create_table(
        "File",
        vec![
            Column::build("File").primary_key().id_string(72),
            Column::build("Component_").id_string(72),
            Column::build("FileName").text_string(255),
            Column::build("Sequence").int16(),
            Column::build("Attributes").nullable().int16(),
        ],
    )?;
    for (file_key, comp_key, filename, sequence) in files {
        pkg.insert_rows(Insert::into("File").row(vec![
            Value::Str((*file_key).to_string()),
            Value::Str((*comp_key).to_string()),
            Value::Str((*filename).to_string()),
            Value::Int(*sequence),
            Value::Null,
        ]))?;
    }

    for (stream_name, bytes) in embedded_streams {
        let mut w = pkg.write_stream(stream_name)?;
        w.write_all(bytes)?;
    }

    pkg.flush()?;
    Ok(())
}

/// Read a file as a UTF-8 string. Convenience for asserting payload
/// contents in tests where payloads are ASCII.
fn read_text(path: &Path) -> String {
    String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

fn build_paths(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let msi = tmp.path().join("test.msi");
    let dest = tmp.path().join("out");
    (msi, dest)
}

// ============================================================================
// Tests
// ============================================================================

#[test]
fn extracts_single_file_into_targetdir() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    build_msi(
        &msi,
        &[("TARGETDIR", None, "SourceDir")],
        &[("c1", "TARGETDIR")],
        &[(1, "#Cab1")],
        &[("f1", "c1", "hello.txt", 1)],
        &[("Cab1", &build_cab(&[("f1", b"hello world")]))],
    )
    .unwrap();

    extract_msi(&msi, &dest).unwrap();
    assert_eq!(read_text(&dest.join("hello.txt")), "hello world");
}

#[test]
fn resolves_short_pipe_long_filename() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    build_msi(
        &msi,
        &[("TARGETDIR", None, ".")],
        &[("c1", "TARGETDIR")],
        &[(1, "#Cab1")],
        &[("f1", "c1", "FOO~1.TXT|LongName.txt", 1)],
        &[("Cab1", &build_cab(&[("f1", b"X")]))],
    )
    .unwrap();

    extract_msi(&msi, &dest).unwrap();
    assert!(
        dest.join("LongName.txt").is_file(),
        "expected long-name file; dest contents: {:?}",
        std::fs::read_dir(&dest).unwrap().collect::<Vec<_>>()
    );
    assert!(
        !dest.join("FOO~1.TXT").exists(),
        "short-name file should not have been created"
    );
}

#[test]
fn resolves_nested_directories() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    build_msi(
        &msi,
        &[
            ("TARGETDIR", None, "."),
            ("a", Some("TARGETDIR"), "A1|alpha"),
            ("b", Some("a"), "B2|beta"),
            ("c", Some("b"), "C3|gamma"),
        ],
        &[("c1", "c")],
        &[(1, "#Cab1")],
        &[("leaf", "c1", "leaf.txt", 1)],
        &[("Cab1", &build_cab(&[("leaf", b"deep")]))],
    )
    .unwrap();

    extract_msi(&msi, &dest).unwrap();
    let expected = dest.join("alpha").join("beta").join("gamma").join("leaf.txt");
    assert!(expected.is_file(), "expected {} to exist", expected.display());
    assert_eq!(read_text(&expected), "deep");
}

#[test]
fn splits_files_across_two_embedded_cabs() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    // Media table: sequences 1 → Cab1, sequences 2..=3 → Cab2.
    build_msi(
        &msi,
        &[("TARGETDIR", None, ".")],
        &[("c1", "TARGETDIR")],
        &[(1, "#Cab1"), (3, "#Cab2")],
        &[
            ("a", "c1", "a.txt", 1),
            ("b", "c1", "b.txt", 2),
            ("c", "c1", "c.txt", 3),
        ],
        &[
            ("Cab1", &build_cab(&[("a", b"AAA")])),
            ("Cab2", &build_cab(&[("b", b"BBB"), ("c", b"CCC")])),
        ],
    )
    .unwrap();

    extract_msi(&msi, &dest).unwrap();
    assert_eq!(read_text(&dest.join("a.txt")), "AAA");
    assert_eq!(read_text(&dest.join("b.txt")), "BBB");
    assert_eq!(read_text(&dest.join("c.txt")), "CCC");
}

#[test]
fn external_cab_resolved_as_sibling() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    build_msi(
        &msi,
        &[("TARGETDIR", None, ".")],
        &[("c1", "TARGETDIR")],
        &[(1, "external.cab")],
        &[("f1", "c1", "ext.txt", 1)],
        &[], // no embedded streams; CAB is on disk
    )
    .unwrap();

    std::fs::write(
        msi.parent().unwrap().join("external.cab"),
        build_cab(&[("f1", b"sibling")]),
    )
    .unwrap();

    extract_msi(&msi, &dest).unwrap();
    assert_eq!(read_text(&dest.join("ext.txt")), "sibling");
}

#[test]
fn bails_on_missing_cab_stream() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    // Media references #Cab1 but we don't write the stream → bail.
    build_msi(
        &msi,
        &[("TARGETDIR", None, ".")],
        &[("c1", "TARGETDIR")],
        &[(1, "#Cab1")],
        &[("f1", "c1", "missing.txt", 1)],
        &[], // <-- absent
    )
    .unwrap();

    let err = extract_msi(&msi, &dest)
        .expect_err("missing embedded CAB stream must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("Cab1") || msg.to_lowercase().contains("stream"),
        "error should mention the missing stream; got: {msg}"
    );
}

#[test]
fn extract_msis_in_parallel_all_succeed() {
    // Build a small batch of distinct MSIs and run them through the
    // parallel batch helper. Each writes a different file into the
    // shared dest. Asserts: all files land, none lost, no per-MSI
    // collision panic.
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("out");

    let mut msi_paths: Vec<std::path::PathBuf> = Vec::new();
    for i in 0..4 {
        let msi_path = tmp.path().join(format!("pkg-{i}.msi"));
        let payload_name = format!("file-{i}.txt");
        let payload_bytes = format!("contents of pkg {i}").into_bytes();
        build_msi(
            &msi_path,
            &[("TARGETDIR", None, ".")],
            &[("c1", "TARGETDIR")],
            &[(1, "#Cab1")],
            &[("f1", "c1", &payload_name, 1)],
            &[("Cab1", &build_cab(&[("f1", &payload_bytes)]))],
        )
        .unwrap();
        msi_paths.push(msi_path);
    }

    super::extract_msis_in_parallel(&msi_paths, &dest).unwrap();

    for i in 0..4 {
        let path = dest.join(format!("file-{i}.txt"));
        assert!(path.is_file(), "missing: {}", path.display());
        assert_eq!(read_text(&path), format!("contents of pkg {i}"));
    }
}

#[test]
fn extract_msis_in_parallel_propagates_first_error() {
    // One bad MSI in a batch must cause the whole call to error. We
    // don't care which worker hit the bad one first, only that the
    // function returns Err and mentions the failing MSI.
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("out");

    let good_msi = tmp.path().join("good.msi");
    build_msi(
        &good_msi,
        &[("TARGETDIR", None, ".")],
        &[("c1", "TARGETDIR")],
        &[(1, "#Cab1")],
        &[("f1", "c1", "good.txt", 1)],
        &[("Cab1", &build_cab(&[("f1", b"good")]))],
    )
    .unwrap();

    let bad_msi = tmp.path().join("bad.msi");
    // Write garbage so `msi::open` fails (Invalid CFB).
    std::fs::write(&bad_msi, b"not an msi").unwrap();

    let err = super::extract_msis_in_parallel(&[good_msi, bad_msi.clone()], &dest)
        .expect_err("bad MSI must surface as error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("bad.msi"),
        "error should mention the failing MSI; got: {msg}"
    );
}

/// Build an MSI with one file under one directory and expect extraction to
/// fail without writing anything outside `dest`.
fn assert_unsafe_rejected(default_dir: &str, file_name: &str) {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);
    build_msi(
        &msi,
        &[("TARGETDIR", None, "SourceDir"), ("d", Some("TARGETDIR"), default_dir)],
        &[("c1", "d")],
        &[(1, "#Cab1")],
        &[("f1", "c1", file_name, 1)],
        &[("Cab1", &build_cab(&[("f1", b"pwned")]))],
    )
    .unwrap();

    let err = extract_msi(&msi, &dest).unwrap_err();
    assert!(
        format!("{err:#}").contains("unsafe path segment"),
        "DefaultDir {default_dir:?} FileName {file_name:?}: unexpected error {err:#}"
    );
    let escaped: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != "test.msi" && n != "out")
        .collect();
    assert!(escaped.is_empty(), "files written outside dest: {escaped:?}");
}

#[test]
fn rejects_parent_dir_in_default_dir() {
    assert_unsafe_rejected("..", "x.txt");
    assert_unsafe_rejected("UP|..", "x.txt");
}

#[test]
fn rejects_separators_in_default_dir() {
    // (A `:` in DefaultDir separates the target and source names, so a drive
    // prefix can't get through it.)
    assert_unsafe_rejected("a/../..", "x.txt");
    assert_unsafe_rejected("a\\..\\..", "x.txt");
    assert_unsafe_rejected("A|/abs", "x.txt");
}

#[test]
fn rejects_unsafe_file_names() {
    assert_unsafe_rejected("sub", "..");
    assert_unsafe_rejected("sub", "X|../x.txt");
    assert_unsafe_rejected("sub", "X|..\\x.txt");
    assert_unsafe_rejected("sub", "/tmp/x.txt");
    assert_unsafe_rejected("sub", "X|C:x.txt");
}

#[test]
fn rejects_dot_and_space_only_names() {
    // Windows trims trailing dots and spaces, so these can become `..`.
    assert_unsafe_rejected(".. ", "x.txt");
    assert_unsafe_rejected("sub", "X|.. .");
    assert_unsafe_rejected("sub", "...");
}

#[test]
fn rejects_external_cab_outside_the_msi_dir() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);
    build_msi(
        &msi,
        &[("TARGETDIR", None, "SourceDir")],
        &[("c1", "TARGETDIR")],
        &[(1, "../other.cab")],
        &[("f1", "c1", "x.txt", 1)],
        &[],
    )
    .unwrap();
    let err = extract_msi(&msi, &dest).unwrap_err();
    assert!(format!("{err:#}").contains("unsafe path segment"), "got: {err:#}");
}

#[test]
fn directory_parent_cycle_is_an_error_not_a_stack_overflow() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);
    build_msi(
        &msi,
        &[("a", Some("b"), "a"), ("b", Some("a"), "b")],
        &[("c1", "a")],
        &[(1, "#Cab1")],
        &[("f1", "c1", "x.txt", 1)],
        &[("Cab1", &build_cab(&[("f1", b"X")]))],
    )
    .unwrap();
    let err = extract_msi(&msi, &dest).unwrap_err();
    assert!(format!("{err:#}").contains("parent cycle"), "got: {err:#}");
}

/// Builds a CAB (MSZIP) of `files`, applies `edit` to each CFFILE entry
/// (index, 16 fixed bytes + name), writes it next to an MSI whose File rows
/// are (key, file name), and extracts. Lets tests make CABs the builder won't.
fn extract_edited(
    tmp: &TempDir,
    files: &[(&str, &[u8])],
    mut edit: impl FnMut(usize, &mut [u8]),
    rows: &[(&str, &str)],
) -> Result<PathBuf> {
    let mut cab = build_cab(files);
    let mut at = u32::from_le_bytes(cab[16..20].try_into().unwrap()) as usize;
    for i in 0..files.len() {
        let len = cab[at + 16..].iter().position(|&b| b == 0).unwrap();
        edit(i, &mut cab[at..at + 16 + len]);
        at += 16 + len + 1;
    }
    let (msi, dest) = build_paths(tmp);
    let rows: Vec<(&str, &str, &str, i32)> =
        rows.iter().enumerate().map(|(i, (k, n))| (*k, "c1", *n, i as i32 + 1)).collect();
    build_msi(&msi, &[("TARGETDIR", None, ".")], &[("c1", "TARGETDIR")], &[(1000, "x.cab")], &rows, &[])?;
    std::fs::write(tmp.path().join("x.cab"), cab)?;
    extract_msi(&msi, &dest)?;
    Ok(dest)
}

/// Each folder is streamed once (#286), so files must come out right in any
/// job order, skipping the ones not asked for, across folders.
#[test]
fn extracts_a_subset_of_a_multi_folder_cab_in_any_order() {
    let payload = |i: usize| format!("file {i} ").repeat(i * 37 % 200).into_bytes(); // 0 is empty
    let mut builder = CabinetBuilder::new();
    for (folder, ctype) in [CompressionType::MsZip, CompressionType::None].into_iter().enumerate() {
        let f = builder.add_folder(ctype);
        for i in 0..40 {
            f.add_file(format!("k{}", folder * 40 + i));
        }
    }
    let mut writer = builder.build(Cursor::new(Vec::<u8>::new())).unwrap();
    while let Some(mut fw) = writer.next_file().unwrap() {
        let i: usize = fw.file_name()[1..].parse().unwrap();
        fw.write_all(&payload(i)).unwrap();
    }
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("x.cab"), writer.finish().unwrap().into_inner()).unwrap();

    let wanted: Vec<usize> = (0..80).rev().filter(|i| i % 3 == 0).collect();
    let keys: Vec<(String, String)> = wanted.iter().map(|i| (format!("k{i}"), format!("f{i}"))).collect();
    let rows: Vec<(&str, &str, &str, i32)> =
        keys.iter().enumerate().map(|(s, (k, n))| (k.as_str(), "c1", n.as_str(), s as i32 + 1)).collect();
    let (msi, dest) = build_paths(&tmp);
    build_msi(&msi, &[("TARGETDIR", None, ".")], &[("c1", "TARGETDIR")], &[(1000, "x.cab")], &rows, &[]).unwrap();
    extract_msi(&msi, &dest).unwrap();
    for i in 0..80 {
        let got = std::fs::read(dest.join(format!("f{i}"))).ok();
        assert_eq!(got, wanted.contains(&i).then(|| payload(i)), "f{i}");
    }
}

/// Two keys for one range (17 pairs in the SDK) both get the bytes, also when
/// they name one file on disk.
#[test]
fn two_keys_for_one_range_both_get_the_bytes() {
    let files: &[(&str, &[u8])] = &[("a", b"hello world"), ("b", b"other bytes!")];
    let mut range = [0u8; 8];
    let mut same_range = |i: usize, e: &mut [u8]| match i {
        0 => range.copy_from_slice(&e[0..8]),
        _ => e[0..8].copy_from_slice(&range),
    };
    let tmp = TempDir::new().unwrap();
    let dest = extract_edited(&tmp, files, &mut same_range, &[("a", "a.txt"), ("b", "b.txt")]).unwrap();
    assert_eq!(read_text(&dest.join("a.txt")), "hello world");
    assert_eq!(read_text(&dest.join("b.txt")), "hello world");
    let tmp = TempDir::new().unwrap();
    let dest = extract_edited(&tmp, files, &mut same_range, &[("a", "x.txt"), ("b", "x.txt")]).unwrap();
    assert_eq!(read_text(&dest.join("x.txt")), "hello world");
}

/// Two entries with one name: the first wins, as with `Cabinet::read_file`.
#[test]
fn duplicate_cab_names_take_the_first_entry() {
    let tmp = TempDir::new().unwrap();
    let rename = |i: usize, e: &mut [u8]| if i == 1 { e[16] = b'a' };
    let dest = extract_edited(&tmp, &[("a", b"first"), ("b", b"second")], rename, &[("a", "a.txt")]).unwrap();
    assert_eq!(read_text(&dest.join("a.txt")), "first");
}

/// A file continued from another cabinet fails the CAB, as before.
#[test]
fn rejects_multi_cabinet_sets() {
    let tmp = TempDir::new().unwrap();
    let continued = |i: usize, e: &mut [u8]| if i == 1 { e[8..10].copy_from_slice(&0xFFFDu16.to_le_bytes()) };
    let err = extract_edited(&tmp, &[("a", b"A"), ("b", b"B")], continued, &[("a", "a.txt")]).unwrap_err();
    assert!(format!("{err:#}").contains("multi-cabinet set"), "{err:#}");
}
