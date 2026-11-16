//! Anubis and Natron decide nothing from environment variables: a build
//! must not depend on the shell it was started from. This test fails when
//! code starts reading, setting or passing one. If a new use is really
//! needed, add it to `ALLOWED` with the reason, so it shows up in review.

use std::path::Path;

/// (file, text on the line, why it is allowed)
const ALLOWED: &[(&str, &str, &str)] = &[
    ("src/rules/rule_utils.rs", ".env_clear()", "every build tool starts with an empty environment"),
    ("natron/src/cli/mod.rs", "try_from_default_env", "RUST_LOG picks the standalone natron CLI's log filter"),
];

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            // Test code may set variables for the programs it starts.
            if name != "tests" {
                rust_files(&path, out);
            }
        } else if name.ends_with(".rs") && !name.ends_with("_tests.rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_new_environment_variable_use() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Split so this file's own text doesn't matter if it is ever scanned.
    let needles: Vec<String> = [
        ["env::", "var"],
        ["env::", "set_var"],
        ["env::", "remove_var"],
        [".en", "v("],
        [".en", "vs("],
        [".env_", "remove("],
        [".env_", "clear("],
        ["option_", "env!"],
        ["from_default", "_env"],
    ]
    .iter()
    .map(|p| p.concat())
    .collect();

    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("natron").join("src"), &mut files);
    assert!(files.len() > 20, "found only {} source files", files.len());

    let mut found = Vec::new();
    for file in files {
        let rel = file.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        for (i, line) in std::fs::read_to_string(&file).unwrap().lines().enumerate() {
            if line.trim_start().starts_with("//") || !needles.iter().any(|n| line.contains(n.as_str())) {
                continue;
            }
            if !ALLOWED.iter().any(|(f, text, _)| *f == rel && line.contains(text)) {
                found.push(format!("{rel}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "environment variable use outside the allow-list in src/tests/env_guard_tests.rs:\n  {}",
        found.join("\n  ")
    );
}
