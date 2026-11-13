//! Tests for `src/providers/git.rs`. The fixtures are made with the `git`
//! CLI, and gix fetches `file://` URLs through `git upload-pack`, so these
//! need `git` on PATH (the provider itself does not for http/https). The
//! HTTP tests run a small local server that also shells out to `git`.

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
    opts_url(url::Url::from_file_path(repo).unwrap().as_str(), commit, strip)
}

fn opts_url(repo: &str, commit: &str, strip: Option<&str>) -> toml::Table {
    let mut t = toml::Table::new();
    t.insert("repo".into(), repo.into());
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

/// A minimal git smart-HTTP server for one repository, served at `/new/`.
/// `/old/...` redirects to `/new/...`, like a renamed GitHub repository.
/// Each request runs `git upload-pack --stateless-rpc`, which is what
/// `git http-backend` does.
struct GitHttpServer {
    addr: std::net::SocketAddr,
    /// "METHOD path" of every request, in order.
    log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl GitHttpServer {
    fn spawn(repo: std::path::PathBuf) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_c = log.clone();
        // Detached: it ends with the test process.
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve_git_request(stream, &repo, &log_c);
            }
        });
        Self { addr, log }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}/{path}", self.addr)
    }
}

fn serve_git_request(mut stream: std::net::TcpStream, repo: &Path, log: &std::sync::Mutex<Vec<String>>) {
    use std::io::{BufRead, Read, Write};
    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let (mut content_length, mut git_protocol) = (0usize, String::new());
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let Some((name, value)) = line.trim_end().split_once(':') else {
            break;
        };
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().unwrap(),
            "git-protocol" => git_protocol = value.trim().to_string(),
            _ => {}
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).unwrap();
    log.lock().unwrap().push(format!("{method} {path}"));

    let mut respond = |status: &str, headers: &str, body: &[u8]| {
        let head = format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
    };
    if let Some(rest) = path.strip_prefix("/old/") {
        return respond("301 Moved Permanently", &format!("Location: /new/{rest}\r\n"), b"");
    }
    let (kind, advertise) = match (method.as_str(), path.as_str()) {
        ("GET", "/new/info/refs?service=git-upload-pack") => ("advertisement", true),
        ("POST", "/new/git-upload-pack") => ("result", false),
        _ => return respond("404 Not Found", "", b"no such repository"),
    };
    let mut cmd = Command::new("git");
    cmd.args(["upload-pack", "--stateless-rpc"]);
    if advertise {
        cmd.arg("--advertise-refs");
    }
    let mut child = cmd
        .arg(repo)
        .env("GIT_PROTOCOL", git_protocol)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&body).unwrap();
    let out = child.wait_with_output().unwrap();
    respond("200 OK", &format!("Content-Type: application/x-git-upload-pack-{kind}\r\n"), &out.stdout);
}

/// The whole fetch over HTTP, through `git_http.rs`.
#[test]
fn installs_over_http() {
    let tmp = TempDir::new().unwrap();
    let (repo, first, _second) = make_repo(tmp.path());
    let server = GitHttpServer::spawn(repo);
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    GitProvider::new().install(&opts_url(&server.url("new"), &first, None), &mut ctx).unwrap();
    let raw = ctx.staging_dir().unwrap();
    assert_eq!(std::fs::read(raw.join("src/Standard/Standard.hxx")).unwrap(), b"v1\r\n");
    assert!(raw.join("README.md").is_file());
}

/// A moved repository: the first GET is redirected, and the POSTs then go
/// straight to the new location.
#[test]
fn follows_a_redirect_on_the_first_request() {
    let tmp = TempDir::new().unwrap();
    let (repo, _first, second) = make_repo(tmp.path());
    let server = GitHttpServer::spawn(repo);
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    GitProvider::new().install(&opts_url(&server.url("old"), &second, Some("src")), &mut ctx).unwrap();
    let raw = ctx.staging_dir().unwrap();
    assert_eq!(std::fs::read(raw.join("Standard/Standard.hxx")).unwrap(), b"v2");
    let log = server.log.lock().unwrap();
    assert_eq!(log[0], "GET /old/info/refs?service=git-upload-pack");
    assert_eq!(log[1], "GET /new/info/refs?service=git-upload-pack");
    assert!(log[2..].iter().all(|r| r == "POST /new/git-upload-pack"), "{log:?}");
    assert!(log.len() > 2, "{log:?}");
}

#[test]
fn http_error_status_is_reported() {
    let tmp = TempDir::new().unwrap();
    let (repo, first, _second) = make_repo(tmp.path());
    let server = GitHttpServer::spawn(repo);
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let err = GitProvider::new().install(&opts_url(&server.url("nope"), &first, None), &mut ctx).unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("HTTP status 404"), "{err}");
    assert_eq!(err.matches("HTTP status 404").count(), 1, "{err}");
}
