//! `git` provider: install the tree of one pinned commit of a git repository.
//!
//! For sources that only exist as a repository, like OpenCASCADE, which has
//! no release assets. GitHub's generated `/archive/<ref>.tar.gz` would do
//! for a `url` entry, but some networks (sandboxed cloud machines) allow
//! git but not those archives, and their bytes aren't guaranteed stable.
//!
//! Fetches only that commit (like `git fetch --depth 1 <repo> <commit>`)
//! and checks it out, using gitoxide (`gix`), so no `git` install is needed.
//! The full 40-hex commit hash pins the content, the way `sha256` pins a
//! `url` archive. The user's git config is ignored, so every host gets the
//! same bytes (no `core.autocrlf`, no filter drivers).
//!
//! `http(s)://` repositories need nothing installed. They must be readable
//! without credentials: no credential helper is consulted and nothing
//! prompts. `ssh` URLs run the `ssh` program and `file://` URLs run
//! `git upload-pack`. Submodules are not fetched (their directories are
//! left empty) and Git LFS files stay pointer files.
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
        let git_dir = root.join("git");
        fetch_commit(&git_dir, &raw, repo, &commit, strip_prefix.as_deref())?;
        // The staging root becomes the install directory, so the fetched
        // pack must not stay in it. Pack files are read-only.
        crate::fs_util::remove_dir_all_writable(&git_dir)
            .with_context(|| format!("removing {}", git_dir.display()))?;

        Ok(Installed {
            fingerprint,
            display,
            options: options.clone(),
            freshly_extracted: true,
        })
    }
}

/// Shallow-fetches exactly `commit` from `repo` into a bare repository at
/// `git_dir`, then checks its tree (or the `strip_prefix` subtree) out into
/// `dest`. No `.git` ends up in `dest`.
fn fetch_commit(git_dir: &Path, dest: &Path, repo: &str, commit: &str, strip_prefix: Option<&str>) -> Result<()> {
    use gix::remote::Direction;
    use std::sync::atomic::AtomicBool;

    let oid = gix::ObjectId::from_hex(commit.as_bytes()).context("parsing commit")?;
    // Isolated: ignore the user's git config and GIT_* variables, so every
    // host checks out the same bytes (no autocrlf, no filter drivers).
    // Proxies still come from HTTPS_PROXY etc. via the HTTP client.
    // No credential prompt either: a private or mistyped repository fails
    // instead of waiting on the terminal.
    let open_opts =
        gix::open::Options::isolated().config_overrides(["gitoxide.credentials.terminalPrompt=false"]);
    let r = gix::ThreadSafeRepository::init_opts(
        git_dir,
        gix::create::Kind::Bare,
        gix::create::Options::default(),
        open_opts,
    )
    .with_context(|| format!("creating {}", git_dir.display()))?
    .to_thread_local();

    let interrupt = AtomicBool::new(false);
    tracing::info!("git: fetching {repo} at {commit}");
    let fetch = || -> Result<()> {
        let mut remote = r
            .remote_at(repo)?
            .with_fetch_tags(gix::remote::fetch::Tags::None);
        remote.replace_refspecs(Some(commit), Direction::Fetch)?;
        remote
            .connect(Direction::Fetch)?
            .prepare_fetch(gix::progress::Discard, Default::default())?
            .with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(1.try_into().expect("1 > 0")))
            .receive(gix::progress::Discard, &interrupt)?;
        Ok(())
    };
    fetch().with_context(|| format!("fetching {commit} from {repo}"))?;

    let commit_obj = r
        .find_commit(oid)
        .with_context(|| format!("{repo} did not send commit {commit}"))?;
    let mut tree = commit_obj.tree()?;
    if let Some(p) = strip_prefix {
        let entry = tree
            .peel_to_entry_by_path(p)?
            .filter(|e| e.mode().is_tree())
            .ok_or_else(|| anyhow!("`git` provider: {repo} at {commit} has no directory {p:?}"))?;
        tree = entry.object()?.into_tree();
    }

    let mut index = r.index_from_tree(&tree.id)?;
    let mut opts = r.checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)?;
    opts.destination_is_initially_empty = true;
    let out = gix::worktree::state::checkout(
        &mut index,
        dest,
        r.objects.clone().into_arc()?,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &interrupt,
        opts,
    )
    .with_context(|| format!("checking out {commit} into {}", dest.display()))?;
    if let Some(e) = out.errors.first() {
        bail!("`git` provider: checking out {}: {}", e.path, e.error);
    }
    if let Some(c) = out.collisions.first() {
        bail!("`git` provider: path collision checking out {} (case-insensitive filesystem?)", c.path);
    }
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
