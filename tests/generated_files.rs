//! Generated files (#213), end to end, two modes in one build.
//!
//! An `anubis_cmd` writes `gen.c` and `gen.h`, a `deploy_into_workspace`
//! copies them into `gen/` (never there before the build), one library
//! compiles `gen.c` and another reads `gen.h` through the first's include
//! dir. Every compile must wait for the deploy, and a change to the
//! generator's input must reach the binary.
//!
//! A command and its deploy are keyed on what they resolve to
//! (`Rule::shared_key`): the same command in both modes is one job and one
//! deploy; a command that differs per mode runs once per mode, and must
//! deploy to a directory that differs too.
//!
//! The workspace is written to `tests/tmp_generated_files_test` and removed
//! afterwards. Needs the toolchains (`anubis install-toolchains`), so it is
//! `#[ignore]`d and run by name:
//!
//!     cargo test --release --test generated_files -- --ignored

use std::path::{Path, PathBuf};
use std::process::Command;

const DIR: &str = "tests/tmp_generated_files_test";
const NAME: &str = "tmp_generated_files_test";

const ANUBIS: &str = r##"cc_binary(
    name = "gen_tool",
    lang = "c",
    srcs = [ RelPath("gen_tool.c") ],
)

anubis_cmd(
    name = "generate",
    tool = Target(":gen_tool"),
    args = [
        [ "header", RelPath("value.txt"), "$OUTDIR/gen.h" ],
        [ "source", RelPath("value.txt"), "$OUTDIR/gen.c" ],
    ],
)

deploy_into_workspace(
    name = "deploy",
    dep = Target(":generate"),
    dst = RelPath("gen"),
    method = "copy",
)

cc_static_library(
    name = "gen",
    lang = "c",
    srcs = [ RelPath("gen/gen.c") ],
    public_include_dirs = [ RelPath("gen") ],
    public_headers = [ RelPath("gen/gen.h") ],
    deps = Targets([ ":deploy" ]),
)

cc_static_library(
    name = "user",
    lang = "c",
    srcs = [ RelPath("user.c") ],
    deps = Targets([ ":gen" ]),
)

cc_binary(
    name = "main",
    lang = "c",
    srcs = [ RelPath("main.c") ],
    deps = Targets([ ":user" ]),
)

# A command that differs per mode, deployed to a directory per mode.
anubis_cmd(
    name = "generate_per_mode",
    tool = Target(":gen_tool"),
    args = [
        [
            "header",
            select((build_type) => { (debug) = RelPath("value_debug.txt"), (release) = RelPath("value_release.txt") }),
            "$OUTDIR/pm.h",
        ],
    ],
)

deploy_into_workspace(
    name = "deploy_per_mode",
    dep = Target(":generate_per_mode"),
    dst = select((build_type) => { (debug) = RelPath("gen_debug"), (release) = RelPath("gen_release") }),
    method = "copy",
)

cc_binary(
    name = "per_mode",
    lang = "c",
    srcs = [ RelPath("per_mode.c") ],
    include_dirs = [ select((build_type) => { (debug) = RelPath("gen_debug"), (release) = RelPath("gen_release") }) ],
    headers = [ select((build_type) => { (debug) = RelPath("gen_debug/pm.h"), (release) = RelPath("gen_release/pm.h") }) ],
    deps = Targets([ ":deploy_per_mode" ]),
)

# The same per-mode command, but both modes deploy to one directory.
deploy_into_workspace(
    name = "deploy_conflict",
    dep = Target(":generate_per_mode"),
    dst = RelPath("gen_conflict"),
    method = "copy",
)

# Names a generated file the deploy never writes.
cc_binary(
    name = "missing",
    lang = "c",
    srcs = [ RelPath("main.c"), RelPath("gen/not_generated.c") ],
    deps = Targets([ ":user" ]),
)
"##;

const GEN_TOOL_C: &str = r##"// gen_tool <header|source> <value file> <output>
#include <stdio.h>
#include <string.h>
int main(int argc, char** argv) {
    int value = 0;
    FILE* in = argc == 4 ? fopen(argv[2], "r") : NULL;
    if (!in || fscanf(in, "%d", &value) != 1) { fprintf(stderr, "gen_tool: bad input\n"); return 1; }
    fclose(in);
    FILE* out = fopen(argv[3], "w");
    if (!out) { fprintf(stderr, "gen_tool: can't write %s\n", argv[3]); return 1; }
    if (strcmp(argv[1], "header") == 0)
        fprintf(out, "#pragma once\n#define GEN_VALUE %d\nint gen_value(void);\n", value);
    else
        fprintf(out, "#include \"gen.h\"\nint gen_value(void) { return GEN_VALUE; }\n");
    fclose(out);
    return 0;
}
"##;

const USER_C: &str = r##"#include "gen.h"
int user_value(void) { return GEN_VALUE * 1000 + gen_value(); }
"##;

const PER_MODE_C: &str = r##"#include <stdio.h>
#include "pm.h"
int main(void) { printf("VALUE=%d\n", GEN_VALUE); return 0; }
"##;

const MAIN_C: &str = r##"#include <stdio.h>
int user_value(void);
int main(void) { printf("VALUE=%d\n", user_value()); return 0; }
"##;

/// Removes the test workspace, also when the test fails.
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `anubis build`; returns whether it succeeded and its output.
fn build(root: &Path, args: &[&str]) -> (bool, String) {
    // A file written just before a compile starts counts as changed during it (#191).
    std::thread::sleep(std::time::Duration::from_millis(100));
    let output = Command::new(env!("CARGO_BIN_EXE_anubis"))
        .current_dir(root)
        .args(["build", "--no-tui"])
        .args(args)
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

/// The entries of `dir` named `<prefix>_<8 hex digits>`: how `Anubis::out_dir`
/// names mode and target directories.
fn hashed(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            name.strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('_'))
                .is_some_and(|hash| hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .collect()
}

/// `.anubis-out/<mode>_<hash>/<NAME>_<target>_<hash>`.
fn target_dir(root: &Path, mode: &str, target: &str) -> PathBuf {
    let modes = hashed(&root.join(".anubis-out"), mode);
    assert_eq!(modes.len(), 1, "output directories of mode {mode}: {modes:?}");
    let targets = hashed(&modes[0], &format!("{NAME}_{target}"));
    assert_eq!(targets.len(), 1, "output directories of {target} in {mode}: {targets:?}");
    targets[0].clone()
}

/// Runs a built executable from the workspace root and returns what it printed.
fn run_exe(root: &Path, mode: &str, target: &str) -> String {
    let exe = target_dir(root, mode, target).join("bin").join(format!("{target}{}", std::env::consts::EXE_SUFFIX));
    let output = Command::new(&exe).current_dir(root).output().unwrap_or_else(|e| panic!("{}: {e}", exe.display()));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// How many output directories (one per distinct resolved command) a command has.
fn command_dirs(root: &Path, target: &str) -> usize {
    hashed(&root.join(".anubis-out/modeless"), &format!("{NAME}_{target}"))
        .iter()
        .map(|dir| std::fs::read_dir(dir.join("cmd")).map(|d| d.count()).unwrap_or(0))
        .sum()
}

#[test]
#[ignore = "needs the toolchains; run with --ignored"]
fn generated_files_are_built_before_their_users_once_per_distinct_command() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (dev, release) = match std::env::consts::OS {
        "windows" => ("win_dev", "win_release"),
        "macos" => ("macos_dev", "macos_release"),
        _ => ("linux_dev", "linux_release"),
    };
    let (mode_dev, mode_release) = (format!("//mode:{dev}"), format!("//mode:{release}"));
    let both = ["-m", mode_dev.as_str(), "-m", mode_release.as_str()];
    let label = |target: &str| format!("//{DIR}:{target}");

    let ws = root.join(DIR);
    let _ = std::fs::remove_dir_all(&ws);
    let _cleanup = Cleanup(ws.clone());
    std::fs::create_dir_all(&ws).unwrap();
    for (name, text) in [
        ("ANUBIS", ANUBIS),
        ("gen_tool.c", GEN_TOOL_C),
        ("user.c", USER_C),
        ("per_mode.c", PER_MODE_C),
        ("main.c", MAIN_C),
        ("value_debug.txt", "1\n"),
        ("value_release.txt", "2\n"),
    ] {
        std::fs::write(ws.join(name), text).unwrap();
    }

    // Builds :main in both modes and returns what the dev binary prints:
    // header value * 1000 + source value.
    let main_value = |extra: &[&str]| {
        let main = label("main");
        let mut args = both.to_vec();
        args.extend(["-t", main.as_str()]);
        args.extend(extra);
        let (ok, log) = build(root, &args);
        assert!(ok, "build failed:\n{log}");
        run_exe(root, dev, "main")
    };

    // A fresh build: gen/ doesn't exist until the deploy runs.
    std::fs::write(ws.join("value.txt"), "42\n").unwrap();
    assert_eq!(main_value(&["--no-cache", "--workers", "16"]), "VALUE=42042", "fresh build");

    // A change to the generator's input reaches the binary.
    std::fs::write(ws.join("value.txt"), "7\n").unwrap();
    assert_eq!(main_value(&[]), "VALUE=7007", "generator input changed");

    // Fresh again, this time with a warm cache.
    std::fs::remove_dir_all(ws.join("gen")).unwrap();
    assert_eq!(main_value(&["--workers", "16"]), "VALUE=7007", "gen/ deleted");

    // The same command in both modes ran as one job.
    assert_eq!(command_dirs(root, "generate"), 1, "command output directories");

    // A command that resolves differently per mode: one run and one deploy each.
    let per_mode = label("per_mode");
    let mut args = both.to_vec();
    args.extend(["-t", per_mode.as_str()]);
    let (ok, log) = build(root, &args);
    assert!(ok, "per-mode command: build failed:\n{log}");
    assert_eq!(run_exe(root, dev, "per_mode"), "VALUE=1");
    assert_eq!(run_exe(root, release, "per_mode"), "VALUE=2");
    assert_eq!(command_dirs(root, "generate_per_mode"), 2, "per-mode command output directories");

    // The same per-mode command deployed to one directory is an error.
    let conflict = label("deploy_conflict");
    let mut args = both.to_vec();
    args.extend(["-t", conflict.as_str()]);
    let (ok, log) = build(root, &args);
    assert!(!ok, "deploy conflict: build succeeded:\n{log}");
    assert!(log.contains("deploys different commands"), "deploy conflict: no clear message:\n{log}");

    // A source the deploy never writes fails with a message that names it.
    let missing = label("missing");
    let (ok, log) = build(root, &["-m", mode_dev.as_str(), "-t", missing.as_str()]);
    assert!(!ok, "missing source: build succeeded:\n{log}");
    assert!(
        log.contains("source files do not exist") && log.contains("gen/not_generated.c"),
        "missing source: no clear message:\n{log}"
    );
}
