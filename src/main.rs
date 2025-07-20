#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

mod anubis;
mod cpp_rules;
mod error;
mod job_system;
mod logging;
mod papyrus;
mod papyrus_serde;
mod papyrus_tests;
mod toolchain;
mod util;

use anubis::*;
use anyhow::{anyhow, bail};
use cpp_rules::*;
use logging::*;
use dashmap::DashMap;
use job_system::*;
use logos::Logos;
use papyrus::*;
use serde::Deserialize;
use std::any;
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::*;
use std::sync::{Arc, Mutex};
use toolchain::*;

fn main() -> anyhow::Result<()> {
    // Initialize logging system
    let log_config = LogConfig {
        level: LogLevel::Info,
        format: LogFormat::Simple,
        output: LogOutput::Stdout,
        enable_timing: true,
        enable_spans: true,
    };
    init_logging(&log_config)?;
    
    tracing::info!("Starting Anubis build system");
    
    // Nuke environment
    let keys: Vec<_> = std::env::vars_os().map(|(key, _)| key).collect();
    for key in keys {
        if let Some(key_str) = key.to_str() {
            std::env::remove_var(key_str);
        }
    }

    // Create anubis
    let cwd = std::env::current_dir()?;
    let mut anubis = Arc::new(Anubis::new(cwd.to_owned()));

    // Initialize anubis with language rules
    tracing::debug!("Registering language rule type infos");
    cpp_rules::register_rule_typeinfos(anubis.clone())?;

    // Build a target!
    let mode = AnubisTarget::new("//mode:linux_dev")?;
    let toolchain = AnubisTarget::new("//toolchains:default")?;
    let target = AnubisTarget::new("//examples/hello_world:hello_world")?;
    
    tracing::info!("Building target: {}", target.target_path());
    let _build_span = timed_span!(tracing::Level::INFO, "build_execution");
    build_single_target(anubis, &mode, &toolchain, &target)?;
    tracing::info!("Build completed successfully");
    
    Ok(())
}
