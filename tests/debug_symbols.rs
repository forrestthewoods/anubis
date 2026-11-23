//! Debug symbols (#212), end to end: builds always make them, a `py_wheel`
//! ships them unless it sets `strip_symbols`, and a shared library's `.pdb`
//! is staged next to the executable that loads it.
//!
//! Builds samples in this checkout, like `noop_build`. Needs the toolchains:
//!
//!     cargo test --release --test debug_symbols -- --ignored

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::Command;

/// `anubis build --outputs-json` for `modes` x `targets`: the log and the JSON.
fn build(modes: &[&str], targets: &[&str]) -> (String, serde_json::Value) {
    let json = std::env::temp_dir().join(format!("anubis_debug_symbols_{}.json", std::process::id()));
    let mut args = vec!["build", "--no-tui", "--outputs-json", json.to_str().unwrap()];
    for m in modes {
        args.extend(["-m", m]);
    }
    args.push("-t");
    args.extend(targets);
    let output =
        Command::new(env!("CARGO_BIN_EXE_anubis")).current_dir(env!("CARGO_MANIFEST_DIR")).args(&args).output().unwrap();
    let log = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "anubis {args:?} failed:\n{log}");
    let outputs = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    let _ = std::fs::remove_file(&json);
    (log, outputs)
}

/// The `_native/` entries (file name -> bytes) of the wheel `target` built in `mode`.
fn native_entries(outputs: &serde_json::Value, mode: &str, target: &str) -> BTreeMap<String, Vec<u8>> {
    let entry = outputs.as_array().unwrap().iter().find(|e| e["mode"] == mode && e["target"] == target).unwrap();
    let wheel = std::fs::File::open(entry["files"][0].as_str().unwrap()).unwrap();
    let mut zip = zip::ZipArchive::new(wheel).unwrap();
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

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The GNU build id note: name size 4, descriptor size 20 (sha1), type 3,
/// "GNU\0", then the id.
fn build_id(elf: &[u8]) -> &[u8] {
    let header = [4, 0, 0, 0, 20, 0, 0, 0, 3, 0, 0, 0, b'G', b'N', b'U', 0];
    let at = find(elf, &header).expect("no build id note") + header.len();
    &elf[at..at + 20]
}

// One test: two builds at once in one workspace would race.
#[test]
#[ignore = "needs the toolchains; run with --ignored"]
fn debug_symbols() {
    // A shared library's .pdb is staged next to the executable, and listed
    // apart from the run-time files.
    let (_, outputs) = build(&["//mode:win_dev"], &["//samples/basic/shared_cpp:main"]);
    let entry = &outputs[0];
    let bin = Path::new(entry["files"][0].as_str().unwrap()).parent().unwrap();
    let debug: Vec<&str> = entry["debug_files"].as_array().unwrap().iter().map(|f| f["dest"].as_str().unwrap()).collect();
    assert_eq!(debug, ["main.pdb", "calc.pdb", "greet.pdb"]);
    assert!(debug.iter().all(|name| bin.join(name).is_file()));
    assert!(!entry["files"].to_string().contains(".pdb"));

    // Python rules don't run on a macOS host yet.
    if std::env::consts::OS == "macos" {
        return;
    }
    let modes = ["//mode:linux_release_static", "//mode:win_release"];
    let targets = ["//samples/python/hello_uv:wheel", "//samples/python/hello_uv:stripped_wheel"];
    let (_, outputs) = build(&modes, &targets);

    // Linux: the stripped extension has no debug sections, and keeps its
    // symbol table and build id.
    let with = native_entries(&outputs, modes[0], targets[0]);
    let without = native_entries(&outputs, modes[0], targets[1]);
    assert!(with.keys().eq(without.keys()));
    let (name, full) = with.iter().find(|(n, _)| n.ends_with(".so")).expect("no extension in the Linux wheel");
    let stripped = &without[name];
    assert!(find(full, b".debug_info").is_some(), "{name} in the default wheel has no debug info");
    assert!(find(stripped, b".debug_info").is_none(), "{name} in the stripped wheel still has debug info");
    assert!(find(stripped, b".symtab").is_some());
    assert_eq!(build_id(full), build_id(stripped));

    // Windows: the stripped wheel differs only by the missing .pdb.
    let with = native_entries(&outputs, modes[1], targets[0]);
    let mut without = native_entries(&outputs, modes[1], targets[1]);
    let pdbs: Vec<&String> = with.keys().filter(|n| n.ends_with(".pdb")).collect();
    assert_eq!(pdbs.len(), 1, "{:?}", with.keys());
    assert!(!without.contains_key(pdbs[0]));
    without.insert(pdbs[0].clone(), with[pdbs[0]].clone());
    assert!(with == without, "the stripped Windows wheel differs in more than the .pdb");

    // The strip is a cached action: nothing to do the second time.
    let (log, _) = build(&modes, &targets);
    assert!(log.contains("ran 0 tool(s) and 0 in-process action(s)"), "{log}");
}
