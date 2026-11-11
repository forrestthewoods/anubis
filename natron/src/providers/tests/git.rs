//! Tests for `src/providers/git.rs`. The fixtures are made with the `git`
//! CLI, and gix fetches `file://` URLs through `git upload-pack`, so these
//! need `git` on PATH (the provider itself does not for http/https).

use super::*;
use crate::cache::Cache;
use std::process::Command;
use tempfile::TempDir;

fn run(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        // core.autocrlf=false: Windows runners default to true, which would
        // commit the CRLF fixture as LF.
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with two commits; returns (repo dir, first commit, second commit).
fn make_repo(tmp: &Path) -> (std::path::PathBuf, String, String) {
    let repo = tmp.join("OCCT");
    std::fs::create_dir_all(repo.join("src/Standard")).unwrap();
    run(&repo, &["init", "-q"]);
    std::fs::write(repo.join("src/Standard/Standard.hxx"), b"v1\r\n").unwrap();
    std::fs::write(repo.join("README.md"), b"readme").unwrap();
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "one"]);
    let first = run(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("src/Standard/Standard.hxx"), b"v2").unwrap();
    run(&repo, &["commit", "-q", "-am", "two"]);
    let second = run(&repo, &["rev-parse", "HEAD"]);
    (repo, first, second)
}

fn opts(repo: &Path, commit: &str, strip: Option<&str>) -> toml::Table {
    let mut t = toml::Table::new();
    t.insert("repo".into(), url::Url::from_file_path(repo).unwrap().to_string().into());
    t.insert("commit".into(), commit.into());
    if let Some(s) = strip {
        t.insert("strip_prefix".into(), s.into());
    }
    t
}

#[test]
fn installs_pinned_commit_not_head() {
    let tmp = TempDir::new().unwrap();
    let (repo, first, _second) = make_repo(tmp.path());
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let installed = GitProvider::new().install(&opts(&repo, &first, None), &mut ctx).unwrap();
    assert!(installed.freshly_extracted);
    assert!(installed.fingerprint.starts_with(&format!("git-OCCT-{}", &first[..12])));
    let raw = ctx.staging_dir().unwrap();
    // The old commit's bytes, CRLF untouched, and no .git.
    assert_eq!(std::fs::read(raw.join("src/Standard/Standard.hxx")).unwrap(), b"v1\r\n");
    assert!(raw.join("README.md").is_file());
    assert!(!raw.join(".git").exists());
    // The fetched pack is gone: the staging root becomes the install dir.
    let left: Vec<_> = std::fs::read_dir(ctx.staging_root().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["raw"]);
}

/// Mode bits and symlinks come out as committed.
#[cfg(unix)]
#[test]
fn keeps_executable_bit_and_symlinks() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q"]);
    std::fs::write(repo.join("run.sh"), b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(repo.join("plain.txt"), b"x").unwrap();
    std::os::unix::fs::symlink("plain.txt", repo.join("link")).unwrap();
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "one"]);
    let commit = run(&repo, &["rev-parse", "HEAD"]);
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    GitProvider::new().install(&opts(&repo, &commit, None), &mut ctx).unwrap();
    let raw = ctx.staging_dir().unwrap();
    let mode = |f: &str| std::fs::metadata(raw.join(f)).unwrap().permissions().mode();
    assert_ne!(mode("run.sh") & 0o111, 0);
    assert_eq!(mode("plain.txt") & 0o111, 0);
    assert_eq!(std::fs::read_link(raw.join("link")).unwrap(), Path::new("plain.txt"));
}

#[test]
fn strip_prefix_must_be_a_directory_in_the_commit() {
    let tmp = TempDir::new().unwrap();
    let (repo, _first, second) = make_repo(tmp.path());
    for bad in ["README.md", "src/nope"] {
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);
        let err = GitProvider::new().install(&opts(&repo, &second, Some(bad)), &mut ctx).unwrap_err();
        assert!(format!("{err:#}").contains("has no directory"), "{bad}: {err:#}");
    }
}

#[test]
fn strip_prefix_installs_subdirectory() {
    let tmp = TempDir::new().unwrap();
    let (repo, _first, second) = make_repo(tmp.path());
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    GitProvider::new().install(&opts(&repo, &second, Some("src")), &mut ctx).unwrap();
    let raw = ctx.staging_dir().unwrap();
    assert_eq!(std::fs::read(raw.join("Standard/Standard.hxx")).unwrap(), b"v2");
    assert!(!raw.join("README.md").exists());
}

#[test]
fn rejects_short_commit_and_bad_prefix() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let p = GitProvider::new();
    let err = format!("{:#}", p.install(&opts(tmp.path(), "abc123", None), &mut ctx).unwrap_err());
    assert!(err.contains("40-hex"), "{err}");
    let c = "0".repeat(40);
    for bad in ["../x", "a/../../x", "..\\x", "C:\\x", "C:x"] {
        let err = format!("{:#}", p.install(&opts(tmp.path(), &c, Some(bad)), &mut ctx).unwrap_err());
        assert!(err.contains("relative path"), "{bad}: {err}");
    }
}

#[test]
fn unknown_commit_fails() {
    let tmp = TempDir::new().unwrap();
    let (repo, _first, _second) = make_repo(tmp.path());
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let c = "1".repeat(40);
    assert!(GitProvider::new().install(&opts(&repo, &c, None), &mut ctx).is_err());
}

#[test]
fn repo_stem_handles_forms() {
    assert_eq!(repo_stem("https://github.com/Open-Cascade-SAS/OCCT.git"), "OCCT");
    assert_eq!(repo_stem("https://github.com/Tencent/rapidjson/"), "rapidjson");
    assert_eq!(repo_stem("git@github.com:CGAL/cgal.git"), "cgal");
}
