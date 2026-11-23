//! Debug symbols (#212), end to end.
//!
//! Builds always make debug symbols. A `py_wheel` ships them unless it sets
//! `strip_symbols`: then Windows wheels carry no `.pdb` and Linux binaries
//! lose their debug sections but keep their build id. A shared library's
//! `.pdb` is staged next to the executable that loads it.
//!
//! Builds samples in this checkout (`.anubis-out`), like `noop_build`. Needs
//! the toolchains (`anubis install-toolchains`), so the tests are
//! `#[ignore]`d and run by name:
//!
//!     cargo test --release --test debug_symbols -- --ignored

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn anubis(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_anubis")).current_dir(root()).args(args).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "anubis {args:?} failed:\n{text}");
    text
}

/// `anubis build --outputs-json` for `modes` x `targets`.
fn build(modes: &[&str], targets: &[&str], json_name: &str) -> (String, serde_json::Value) {
    let json = std::env::temp_dir().join(format!("anubis_{json_name}_{}.json", std::process::id()));
    let mut args = vec!["build", "--no-tui", "--outputs-json", json.to_str().unwrap()];
    for m in modes {
        args.extend(["-m", m]);
    }
    args.push("-t");
    args.extend(targets);
    let log = anubis(&args);
    let outputs = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    let _ = std::fs::remove_file(&json);
    (log, outputs)
}

/// The one output file of `target` in `mode`.
fn output_file(outputs: &serde_json::Value, mode: &str, target: &str) -> PathBuf {
    let entry = outputs
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["mode"] == mode && e["target"] == target)
        .unwrap_or_else(|| panic!("no entry for {mode} {target} in {outputs}"));
    PathBuf::from(entry["files"][0].as_str().unwrap())
}

/// A wheel's entries under `_native/`: file name -> bytes.
fn native_entries(wheel: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(wheel).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).unwrap();
        if let Some((_, name)) = entry.name().to_owned().split_once("/_native/") {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            out.insert(name.to_owned(), bytes);
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The 20-byte GNU build id (`--build-id=sha1`) from the ELF note: name
/// size 4, descriptor size 20, type 3 (NT_GNU_BUILD_ID), "GNU\0".
fn build_id(elf: &[u8]) -> Vec<u8> {
    let header = [4, 0, 0, 0, 20, 0, 0, 0, 3, 0, 0, 0, b'G', b'N', b'U', 0];
    let at = elf.windows(header.len()).position(|w| w == header).expect("no build id note");
    elf[at + header.len()..at + header.len() + 20].to_vec()
}

// One test: two builds at once in one workspace would race (no lock).
#[test]
#[ignore = "needs the toolchains; run with --ignored"]
fn debug_symbols() {
    a_shared_librarys_pdb_is_staged_next_to_the_executable();
    py_wheel_ships_symbols_unless_strip_symbols();
}

fn py_wheel_ships_symbols_unless_strip_symbols() {
    // Python rules don't run on a macOS host yet.
    if std::env::consts::OS == "macos" {
        return;
    }
    let modes = ["//mode:linux_release_static", "//mode:win_release"];
    let targets = ["//samples/python/hello_uv:wheel", "//samples/python/hello_uv:wheel_stripped"];
    let (_, outputs) = build(&modes, &targets, "wheels");

    // Linux: the same extension, with and without its debug sections.
    let with = native_entries(&output_file(&outputs, modes[0], targets[0]));
    let without = native_entries(&output_file(&outputs, modes[0], targets[1]));
    assert_eq!(with.keys().collect::<Vec<_>>(), without.keys().collect::<Vec<_>>());
    let (name, full) = with.iter().find(|(n, _)| n.ends_with(".so")).expect("no extension in the Linux wheel");
    let stripped = &without[name];
    assert!(full.starts_with(b"\x7fELF") && stripped.starts_with(b"\x7fELF"));
    assert!(contains(full, b".debug_info"), "{name} in the default wheel has no debug info");
    assert!(!contains(stripped, b".debug_info"), "{name} in the stripped wheel still has debug info");
    assert!(contains(stripped, b".symtab"), "--strip-debug keeps the symbol table");
    assert!(stripped.len() < full.len());
    assert_eq!(build_id(full), build_id(stripped), "stripping must keep the build id");

    // Windows: the .pdb ships by default and is left out when stripping;
    // every other file is the same.
    let with = native_entries(&output_file(&outputs, modes[1], targets[0]));
    let mut without = native_entries(&output_file(&outputs, modes[1], targets[1]));
    let pdbs: Vec<&String> = with.keys().filter(|n| n.ends_with(".pdb")).collect();
    assert_eq!(pdbs.len(), 1, "expected the extension's .pdb in the default Windows wheel: {:?}", with.keys());
    assert!(!without.keys().any(|n| n.ends_with(".pdb")), "{:?}", without.keys());
    without.insert(pdbs[0].clone(), with[pdbs[0]].clone());
    assert!(with == without, "the stripped Windows wheel differs in more than the .pdb");

    // Nothing to do the second time: the strip is a cached action.
    let (log, _) = build(&modes, &targets, "wheels");
    assert!(log.contains("ran 0 tool(s) and 0 in-process action(s)"), "{log}");
}

fn a_shared_librarys_pdb_is_staged_next_to_the_executable() {
    let (_, outputs) = build(&["//mode:win_dev"], &["//samples/basic/shared_cpp:main"], "shared");
    let entry = &outputs[0];
    let exe = PathBuf::from(entry["files"][0].as_str().unwrap());
    let bin = exe.parent().unwrap();
    let debug: Vec<&str> = entry["debug_files"].as_array().unwrap().iter().map(|f| f["dest"].as_str().unwrap()).collect();
    assert_eq!(debug, ["main.pdb", "calc.pdb", "greet.pdb"]);
    for name in debug {
        assert!(bin.join(name).is_file(), "{name} is not in {}", bin.display());
    }
    // Run-time files and debug files are separate lists.
    let files: Vec<&str> = entry["files"].as_array().unwrap().iter().map(|f| f.as_str().unwrap()).collect();
    assert!(!files.iter().any(|f| f.ends_with(".pdb")), "{files:?}");
}
