//! TOML config (`natron.toml`) parsing, validation, path expansion, and discovery.
//!
//! Schema is intentionally small: a `[settings]` table and an array of
//! `[[toolchain]]` entries. Each entry has a `[toolchain.options]` sub-table
//! that the chosen provider validates.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const CONFIG_FILENAME: &str = "natron.toml";

/// Root of a parsed `natron.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "toolchain")]
    pub toolchains: Vec<ToolchainEntry>,

    /// Absolute path to the directory that contained `natron.toml`. Used to
    /// resolve `settings.deploy_dir` if it is relative. Populated by `load`.
    #[serde(skip)]
    pub config_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Where deployed toolchain dirs land. Relative paths are resolved from
    /// the config file's directory.
    #[serde(default = "default_deploy_dir")]
    pub deploy_dir: PathBuf,

    /// Default deploy mode for toolchains that don't override it.
    #[serde(default)]
    pub deploy_mode: DeployMode,

    /// Override the global cache directory. `~`, `${HOME}`, and `%USERPROFILE%`
    /// are expanded post-parse.
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            deploy_dir: default_deploy_dir(),
            deploy_mode: DeployMode::default(),
            cache_dir: None,
        }
    }
}

fn default_deploy_dir() -> PathBuf {
    PathBuf::from("toolchains")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployMode {
    Symlink,
    Copy,
}

impl Default for DeployMode {
    fn default() -> Self {
        // Symlink is the right default for deployment: instant, atomic
        // version swaps, cross-volume safe, and on Windows we transparently
        // fall back to a directory junction when symlink privilege is
        // missing. Hardlinks within the cache (CAS dedup) are unaffected
        // — that machinery is internal to <cache>/cas/ and <cache>/installs/.
        DeployMode::Symlink
    }
}

impl std::fmt::Display for DeployMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeployMode::Symlink => f.write_str("symlink"),
            DeployMode::Copy => f.write_str("copy"),
        }
    }
}

/// One `[[toolchain]]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolchainEntry {
    /// Unique identifier. Used in state tracking and CLI args.
    pub name: String,
    /// Path under `settings.deploy_dir` where the toolchain is deployed.
    /// Two toolchains may not share the same `deploy_dir`.
    pub deploy_dir: String,
    /// Registered provider id (e.g. "github", "url", "msvc",
    /// "windows_sdk").
    pub provider: String,
    /// Optional per-toolchain override of the deploy mode.
    #[serde(default)]
    pub deploy_mode: Option<DeployMode>,
    /// Hosts this toolchain applies to, as `<os>-<arch>` (e.g.
    /// `"windows-x64"`, `"linux-x64"`, `"macos-arm64"`). Empty means every
    /// host. Entries for other hosts are dropped at load time, so two
    /// entries may share a `deploy_dir` as long as their hosts don't
    /// overlap: e.g. a Windows and a Linux LLVM both deployed to `llvm/`.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Provider-specific options. Validated by the provider.
    #[serde(default)]
    pub options: toml::Table,
}

/// Recognized archive formats. MSVC's `.vsix` and `.msi` payloads are
/// provider-internal and never appear in user config — the `archive` field
/// is only legal for the `github` and `url` providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    TarXz,
    TarGz,
}

impl ArchiveKind {
    /// Parse a config-supplied `archive` string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "zip" => Ok(Self::Zip),
            "tar.xz" => Ok(Self::TarXz),
            "tar.gz" => Ok(Self::TarGz),
            other => bail!(
                "unknown archive kind '{other}' (expected one of: zip, tar.xz, tar.gz)"
            ),
        }
    }

    /// Best-effort inference from a filename or URL path.
    pub fn infer_from_filename(name: &str) -> Option<Self> {
        let lower = name.to_lowercase();
        if lower.ends_with(".tar.xz") {
            Some(Self::TarXz)
        } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Some(Self::TarGz)
        } else if lower.ends_with(".zip") {
            Some(Self::Zip)
        } else {
            None
        }
    }
}

impl Config {
    /// Load + validate. If `path` is a file, load it. If `path` is a directory,
    /// search upward from `path` toward filesystem root for the first
    /// `natron.toml`.
    pub fn load(path: &Path) -> Result<Self> {
        let resolved = if path.is_file() {
            path.to_path_buf()
        } else if path.is_dir() {
            find_upward(path).ok_or_else(|| {
                anyhow!(
                    "no `{CONFIG_FILENAME}` found searching upward from {}",
                    path.display()
                )
            })?
        } else {
            bail!("config path does not exist: {}", path.display());
        };

        let text = std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading {}", resolved.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", resolved.display()))?;

        cfg.config_dir = resolved
            .parent()
            .ok_or_else(|| anyhow!("config has no parent dir: {}", resolved.display()))?
            .to_path_buf();

        cfg.expand_paths()?;
        cfg.validate_hosts()?;
        let host = current_host();
        cfg.toolchains.retain(|t| t.applies_to_host(&host));
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate_hosts(&self) -> Result<()> {
        for entry in &self.toolchains {
            for h in &entry.hosts {
                let ok = matches!(
                    h.split_once('-'),
                    Some(("windows" | "linux" | "macos", "x64" | "arm64"))
                );
                if !ok {
                    bail!(
                        "toolchain '{}': unknown host '{h}' (expected <windows|linux|macos>-<x64|arm64>)",
                        entry.name
                    );
                }
            }
        }
        Ok(())
    }

    /// Path to the deploy dir, resolved against `config_dir`.
    pub fn resolved_deploy_dir(&self) -> PathBuf {
        if self.settings.deploy_dir.is_absolute() {
            self.settings.deploy_dir.clone()
        } else {
            self.config_dir.join(&self.settings.deploy_dir)
        }
    }

    /// Resolve the effective deploy mode for a given entry.
    pub fn effective_mode(&self, entry: &ToolchainEntry) -> DeployMode {
        entry.deploy_mode.unwrap_or(self.settings.deploy_mode)
    }

    fn expand_paths(&mut self) -> Result<()> {
        if let Some(p) = self.settings.cache_dir.take() {
            self.settings.cache_dir = Some(expand_user_path(&p)?);
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        let mut names = HashSet::new();
        let mut deploy_dirs = HashSet::new();
        for entry in &self.toolchains {
            if entry.name.is_empty() {
                bail!("toolchain has empty `name`");
            }
            if entry.deploy_dir.is_empty() {
                bail!("toolchain '{}' has empty `deploy_dir`", entry.name);
            }
            let dd = Path::new(&entry.deploy_dir);
            if dd.is_absolute()
                || dd.has_root()
                || dd.components().any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                bail!(
                    "toolchain '{}': deploy_dir '{}' must be a plain relative path (no `..`, no root)",
                    entry.name, entry.deploy_dir
                );
            }
            if !names.insert(entry.name.clone()) {
                bail!("duplicate toolchain name: '{}'", entry.name);
            }
            if !deploy_dirs.insert(entry.deploy_dir.clone()) {
                bail!(
                    "duplicate deploy_dir: '{}' (used by '{}')",
                    entry.deploy_dir, entry.name
                );
            }
            // Provider id is checked when we look it up in the registry; we
            // can't check here because the registry is constructed by the
            // engine, not the config layer.
            if !is_valid_provider_id(&entry.provider) {
                bail!(
                    "toolchain '{}': invalid provider id '{}' (must be lowercase letters/digits/underscores)",
                    entry.name, entry.provider
                );
            }
        }
        Ok(())
    }
}

impl ToolchainEntry {
    /// True if this entry has no `hosts` filter or lists `host`.
    pub fn applies_to_host(&self, host: &str) -> bool {
        self.hosts.is_empty() || self.hosts.iter().any(|h| h == host)
    }
}

/// The running host as `<os>-<arch>`, matching the `hosts` field.
pub fn current_host() -> String {
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}-{arch}")
}

fn is_valid_provider_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn find_upward(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

/// Expand a leading `~` to the home directory. Nothing else is expanded:
/// environment variables are never read from `natron.toml` (use the
/// `--cache-dir` flag for a per-machine location).
pub fn expand_user_path(p: &Path) -> Result<PathBuf> {
    let s = p
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 path: {}", p.display()))?;
    let expanded = expand_user_str(s)?;
    Ok(PathBuf::from(expanded))
}

fn expand_user_str(s: &str) -> Result<String> {
    if s.contains("${") {
        bail!("environment variables are not expanded in natron.toml: {s:?} (pass --cache-dir instead)");
    }
    match s.strip_prefix('~') {
        Some(rest) => Ok(format!("{}{rest}", home_dir()?.to_str().unwrap_or(""))),
        None => Ok(s.to_string()),
    }
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not determine user home directory"))
}
#[cfg(test)]
#[path = "tests/config.rs"]
mod tests;
