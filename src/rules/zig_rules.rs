//! Zig-related build rules for extracting libc and runtime libraries.
//!
//! This module provides rules for extracting Zig's bundled libc and runtime
//! libraries for cross-compilation scenarios.

use crate::anubis::{self, AnubisTarget};
use crate::cc_rules;
use crate::job_system::*;
use crate::rules::{CcBuildOutput, CcLanguage, rule_utils};
use crate::rules::rule_utils::{ensure_directory, run_command};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{anyhow_loc, bail_loc, function_name};
use anyhow::Context;
use rusqlite::ffi::SQLITE_DBCONFIG_DEFENSIVE;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;


// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZigGlibc {
    pub name: String,
    pub target_triple: String,      
    pub glibc_version: String,
    pub expected_link_args: Vec<String>,
    pub lang: cc_rules::CcLanguage,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

fn default_lang() -> String {
    "c".to_string()
}

// ----------------------------------------------------------------------------
// Trait Implementations
// ----------------------------------------------------------------------------
impl crate::papyrus::PapyrusObjectType for ZigGlibc {
    fn name() -> &'static str {
        "zig_glibc"
    }
}

impl anubis::Rule for ZigGlibc {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let zig_libc = arc_self
            .clone()
            .downcast_arc::<ZigGlibc>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to ZigGlibc", arc_self))?;

        Ok(ctx.new_job(
            format!("Build ZigGlibc: {}", self.target),
            Box::new(move |job| build_zig_glibc(zig_libc.clone(), job)),
        ))
    }
}


// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------
fn parse_zig_glibc(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut zig_glibc = ZigGlibc::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    zig_glibc.target = t;
    Ok(Arc::new(zig_glibc))
}

fn build_zig_glibc(zig_glibc: Arc<ZigGlibc>, job: Job) -> anyhow::Result<JobOutcome> {
    // setup
    let mode = job.ctx.mode.as_ref().ok_or_else(|| anyhow_loc!("No mode specified"))?;
    let toolchain = job.ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref();
    
    let build_dir = job
        .ctx
        .anubis
        .build_dir(&mode.name)
        .join("zig_glibc")
        .join(&zig_glibc.target_triple)
        .join(format!("{:?}", zig_glibc.lang));
    ensure_directory(&build_dir)?;
    
    let temp_dir = job
        .ctx
        .anubis
        .temp_dir()
        .join(&mode.name)
        .join("zig_glibc")
        .join(&zig_glibc.target_triple)
        .join(format!("{:?}", zig_glibc.lang));
    ensure_directory(&temp_dir)?;

    // ex: x86_64-linux-gnu.2.28
    let full_target_triple = format!("{}.{}", &zig_glibc.target_triple, &zig_glibc.glibc_version);

    // Create stub source file
    let (src_file, zig_cmd) = match zig_glibc.lang {
        CcLanguage::C => {
            let src_path = temp_dir.join("dummy.c");
            std::fs::write(&src_path, "int main() { return 0; }\n")
                .with_context(|| format!("Failed to write dummy source: {:?}", src_path))?;
            (src_path, "cc".to_owned())
        },
        CcLanguage::Cpp => {
            let src_path = temp_dir.join("dummy.cpp");
            std::fs::write(&src_path, "int main() { return 0; }\n")
                .with_context(|| format!("Failed to write dummy source: {:?}", src_path))?;
            (src_path, "c++".to_owned())
        },
    };
    
    // Compile stub file
    let dummy_bin_name = "dummy_exe";
    let args : Vec<String> = vec![
        "build-exe".into(),
        "--global-cache-dir".into(),
        build_dir.join("zig").to_string_lossy().into(),
        "-target".into(),
        full_target_triple,
        "--verbose-link".into(),
        "-lc".into(),
        format!("-femit-bin={}", temp_dir.join(dummy_bin_name).to_string_lossy()),
        src_file.to_string_lossy().into(),
    ];

    let output = rule_utils::run_command(&toolchain.zig.compiler, &args)?;

    if output.status.success() {
        // zig emits all logs to stderr
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lines = stderr.lines().rev();

        // find the final linker line
        let linker_cmd = stderr.lines().rev().find(|l| l.contains(dummy_bin_name))
            .ok_or_else(|| anyhow_loc!("Failed to find binname [{}] in in zig linker output: [{}]", dummy_bin_name, stderr))?;

        // split linker line into parts
        let linker_parts : Vec<&str> = linker_cmd.split_ascii_whitespace().collect();
        let link_args : Vec<PathBuf> = linker_parts.iter()
            .filter(|part| zig_glibc.expected_link_args.iter().any(|arg| part.rfind(arg).is_some()))
            .map(|part| PathBuf::from(part))
            .collect();

        Ok(JobOutcome::Success(Arc::new(CcBuildOutput {
            object_files: Vec::new(),
            library: None,
            transitive_libraries: link_args
        })))
    } else {
        tracing::error!(
            exit_code = output.status.code(),
            stdout = %String::from_utf8_lossy(&output.stdout),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "Compilation failed"
        );

        bail_loc!(
            "Command completed with error status [{}].\n  Args: {}\n  stdout: {}\n  stderr: {}",
            output.status,
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

// ----------------------------------------------------------------------------
// Public Functions
// ----------------------------------------------------------------------------

/// Registers zig rule types with Anubis.
pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("zig_glibc".to_owned()),
        parse_rule: parse_zig_glibc,
    })?;

    Ok(())
}
