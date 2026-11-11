//! `git` provider: install the tree of one pinned commit of a git repository.
//!
//! For sources that only exist as a repository, like OpenCASCADE, which has
//! no release assets. GitHub's generated `/archive/<ref>.tar.gz` would do
//! for a `url` entry, but some networks (sandboxed cloud machines) allow
//! git but not those archives, and their bytes aren't guaranteed stable.
//!
//! Fetches only that commit (`git fetch --depth 1 <repo> <commit>`) and
//! checks it out. The full 40-hex commit hash pins the content, the way
//! `sha256` pins a `url` archive. Needs `git` on PATH.
//!
//! ```toml
//! [[toolchain]]
//! name       = "occt"
//! deploy_dir = "third_party/occt"
//! provider   = "git"
//! [toolchain.options]
//! repo         = "https://github.com/Open-Cascade-SAS/OCCT.git"
//! commit       = "<40 hex digits>"
//! version      = "7.9.3"        # optional, display only
//! strip_prefix = "src"          # optional: install only this subdirectory
//! ```

use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;
use std::process::Command;

use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;

/// Provider id used in `[[toolchain]] provider = "git"`.
pub const ID: &str = "git";

pub struct GitProvider;

impl GitProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GitProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for GitProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn install(&self, options: &toml::Table, ctx: &mut InstallCtx) -> Result<Installed> {
        let repo = super::options::required_str(options, "repo", ID)?;
        let commit = super::options::required_str(options, "commit", ID)?;
        if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("`git` provider: options.commit must be a full 40-hex commit hash, got {commit:?}");
        }
        let commit = commit.to_ascii_lowercase();
        let strip_prefix = super::options::optional_str(options, "strip_prefix", ID)?
            .map(|s| s.trim_matches('/').to_string())
            .filter(|s| !s.is_empty());
        if let Some(p) = &strip_prefix {
            // `/`-separated components only: a `\` or `:` would be path
            // syntax on Windows (`..\x`, `C:\x`) and could leave the checkout.
            if p.contains(['\\', ':']) || p.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
                bail!("`git` provider: options.strip_prefix must be a relative path, got {p:?}");
            }
        }

        let fingerprint = sanitize_fingerprint(&compute_fingerprint(repo, &commit, &strip_prefix));
        let display = display(repo, &commit, options);
        if ctx.cache().install_present(&fingerprint) {
            return Ok(Installed {
                fingerprint,
                display,
                options: options.clone(),
                freshly_extracted: false,
            });
        }

        let raw = ctx.staging_dir()?;
        let root = ctx.staging_root().ok_or_else(|| anyhow!("no staging root"))?;
        let checkout = root.join("git");
        std::fs::create_dir_all(&checkout)
            .with_context(|| format!("creating {}", checkout.display()))?;
        fetch_commit(&checkout, repo, &commit)?;

        // Move the wanted tree into raw/ (same filesystem, so a rename).
        let src = match &strip_prefix {
            Some(p) => checkout.join(p),
            None => {
                std::fs::remove_dir_all(checkout.join(".git")).context("removing .git")?;
                checkout.clone()
            }
        };
        if !src.is_dir() {
            bail!("`git` provider: {repo} at {commit} has no directory {:?}", strip_prefix.unwrap_or_default());
        }
        std::fs::remove_dir(&raw).with_context(|| format!("removing {}", raw.display()))?;
        std::fs::rename(&src, &raw)
            .with_context(|| format!("moving {} to {}", src.display(), raw.display()))?;
        if checkout.exists() {
            std::fs::remove_dir_all(&checkout).ok();
        }

        Ok(Installed {
            fingerprint,
            display,
            options: options.clone(),
            freshly_extracted: true,
        })
    }
}

/// `git init` + shallow fetch of exactly `commit` + checkout, in `dir`.
fn fetch_commit(dir: &Path, repo: &str, commit: &str) -> Result<()> {
    // Same bytes on every host: no CRLF conversion, no filters from the
    // user's git config.
    let git = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "core.autocrlf=false", "-c", "core.eol=lf", "-c", "advice.detachedHead=false"])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            // Set when natron runs from a git hook or similar; they would
            // point every command at the caller's repository instead.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .with_context(|| "running git (the `git` provider needs git on PATH)")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    git(&["init", "-q"])?;
    tracing::info!("git: fetching {repo} at {commit}");
    git(&["fetch", "-q", "--depth", "1", "--no-tags", repo, commit])
        .with_context(|| format!("fetching {commit} from {repo}"))?;
    let got = git(&["rev-parse", "FETCH_HEAD"])?;
    if got != commit {
        bail!("`git` provider: {repo} returned {got}, expected {commit}");
    }
    git(&["checkout", "-q", "--detach", "FETCH_HEAD"])?;
    Ok(())
}

fn repo_stem(repo: &str) -> &str {
    repo.trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git")
}

fn compute_fingerprint(repo: &str, commit: &str, strip_prefix: &Option<String>) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let key = format!("{repo}|{commit}|{}", strip_prefix.as_deref().unwrap_or(""));
    let h = xxh3_64(key.as_bytes());
    format!("git-{}-{}-{:08x}", repo_stem(repo), &commit[..12], h & 0xFFFF_FFFF)
}

fn display(repo: &str, commit: &str, options: &toml::Table) -> String {
    match options.get("version").and_then(|v| v.as_str()) {
        Some(v) => format!("git {} {v} ({})", repo_stem(repo), &commit[..12]),
        None => format!("git {} {}", repo_stem(repo), &commit[..12]),
    }
}

#[cfg(test)]
#[path = "tests/git.rs"]
mod tests;
