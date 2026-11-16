//! `anubis install-toolchains`: install every toolchain listed in the
//! repo's `natron.toml` into `toolchains/`, using the natron library.

use anyhow::{bail, Context};
use clap::Parser;

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
    let anubis_root_file = crate::anubis::find_anubis_root(&std::env::current_dir()?)?;
    let project_root = anubis_root_file.parent().context("no parent for .anubis_root")?;
    let config_path = project_root.join(natron::config::CONFIG_FILENAME);
    if !config_path.is_file() {
        bail!("no {} at {}", natron::config::CONFIG_FILENAME, project_root);
    }

    let mut n = natron::Natron::from_config_file(config_path.as_std_path())
        .with_context(|| format!("loading {config_path}"))?;
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
    Ok(())
}
