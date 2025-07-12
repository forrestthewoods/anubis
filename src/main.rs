#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

mod anubis;
mod cpp_rules;
mod error;
mod job_system;
mod papyrus;
mod papyrus_serde;
mod papyrus_tests;
mod toolchain;
mod util;

use anubis::*;
use anyhow::{anyhow, bail};
use cpp_rules::*;
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
    // Could someday be via dynamic libs
    cpp_rules::register_rule_typeinfos(anubis.clone())?;

    // Build a target!
    //build_target(&anubis, &Path::new("//examples/hello_world:hello_world"))

    let mode = AnubisTarget::new("//mode:linux_dev")?;
    //let mode = AnubisTarget::new("//mode:win_dev")?;
    let toolchain = AnubisTarget::new("//toolchains:default")?;
    let target = AnubisTarget::new("//examples/hello_world:hello_world")?;
    //let target = AnubisTarget::new("//toolchains:glibc")?;
    build_single_target(anubis, &mode, &toolchain, &target)?;

    Ok(())
}
