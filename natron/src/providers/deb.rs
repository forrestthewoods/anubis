//! `deb` provider: download a fixed set of Debian packages and merge part of
//! their contents (by default `usr/include`) into one tree.
//!
//! Made for system headers that a Linux target needs but no toolchain ships,
//! such as X11, xcb and GL. Only files are taken from the packages; nothing
//! is installed or run, so this works the same on Windows, Linux and macOS
//! hosts. Every package is pinned by pool path and SHA-256.
//!
//! ```toml
//! [[toolchain]]
//! name     = "x11_headers"
//! provider = "deb"
//! [toolchain.options]
//! mirror   = "https://deb.debian.org/debian"   # default; or a list, tried in order
//! strip_prefix = "usr/include"                 # default
//! packages = [
//!   { path = "pool/main/libx/libx11/libx11-dev_1.8.4-2+deb12u2_amd64.deb", sha256 = "..." },
//! ]
//! ```

use anyhow::{Context, Result, anyhow};

use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::extract;

/// Provider id used in `[[toolchain]] provider = "deb"`.
pub const ID: &str = "deb";

const DEFAULT_MIRROR: &str = "https://deb.debian.org/debian";
const DEFAULT_STRIP: &str = "usr/include";

pub struct DebProvider;

impl DebProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DebProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DebPackage {
    pub path: String,
    pub sha256: String,
}

pub(crate) fn parse_packages(options: &toml::Table) -> Result<Vec<DebPackage>> {
    let arr = options
        .get("packages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("`deb` provider requires options.packages (array of {{ path, sha256 }})"))?;
    let mut out = Vec::new();
    for (i, v) in arr.iter().enumerate() {
        let t = v
            .as_table()
            .ok_or_else(|| anyhow!("packages[{i}] must be a table {{ path, sha256 }}"))?;
        let get = |k: &str| {
            t.get(k)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| anyhow!("packages[{i}] is missing `{k}`"))
        };
        out.push(DebPackage { path: get("path")?, sha256: get("sha256")? });
    }
    if out.is_empty() {
        anyhow::bail!("`deb` provider: options.packages is empty");
    }
    Ok(out)
}

/// `mirror` is one URL or a list tried in order (e.g. the main pool, then
/// snapshot.debian.org, which keeps packages the pool has dropped).
pub(crate) fn parse_mirrors(options: &toml::Table) -> Result<Vec<String>> {
    let mirrors: Vec<String> = match options.get("mirror") {
        None => vec![DEFAULT_MIRROR.to_string()],
        Some(toml::Value::String(m)) => vec![m.clone()],
        Some(toml::Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| anyhow!("`mirror` entries must be strings")))
            .collect::<Result<_>>()?,
        Some(_) => anyhow::bail!("`mirror` must be a string or an array of strings"),
    };
    if mirrors.is_empty() {
        anyhow::bail!("`mirror` is an empty list");
    }
    Ok(mirrors.into_iter().map(|m| m.trim_end_matches('/').to_string()).collect())
}

fn download_from_mirrors(
    ctx: &InstallCtx,
    mirrors: &[String],
    p: &DebPackage,
) -> Result<(String, std::path::PathBuf)> {
    let mut errors = Vec::new();
    for mirror in mirrors {
        let url = format!("{mirror}/{}", p.path.trim_start_matches('/'));
        match ctx.download(&url, Some(&p.sha256)) {
            Ok(archive) => return Ok((url, archive)),
            Err(e) => {
                tracing::warn!("{url}: {e:#}");
                errors.push(format!("{url}: {e:#}"));
            }
        }
    }
    anyhow::bail!("every mirror failed for {}:\n  {}", p.path, errors.join("\n  "))
}

pub(crate) fn compute_fingerprint(name: &str, mirror: &str, strip: &str, pkgs: &[DebPackage]) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    // The mirror isn't part of the content (sha256 pins it), so leave it out:
    // switching mirrors must not invalidate installs.
    let _ = mirror;
    let mut key = format!("{strip}\n");
    for p in pkgs {
        key.push_str(&p.path);
        key.push('|');
        key.push_str(&p.sha256.to_ascii_lowercase());
        key.push('\n');
    }
    sanitize_fingerprint(&format!("deb-{name}-{:016x}", xxh3_64(key.as_bytes())))
}

impl Provider for DebProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn install(&self, options: &toml::Table, ctx: &mut InstallCtx) -> Result<Installed> {
        let mirrors = parse_mirrors(options)?;
        let strip = options
            .get("strip_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_STRIP)
            .to_string();
        let pkgs = parse_packages(options)?;
        // Name the install after the first package so cache dirs are legible.
        let stem = pkgs[0]
            .path
            .rsplit('/')
            .next()
            .and_then(|f| f.split('_').next())
            .unwrap_or("deb")
            .to_string();
        let fingerprint = compute_fingerprint(&stem, &mirrors[0], &strip, &pkgs);
        let display = format!("deb {} packages ({stem}, ...)", pkgs.len());

        if ctx.cache().install_present(&fingerprint) {
            return Ok(Installed {
                fingerprint,
                display,
                options: options.clone(),
                freshly_extracted: false,
            });
        }

        let staging = ctx.staging_dir()?;
        for p in &pkgs {
            let (url, archive) = download_from_mirrors(ctx, &mirrors, p)?;
            extract::extract_deb(&archive, &staging, Some(&strip))
                .with_context(|| format!("extracting {url}"))?;
        }

        Ok(Installed {
            fingerprint,
            display,
            options: options.clone(),
            freshly_extracted: true,
        })
    }
}

#[cfg(test)]
#[path = "tests/deb.rs"]
mod tests;
