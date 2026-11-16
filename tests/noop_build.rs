//! A build with nothing to do does nothing.
//!
//! Builds the samples three times and requires that the third build
//! - adds, removes, rewrites or relinks no file under `.anubis-out` or in
//!   the sample sources,
//! - reads no file to hash it,
//! - misses no cache entry, and starts no tool or in-process action.
//!
//! This is the rule "no action may be silently uncacheable" as a check, for
//! every rule type the samples use. An *action* (new bytes: compile, link,
//! archive) passes by having an action cache entry. A *placement* (existing
//! bytes at a path: an executable staged next to its runtime libraries, a
//! bundle) passes by going through `stage.rs` or the cache's restore, which leave files in place.
//!
//! Needs the toolchains (`anubis install-toolchains`), so it is `#[ignore]`d
//! and run by name in CI:
//!
//!     cargo test --release --test noop_build -- --ignored

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Targets that still do work on every build. Each needs an issue; this
/// list should only shrink.
const KNOWN_FAILURES: &[(&str, &str)] = &[
    // The archive rules have no action cache entry: they compress and
    // rewrite the archive on every build.
    ("//samples/basic/nasm_c:src", "https://github.com/forrestthewoods/anubis/issues/244"),
    ("//samples/basic/nasm_c:src_7z", "https://github.com/forrestthewoods/anubis/issues/244"),
];

/// What a build may not change about a file. `ctime` and `inode` are 0 on
/// Windows. A hard link that is deleted and made again keeps its size,
/// modification time and inode; only the change time shows it.
#[derive(Debug, PartialEq, Eq)]
struct FileState {
    size: u64,
    modified: std::time::SystemTime,
    ctime: (i64, i64),
    inode: u64,
}

fn snapshot(root: &Path, dirs: &[&str]) -> BTreeMap<PathBuf, FileState> {
    let mut out = BTreeMap::new();
    let mut stack: Vec<PathBuf> = dirs.iter().map(|d| root.join(d)).collect();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            #[cfg(unix)]
            let (ctime, inode) = {
                use std::os::unix::fs::MetadataExt;
                ((meta.ctime(), meta.ctime_nsec()), meta.ino())
            };
            #[cfg(not(unix))]
            let (ctime, inode) = ((0, 0), 0);
            out.insert(path, FileState { size: meta.len(), modified: meta.modified().unwrap(), ctime, inode });
        }
    }
    out
}

fn anubis(root: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_anubis")).current_dir(root).args(args).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "anubis {args:?} failed:\n{text}");
    text
}

/// Every number that comes right before `label` in the build output, e.g.
/// `3` in `3 misses`: one per build summary.
fn numbers_before(log: &str, label: &str) -> Vec<u64> {
    log.match_indices(label)
        .filter_map(|(at, _)| {
            let digits: String = log[..at].trim_end().chars().rev().take_while(|c| c.is_ascii_digit()).collect();
            digits.chars().rev().collect::<String>().parse().ok()
        })
        .collect()
}

#[test]
#[ignore = "needs the toolchains; run with --ignored"]
fn a_build_with_nothing_to_do_does_nothing() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The host's own platform plus one cross-compile. NASM is x86-only, so
    // macOS (arm64) modes get their own target list below.
    let modes: &[&str] = match std::env::consts::OS {
        "windows" => &["//mode:win_dev", "//mode:linux_dev"],
        "macos" => &["//mode:linux_dev"],
        _ => &["//mode:linux_dev", "//mode:win_dev"],
    };
    let listed = anubis(root, &["targets", "//samples/basic/..."]);
    let targets: Vec<&str> = listed
        .lines()
        .map(str::trim)
        .filter(|t| t.starts_with("//"))
        .filter(|t| !KNOWN_FAILURES.iter().any(|(known, _)| known == t))
        .collect();
    assert!(targets.len() > 10, "expected the sample targets, got:\n{listed}");

    let mut basic = vec!["build", "--no-tui", "-m"];
    basic.extend(modes);
    basic.push("-t");
    basic.extend(&targets);
    // macOS targets build on every host: every sample executable (the
    // NASM-only targets are x86) and a bundle (`macos_app`).
    let mut macos = vec!["build", "--no-tui", "-m", "//mode:macos_dev", "-t", "//samples/macos/objc:ObjcSample"];
    macos.extend(targets.iter().filter(|t| t.ends_with(":main")));
    let build = || format!("{}{}", anubis(root, &basic), anubis(root, &macos));

    build();
    // The stat cache doesn't trust a digest taken within 2 s of the file's
    // modification time, so the build after a real one still reads the
    // files that one wrote. The build after that is the settled one.
    std::thread::sleep(std::time::Duration::from_secs(3));
    build();

    let dirs = [".anubis-out", "samples/basic", "samples/macos"];
    let before = snapshot(root, &dirs);
    let log = build();
    let after = snapshot(root, &dirs);

    let mut problems = Vec::new();
    for (path, state) in &after {
        match before.get(path) {
            None => problems.push(format!("added: {}", path.display())),
            Some(old) if old != state => {
                problems.push(format!("rewritten: {}\n    before {old:?}\n    after  {state:?}", path.display()))
            }
            _ => {}
        }
    }
    for path in before.keys().filter(|p| !after.contains_key(*p)) {
        problems.push(format!("removed: {}", path.display()));
    }
    for (label, what) in [
        ("misses", "cache misses"),
        ("files hashed", "files read to hash them"),
        ("tool(s)", "tools started"),
        ("in-process action(s)", "in-process actions run"),
    ] {
        let n = numbers_before(&log, label);
        assert_eq!(n.len(), 2, "expected two build summaries with '{label}':\n{log}");
        if n.iter().any(|&n| n != 0) {
            problems.push(format!("{what}: {n:?}"));
        }
    }
    assert!(problems.is_empty(), "a build with nothing to do did work:\n{}\n\n{log}", problems.join("\n"));
}
