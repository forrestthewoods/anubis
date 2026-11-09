//! End to end: export the fixtures (and some Anubis samples), build them with
//! a pinned CMake and Ninja, run them, and check they print what Anubis
//! builds print (`fixtures/expected.txt`).
//!
//! Needs CMake and Ninja: by default the ones `anubis_cmake/natron.toml`
//! installs into `anubis_cmake/toolchains/` (or `ANUBIS_CMAKE_CMAKE`,
//! `ANUBIS_CMAKE_NINJA`). Without them the test is skipped, unless
//! `ANUBIS_CMAKE_E2E=1` says it must run. The compiler is CMake's default
//! (set `CC`/`CXX` to choose).
//!
//! `ANUBIS_CMAKE_COMPARE_ANUBIS=1` also builds the fixtures with Anubis
//! (`target/release/anubis`, toolchains installed) and checks those
//! executables print the same thing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const MODES: &[&str] = &[
    "//mode:win_dev",
    "//mode:win_release",
    "//mode:linux_dev",
    "//mode:linux_release",
];
const SAMPLES: &[&str] = &[
    "//samples/basic/trivial_c:main",
    "//samples/basic/trivial_cpp:main",
    "//samples/basic/staticlib_cpp:main",
    "//samples/basic/nested_staticlib_cpp:main",
    "//samples/basic/shared_cpp:main",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// A pinned tool, or None (test skipped) unless the test must run.
fn tool(env: &str, rel: &str) -> Option<PathBuf> {
    let p = std::env::var_os(env)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo().join("anubis_cmake/toolchains").join(exe(rel)));
    if p.is_file() {
        return Some(p);
    }
    if std::env::var("ANUBIS_CMAKE_E2E").is_ok_and(|v| v == "1") {
        panic!(
            "{} not found; install with `natron -c anubis_cmake/natron.toml install` or set {env}",
            p.display()
        );
    }
    eprintln!(
        "skipping: {} not found (install with `natron -c anubis_cmake/natron.toml install`)",
        p.display()
    );
    None
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("running {cmd:?}: {e}"));
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{cmd:?} failed:\n{text}");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn expected() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(repo().join("anubis_cmake/tests/fixtures/expected.txt")).unwrap();
    text.lines()
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect()
}

fn export(out: &str, extra: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_anubis-cmake"));
    c.current_dir(repo())
        .args(["--namespace", "fx", "-o", out, "-m"])
        .args(MODES)
        .arg("-t")
        .arg("//anubis_cmake/tests/fixtures/...")
        .args(SAMPLES)
        .args(extra);
    c
}

/// Configure and build with one CMake, then check every fixture's output.
fn build_and_check(cmake: &Path, ninja: &Path, src: &Path, build: &Path) {
    let _ = std::fs::remove_dir_all(build);
    run(Command::new(cmake)
        .arg("-S")
        .arg(src)
        .arg("-B")
        .arg(build)
        .args([
            "-G",
            "Ninja",
            "-DCMAKE_BUILD_TYPE=Debug",
            "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON",
        ])
        .arg(format!("-DCMAKE_MAKE_PROGRAM={}", ninja.display())));
    run(Command::new(cmake).arg("--build").arg(build));
    for (name, want) in expected() {
        let got = run(&mut Command::new(build.join(exe(&name))));
        assert_eq!(got.trim(), want, "{name} (CMake build, {})", cmake.display());
    }
}

#[test]
fn exported_fixtures_build_and_behave_like_anubis() {
    if cfg!(target_os = "macos") {
        eprintln!("skipping: no macOS mode to export (unexported_platforms_are_refused covers macOS)");
        return;
    }
    let (Some(cmake), Some(ninja)) = (
        tool("ANUBIS_CMAKE_CMAKE", "cmake/bin/cmake"),
        tool("ANUBIS_CMAKE_NINJA", "ninja/ninja"),
    ) else {
        return;
    };
    let rel_out = ".anubis-temp/cmake-e2e/CMakeLists.txt";
    run(&mut export(rel_out, &[]));
    // The file it wrote is fresh.
    run(&mut export(rel_out, &["--check"]));

    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cmake-e2e");
    let src = repo().join(".anubis-temp/cmake-e2e");
    build_and_check(&cmake, &ninja, &src, &tmp.join("build"));

    // The oldest CMake the file claims to support, where installed.
    let min = repo().join("anubis_cmake/toolchains").join(exe("cmake_min/bin/cmake"));
    if min.is_file() {
        build_and_check(&min, &ninja, &src, &tmp.join("build-min"));
    }

    // Every sample executable runs.
    let mut ran = 0;
    for entry in std::fs::read_dir(tmp.join("build")).unwrap() {
        let p = entry.unwrap().path();
        let n = p.file_name().unwrap().to_string_lossy().to_string();
        if n.starts_with("fx_samples_basic_") && (n.ends_with("_main") || n.ends_with("_main.exe")) {
            run(&mut Command::new(&p));
            ran += 1;
        }
    }
    assert_eq!(
        ran,
        SAMPLES.len(),
        "sample executables found in {}",
        tmp.join("build").display()
    );

    if std::env::var("ANUBIS_CMAKE_COMPARE_ANUBIS").is_ok_and(|v| v == "1") {
        compare_with_anubis();
        if cfg!(target_os = "linux") {
            compare_compile_commands(&tmp.join("build"));
        }
    }
}

/// Every fixture source compiles with the same arguments under CMake as
/// under Anubis, apart from what only Anubis's hermetic toolchain needs (its
/// sysroot and `-nostdinc`-style flags, `-target`) and how each tool names
/// outputs and dependency files. Compared per kind (flags, defines, include
/// dirs), in order: the order between kinds doesn't change a compile.
fn compare_compile_commands(cmake_build: &Path) {
    let anubis = repo().join("target/release").join(exe("anubis"));
    let out = Command::new(&anubis)
        .current_dir(repo())
        .args([
            "-l",
            "trace",
            "build",
            "--no-tui",
            "--no-cache",
            "-m",
            "//mode:linux_dev",
            "-t",
        ])
        .arg("//anubis_cmake/tests/fixtures/...")
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "anubis build failed:\n{log}");
    let fixtures = format!("{}/anubis_cmake/tests/fixtures/", repo().display());
    let mut anubis_cmds: BTreeMap<String, Normalized> = BTreeMap::new();
    for line in log.lines() {
        let Some(i) = line.find("Executing command: ") else {
            continue;
        };
        let toks: Vec<&str> = line[i + 19..].split_whitespace().collect();
        let Some(src) = toks.last() else { continue };
        if toks.contains(&"-c") && src.starts_with(&fixtures) {
            anubis_cmds.insert(norm_path(src), normalize_command(&toks));
        }
    }
    assert!(
        anubis_cmds.len() >= 10,
        "found {} Anubis compiles of fixtures:\n{log}",
        anubis_cmds.len()
    );

    let json = std::fs::read_to_string(cmake_build.join("compile_commands.json")).unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let mut cmake_cmds: BTreeMap<String, Normalized> = BTreeMap::new();
    for e in entries {
        // Ninja quotes paths with spaces; Anubis's log doesn't. Compare tokens unquoted.
        let cmd = e["command"].as_str().unwrap().replace(['"', '\\'], "");
        let toks: Vec<&str> = cmd.split_whitespace().collect();
        cmake_cmds.insert(norm_path(e["file"].as_str().unwrap()), normalize_command(&toks));
    }
    for (src, want) in &anubis_cmds {
        let got = cmake_cmds.get(src).unwrap_or_else(|| panic!("no CMake compile of {src}"));
        assert_eq!(got, want, "compile of {src}: CMake (left) vs Anubis (right)");
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Normalized {
    /// CMake adds its own -fPIC to shared libraries (first); only whether
    /// code is position independent matters.
    pic: bool,
    flags: Vec<String>,
    defines: Vec<String>,
    includes: Vec<String>,
}

fn normalize_command(toks: &[&str]) -> Normalized {
    let mut n = Normalized::default();
    let push = |v: &mut Vec<String>, s: String| {
        if !v.contains(&s) {
            v.push(s);
        }
    };
    let mut i = 1; // the compiler
    let src = norm_path(toks.last().unwrap());
    while i < toks.len() {
        let t = toks[i];
        i += 1;
        match t {
            // Anubis's hermetic toolchain, and each tool's own outputs.
            "-c" | "-MD" | "-nostdinc" | "-nostdinc++" | "-nostdlib" | "-nostdlib++" | "-nodefaultlibs"
            | "-D__linux__" | "-D__x86_64__" => {}
            "-o" | "-MF" | "-MT" | "-MQ" | "-target" | "-isystem" => i += 1,
            // CMake marks a .c file in a C++ target; clang++ compiles it as C++ anyway.
            "-x" if toks.get(i) == Some(&"c++") => i += 1,
            t if t.starts_with("-isysroot")
                || t.starts_with("-resource-dir")
                || t.starts_with("--target=") => {}
            t if t.starts_with("-fcolor-diagnostics") || t.starts_with("-fdiagnostics-color") => {}
            "-fPIC" => n.pic = true,
            "-include" | "-I" | "-iquote" => {
                let v = format!("{t} {}", norm_path(toks.get(i).copied().unwrap_or("")));
                i += 1;
                push(
                    if t == "-include" {
                        &mut n.flags
                    } else {
                        &mut n.includes
                    },
                    v,
                );
            }
            t if norm_path(t) == src => {}
            t if t.starts_with("-D") || t.starts_with("-U") => push(&mut n.defines, t.to_string()),
            t if t.starts_with("-I") => push(&mut n.includes, format!("-I {}", norm_path(&t[2..]))),
            t => push(&mut n.flags, norm_path(t)),
        }
    }
    n
}

/// Absolute paths lexically normalized, with the repo root as `<root>`.
fn norm_path(t: &str) -> String {
    if !t.starts_with('/') {
        return t.to_string();
    }
    let mut parts: Vec<&str> = vec![];
    for p in t.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let p = format!("/{}", parts.join("/"));
    let root = repo().display().to_string();
    match p.strip_prefix(&root) {
        Some(rest) => format!("<root>{rest}"),
        None => p,
    }
}

/// The expected outputs are what Anubis's own builds print.
fn compare_with_anubis() {
    let anubis = repo().join("target/release").join(exe("anubis"));
    let mode = if cfg!(windows) {
        "//mode:win_dev"
    } else {
        "//mode:linux_dev"
    };
    run(Command::new(&anubis).current_dir(repo()).args([
        "build",
        "--no-tui",
        "-m",
        mode,
        "-t",
        "//anubis_cmake/tests/fixtures/...",
    ]));
    let prefix = if cfg!(windows) { "win_dev_" } else { "linux_dev_" };
    let out_dir = std::fs::read_dir(repo().join(".anubis-out"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .expect("anubis output dir");
    for (name, want) in expected() {
        let p =
            out_dir.join("anubis_cmake/tests/fixtures").join(&name).join(&name).join("bin").join(exe(&name));
        let got = run(&mut Command::new(&p));
        assert_eq!(got.trim(), want, "{name} (Anubis build)");
    }
}

#[test]
fn unexported_platforms_are_refused() {
    let (Some(cmake), Some(ninja)) = (
        tool("ANUBIS_CMAKE_CMAKE", "cmake/bin/cmake"),
        tool("ANUBIS_CMAKE_NINJA", "ninja/ninja"),
    ) else {
        return;
    };
    // Export for the other platform only, then configure here.
    let (mode, other, here) = if cfg!(windows) {
        ("//mode:linux_dev", "Linux", "Windows")
    } else {
        (
            "//mode:win_dev",
            "Windows",
            if cfg!(target_os = "macos") {
                "Darwin"
            } else {
                "Linux"
            },
        )
    };
    let rel_out = ".anubis-temp/cmake-e2e-platform/CMakeLists.txt";
    run(
        Command::new(env!("CARGO_BIN_EXE_anubis-cmake")).current_dir(repo()).args([
            "--namespace",
            "fx",
            "-o",
            rel_out,
            "-m",
            mode,
            "-t",
            "//samples/basic/trivial_c:main",
        ]),
    );
    let build = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cmake-e2e-platform");
    let _ = std::fs::remove_dir_all(&build);
    let out = Command::new(&cmake)
        .arg("-S")
        .arg(repo().join(".anubis-temp/cmake-e2e-platform"))
        .arg("-B")
        .arg(&build)
        .args(["-G", "Ninja"])
        .arg(format!("-DCMAKE_MAKE_PROGRAM={}", ninja.display()))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "configure should fail:\n{text}");
    assert!(
        text.contains(&format!("exported for {other} (x64), not {here}")),
        "{text}"
    );
}

/// The generated file for the fixtures, checked in so changes to the output
/// show up in review, and identical on every host. `UPDATE_GOLDEN=1`
/// rewrites it.
#[test]
fn generated_file_matches_golden() {
    let rel_out = ".anubis-temp/cmake-golden/CMakeLists.txt";
    let mut c = Command::new(env!("CARGO_BIN_EXE_anubis-cmake"));
    c.current_dir(repo())
        .args(["--namespace", "fx", "-o", rel_out, "-m"])
        .args(MODES)
        .arg("-t")
        .arg("//anubis_cmake/tests/fixtures/...");
    run(&mut c);
    let got = std::fs::read_to_string(repo().join(rel_out)).unwrap();
    let golden = repo().join("anubis_cmake/tests/fixtures/CMakeLists.golden.txt");
    if std::env::var("UPDATE_GOLDEN").is_ok_and(|v| v == "1") {
        std::fs::write(&golden, &got).unwrap();
    }
    let want = std::fs::read_to_string(&golden).unwrap().replace("\r\n", "\n");
    assert!(
        got == want,
        "generated file differs from {}; rerun with UPDATE_GOLDEN=1 and review the diff",
        golden.display()
    );
}
