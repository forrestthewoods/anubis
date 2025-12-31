#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

mod anubis;
mod cpp_rules;
mod error;
mod install_toolchains;
mod job_system;
mod logging;
mod nasm_rules;
mod papyrus;
mod rule_utils;
mod papyrus_serde;
mod papyrus_tests;
mod toolchain;
mod toolchain_db;
mod util;

use anubis::*;
use anyhow::{anyhow, bail};
use cpp_rules::*;
use dashmap::DashMap;
use install_toolchains::*;
use job_system::*;
use logging::*;
use logos::Logos;
use nasm_rules::*;
use papyrus::*;
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

use clap::{Parser, Subcommand};

// ----------------------------------------------------------------------------
// CLI args
// ----------------------------------------------------------------------------
#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    /// Set the log level (error, warn, info, debug, trace)
    #[arg(short = 'l', long, default_value = "info", global = true)]
    log_level: LogLevel,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Build(BuildArgs),
    InstallToolchains(InstallToolchainsArgs),
}

#[derive(Debug, Parser)]
struct BuildArgs {
    #[arg(short, long)]
    mode: String,

    #[arg(short, long)]
    targets: Vec<String>,

    /// Number of parallel workers (defaults to number of physical CPU cores)
    #[arg(short, long)]
    workers: Option<usize>,
}

// ----------------------------------------------------------------------------
// CLI Commands
// ----------------------------------------------------------------------------
fn build(args: &BuildArgs) -> anyhow::Result<()> {
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

    // Create anubis
    let cwd = std::env::current_dir()?;
    let mut anubis = Arc::new(Anubis::new(cwd.to_owned())?);

    // Build a target!
    let mode = AnubisTarget::new(&args.mode)?;
    let toolchain = AnubisTarget::new("//toolchains:default")?;

    for target in &args.targets {
        let anubis_target = AnubisTarget::new(target)?;

        tracing::info!("Building target: {}", anubis_target.target_path());
        let _build_span = timed_span!(tracing::Level::INFO, "build_execution");
        build_single_target(anubis.clone(), &mode, &toolchain, &anubis_target, args.workers)?;
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
    init_logging(&log_config)?;
    let result = match args.command {
        Commands::InstallToolchains(t) => install_toolchains(&t),
        Commands::Build(b) => build(&b),
    };

    match &result {
        Ok(_) => {}
        Err(e) => {
            tracing::error!("{}", e);
        }
    }

    std::process::exit(if result.is_ok() { 0 } else { -1 })
}
