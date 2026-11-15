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
//! bytes at a path: staged runtime libraries) passes by going through
//! `sync_dir` or the cache's restore, which skip files already in place.
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
    ("//samples/basic/nasm_c:src", "ISSUE_ARCHIVE"),
    ("//samples/basic/nasm_c:src_7z", "ISSUE_ARCHIVE"),
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

/// The number before `label` in the build summary, e.g. `3` in `3 misses`.
fn number_before(log: &str, label: &str) -> u64 {
    let at = log.rfind(label).unwrap_or_else(|| panic!("no '{label}' in the build output:\n{log}"));
    let digits: String = log[..at].trim_end().chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    digits.chars().rev().collect::<String>().parse().unwrap_or_else(|_| panic!("no number before '{label}'"))
}

#[test]
#[ignore = "needs the toolchains; run with --ignored"]
fn a_build_with_nothing_to_do_does_nothing() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The host's own platform plus one cross-compile.
    let modes: &[&str] = match std::env::consts::OS {
        "windows" => &["//mode:win_dev", "//mode:linux_dev"],
        "macos" => &["//mode:macos_dev"],
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

    let mut args = vec!["build", "--no-tui", "-m"];
    args.extend(modes);
    args.push("-t");
    args.extend(&targets);

    anubis(root, &args);
    // The stat cache doesn't trust a digest taken within 2 s of the file's
    // modification time, so the build after a real one still reads the
    // files that one wrote. The build after that is the settled one.
    std::thread::sleep(std::time::Duration::from_secs(3));
    anubis(root, &args);

    let dirs = [".anubis-out", "samples/basic"];
    let before = snapshot(root, &dirs);
    let log = anubis(root, &args);
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
        let n = number_before(&log, label);
        if n != 0 {
            problems.push(format!("{what}: {n}"));
        }
    }
    assert!(problems.is_empty(), "a build with nothing to do did work:\n{}\n\n{log}", problems.join("\n"));
}
