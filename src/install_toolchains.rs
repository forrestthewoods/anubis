//! `anubis install-toolchains`: install every toolchain listed in the
//! repo's `natron.toml` into `toolchains/`, using the natron library.

use anyhow::{bail, Context};
use clap::Parser;
use std::path::Path;

#[derive(Debug, Parser)]
pub struct InstallToolchainsArgs {
    /// Only install the named toolchain(s) from natron.toml.
    #[arg(long)]
    pub only: Vec<String>,

    /// Show what would be installed without downloading anything.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn install_toolchains(args: &InstallToolchainsArgs) -> anyhow::Result<()> {
    let project_root = std::env::current_dir()?;
    let config_path = project_root.join(natron::config::CONFIG_FILENAME);
    if !config_path.is_file() {
        bail!("no {} at {}", natron::config::CONFIG_FILENAME, project_root.display());
    }

    let mut n = natron::Natron::from_config_file(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
    n.options.only = args.only.iter().cloned().collect();
    n.options.dry_run = args.dry_run;

    let report = n.sync()?;
    for entry in &report.entries {
        tracing::info!("{:?} {} {} ({})", entry.action, entry.name, entry.display, entry.mode);
    }
    if !report.errors.is_empty() {
        for e in &report.errors {
            tracing::error!("{}: {}", e.name, e.message);
        }
        bail!("{} toolchain(s) failed to install", report.errors.len());
    }

    // Toolchain bytes changed under the same paths: the persisted file hash
    // cache may hold stale entries for them.
    invalidate_hash_cache(&project_root);
    Ok(())
}

fn invalidate_hash_cache(project_root: &Path) {
    let hash_cache_path = project_root.join(".anubis-out").join("hash_cache.cache");
    if hash_cache_path.exists() {
        tracing::info!("Removing hash disk cache (toolchains changed)");
        let _ = std::fs::remove_file(&hash_cache_path);
    }
}
