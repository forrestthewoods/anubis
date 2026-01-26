#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

mod anubis;
mod error;
mod install_toolchains;
mod job_system;
mod logging;
mod papyrus;
mod papyrus_serde;
mod rules;
mod toolchain;
mod toolchain_db;
mod util;

#[cfg(test)]
mod anubis_tests;
#[cfg(test)]
mod job_system_tests;
#[cfg(test)]
mod papyrus_tests;
#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod util_tests;

use anubis::*;
use dashmap::DashMap;
use install_toolchains::*;
use job_system::*;
use logging::*;
use logos::Logos;
use papyrus::*;
use rules::*;
use serde::Deserialize;
use std::any;
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::*;
use std::sync::{Arc, Mutex};
use toolchain::*;
use util::SlashFix;

use clap::{Parser, Subcommand};

// ----------------------------------------------------------------------------
// CLI args
// ----------------------------------------------------------------------------
#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    /// Set the log level (error, warn, info, debug, trace, fullverbose).
    /// 'fullverbose' enables trace logging AND verbose output from external tools (e.g., clang -v).
    #[arg(short = 'l', long, default_value = "info", global = true)]
    log_level: LogLevel,

    /// Number of parallel workers (defaults to number of physical CPU cores)
    #[arg(short, long, global = true)]
    workers: Option<usize>,

    /// Enable profiling and write trace to specified file (viewable in Firefox Profiler, chrome://tracing, or Perfetto)
    #[arg(short = 'p', long, global = true)]
    profile: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Build(BuildArgs),
    Dump(DumpArgs),
    Run(RunArgs),
    InstallToolchains(InstallToolchainsArgs),
}

#[derive(Debug, Parser)]
struct RunArgs {
    #[arg(short, long)]
    mode: String,

    #[arg(short, long)]
    target: String,

    /// Arguments to pass to the executable
    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Debug, Parser)]
struct BuildArgs {
    #[arg(short, long)]
    mode: String,

    #[arg(short, long, required = true, visible_alias = "target", num_args = 1..)]
    targets: Vec<String>,
}

#[derive(Debug, Parser)]
struct DumpArgs {
    /// Mode target (e.g., //mode:win_dev)
    #[arg(short, long)]
    mode: String,

    /// Target to dump (e.g., //toolchains:default)
    #[arg(short, long)]
    target: String,
}

// ----------------------------------------------------------------------------
// CLI Commands
// ----------------------------------------------------------------------------
fn dump(args: &DumpArgs, verbose_tools: bool) -> anyhow::Result<()> {
    tracing::info!("Dumping target: {} with mode: {}", args.target, args.mode);

    // Find the project root
    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_path_buf();

    // Create anubis
    let anubis = Anubis::new(project_root, verbose_tools)?;

    // Parse mode and target
    let mode_target = AnubisTarget::new(&args.mode)?;
    let target = AnubisTarget::new(&args.target)?;

    // Get mode
    let mode = anubis.get_mode(&mode_target)?;

    // Get resolved config for the target
    let config_relpath = target.get_config_relpath();
    let resolved_config = anubis.get_resolved_config(&config_relpath, &mode)?;

    // Find the specific target within the config
    let target_value = resolved_config.get_named_object(target.target_name())?;

    // Format and print the resolved target
    println!("# Resolved target: {} with mode: {}", args.target, args.mode);
    println!("# Mode variables:");
    for (k, v) in mode.vars.iter() {
        println!("#   {} = {}", k, v);
    }
    println!();
    println!("{}", papyrus::format_value(target_value, 0));

    Ok(())
}

fn build(args: &BuildArgs, workers: Option<usize>, verbose_tools: bool) -> anyhow::Result<()> {
    tracing::info!("Starting Anubis build command: [{:?}]", args);

    // Nuke environment variables to ensure clean build environment
    let keys: Vec<_> = std::env::vars_os().map(|(key, _)| key).collect();
    for key in keys {
        if let Some(key_str) = key.to_str() {
            // Skip RUST_ macros (such as RUST_BACKTRACE)
            if key_str.contains("RUST_") {
                continue;
            }

            std::env::remove_var(key_str);
        }
    }

    // Find the project root by looking for .anubis_root file
    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_path_buf();
    tracing::debug!("Found project root: {:?}", project_root);

    // Create anubis with the discovered project root
    let anubis = Arc::new(Anubis::new(project_root.clone(), verbose_tools)?);

    // Expand any target patterns (e.g., "//samples/basic/..." -> all targets under samples/basic/)
    let expanded_targets = expand_targets(&args.targets, &project_root, &anubis.rule_typeinfos)?;

    if expanded_targets.is_empty() {
        tracing::warn!("No targets to build");
        return Ok(());
    }

    tracing::info!(
        "Building {} target(s): {:?}",
        expanded_targets.len(),
        expanded_targets
    );

    // Parse target paths
    let mode = AnubisTarget::new(&args.mode)?;
    let toolchain = AnubisTarget::new("//toolchains:default")?;

    let anubis_targets: Vec<AnubisTarget> =
        expanded_targets.iter().map(|t| AnubisTarget::new(t)).collect::<anyhow::Result<Vec<_>>>()?;

    for target in &anubis_targets {
        tracing::info!("Building target: {}", target.target_path());
    }

    // Build all targets together with a shared JobSystem
    // This ensures job caches remain valid (job IDs are per-JobSystem)
    let _build_span = timed_span!(tracing::Level::INFO, "build_execution");
    let _ = build_targets(anubis, &mode, &toolchain, &anubis_targets, workers)?;

    Ok(())
}

/// Expand target patterns into concrete target paths.
///
/// Target patterns like "//samples/basic/..." are expanded to all targets
/// found in ANUBIS files under the specified directory.
/// Regular targets are passed through unchanged.
fn expand_targets(
    targets: &[String],
    project_root: &Path,
    rule_typeinfos: &anubis::SharedHashMap<anubis::RuleTypename, anubis::RuleTypeInfo>,
) -> anyhow::Result<Vec<String>> {
    let mut result = Vec::new();

    for target in targets {
        if let Some(pattern) = anubis::TargetPattern::parse(target) {
            // This is a pattern - expand it
            let expanded = anubis::expand_target_pattern(project_root, &pattern, rule_typeinfos)?;
            tracing::debug!("Expanded pattern '{}' to {} targets", target, expanded.len());
            result.extend(expanded);
        } else {
            // Regular target - pass through
            result.push(target.clone());
        }
    }

    Ok(result)
}

fn run(args: &RunArgs, workers: Option<usize>, verbose_tools: bool) -> anyhow::Result<()> {
    tracing::info!("Starting Anubis run command: [{:?}]", args);

    // Nuke environment variables to ensure clean build environment
    let keys: Vec<_> = std::env::vars_os().map(|(key, _)| key).collect();
    for key in keys {
        if let Some(key_str) = key.to_str() {
            // Skip RUST_ macros (such as RUST_BACKTRACE)
            if key_str.contains("RUST_") {
                continue;
            }

            std::env::remove_var(key_str);
        }
    }

    // Find the project root by looking for .anubis_root file
    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_path_buf();
    tracing::debug!("Found project root: {:?}", project_root);

    // Create anubis with the discovered project root
    let anubis = Arc::new(Anubis::new(project_root.clone(), verbose_tools)?);

    // Build the target
    let mode = AnubisTarget::new(&args.mode)?;
    let toolchain = AnubisTarget::new("//toolchains:default")?;
    let anubis_target = AnubisTarget::new(&args.target)?;

    tracing::info!("Building target: {}", anubis_target.target_path());
    let artifact = {
        let _build_span = timed_span!(tracing::Level::INFO, "build_execution");
        build_single_target(anubis.clone(), &mode, &toolchain, &anubis_target, workers)?
    };

    // Get the executable path from the build artifact
    let exe_artifact = artifact.downcast_arc::<CompileExeArtifact>().map_err(|_| {
        anyhow_loc!(
            "Target '{}' is not an executable. The 'run' command only works with cpp_binary targets.",
            args.target
        )
    })?;

    // Normalize the path for cross-platform compatibility
    let exe_path = exe_artifact.output_file.clone().slash_fix();

    tracing::info!("Running executable: {:?}", exe_path);

    // Verify the executable exists
    if !exe_path.exists() {
        bail_loc!("Executable not found at {:?}. Build may have failed.", exe_path);
    }

    // Run the executable
    let status = std::process::Command::new(&exe_path).args(&args.args).status()?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        tracing::warn!("Executable exited with code: {}", code);
        std::process::exit(code);
    }

    Ok(())
}

// ----------------------------------------------------------------------------
// Main
// ----------------------------------------------------------------------------
fn main() -> anyhow::Result<()> {
    // Parse command-line arguments first to get the log level
    let args = Args::parse();

    // Initialize logging system with the specified log level
    let log_config = LogConfig {
        level: args.log_level,
        format: LogFormat::Simple,
        output: LogOutput::Stdout,
        enable_timing: true,
        enable_spans: true,
    };

    // Hold the profile guard for the duration of the program (if profiling)
    let _profile_guard = if let Some(ref profile_path) = args.profile {
        Some(logging::init_logging_with_profile(&log_config, profile_path)?)
    } else {
        init_logging(&log_config)?;
        None
    };

    // Determine if we should enable verbose output from external tools
    let verbose_tools = args.log_level.is_verbose_tools();

    let result = match args.command {
        Commands::Build(b) => build(&b, args.workers, verbose_tools),
        Commands::Dump(d) => dump(&d, verbose_tools),
        Commands::Run(r) => run(&r, args.workers, verbose_tools),
        Commands::InstallToolchains(t) => install_toolchains(&t),
    };

    match &result {
        Ok(_) => {}
        Err(e) => {
            tracing::error!("{}", e);
        }
    }

    // Explicitly drop the profile guard to ensure the trace file is flushed
    // before we exit (std::process::exit doesn't run destructors)
    drop(_profile_guard);

    std::process::exit(if result.is_ok() { 0 } else { -1 })
}
