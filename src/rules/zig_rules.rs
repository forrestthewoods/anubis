//! Zig-related build rules for extracting libc and runtime libraries.
//!
//! This module provides rules for extracting Zig's bundled libc and runtime
//! libraries for cross-compilation scenarios.

use crate::anubis::{self, AnubisTarget};
use crate::job_system::*;
use crate::rules::rule_utils::ensure_directory;
use crate::util::SlashFix;
use crate::zig::{extract_zig_libc, get_link_libraries, ZigLibcArtifacts, ZigLibcConfig};
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{anyhow_loc, bail_loc, function_name};
use anyhow::Context;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------

/// Rule for extracting Zig's libc and runtime libraries for a target platform.
///
/// Example usage in ANUBIS files:
/// ```papyrus
/// zig_libc(
///     name = "linux_libc",
///     target = "x86_64-linux-gnu",
///     lang = "c++",
///     glibc_version = "2.28",  # optional
///     zig_exe = RelPath("zig/0.15.2/bin/windows_x64/zig.exe"),
/// )
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZigLibc {
    /// Name of this rule target
    pub name: String,

    /// Target triple (e.g., "x86_64-linux-gnu", "aarch64-linux-gnu")
    pub target: String,

    /// Language: "c" or "c++" (determines which runtime libraries to extract)
    #[serde(default = "default_lang")]
    pub lang: String,

    /// Optional glibc version to target (e.g., "2.28")
    #[serde(default)]
    pub glibc_version: Option<String>,

    /// Path to the Zig executable
    pub zig_exe: PathBuf,

    #[serde(skip_deserializing)]
    anubis_target: anubis::AnubisTarget,
}

fn default_lang() -> String {
    "c".to_string()
}

/// Artifact produced by the zig_libc rule containing paths to extracted libraries.
#[derive(Debug)]
pub struct ZigLibcArtifact {
    /// All library paths in correct link order
    pub libraries: Vec<PathBuf>,
    /// The extracted artifacts structure
    pub artifacts: ZigLibcArtifacts,
}

// ----------------------------------------------------------------------------
// Trait Implementations
// ----------------------------------------------------------------------------

impl crate::papyrus::PapyrusObjectType for ZigLibc {
    fn name() -> &'static str {
        "zig_libc"
    }
}

impl anubis::Rule for ZigLibc {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.anubis_target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let zig_libc = arc_self
            .clone()
            .downcast_arc::<ZigLibc>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to ZigLibc", arc_self))?;

        Ok(ctx.new_job(
            format!("Extract Zig libc for {}", self.target),
            Box::new(move |job| build_zig_libc(zig_libc.clone(), job)),
        ))
    }
}

impl JobArtifact for ZigLibcArtifact {}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------

fn parse_zig_libc(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut zig_libc = ZigLibc::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    zig_libc.anubis_target = t;
    Ok(Arc::new(zig_libc))
}

fn build_zig_libc(zig_libc: Arc<ZigLibc>, job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job
        .ctx
        .mode
        .as_ref()
        .ok_or_else(|| anyhow_loc!("No mode specified"))?;

    // Create cache directory for extracted libraries
    // Structure: .anubis-build/{mode}/zig_libc/{target}/{lang}/
    let cache_dir = job
        .ctx
        .anubis
        .build_dir(&mode.name)
        .join("zig_libc")
        .join(&zig_libc.target)
        .join(&zig_libc.lang);

    ensure_directory(&cache_dir)?;

    // Check if we already have cached artifacts
    let libs_dir = cache_dir.join("libs");
    let marker_file = cache_dir.join(".extracted");

    if marker_file.exists() && libs_dir.exists() {
        tracing::info!("Using cached Zig libc artifacts from {:?}", libs_dir);
        let artifacts = load_cached_artifacts(&libs_dir)?;
        let libraries = get_link_libraries(&artifacts);

        return Ok(JobOutcome::Success(Arc::new(ZigLibcArtifact {
            libraries,
            artifacts,
        })));
    }

    // Extract Zig libc
    tracing::info!(
        "Extracting Zig libc for target {} (lang: {})",
        zig_libc.target,
        zig_libc.lang
    );

    let config = ZigLibcConfig {
        zig_exe: zig_libc.zig_exe.clone(),
        target: zig_libc.target.clone(),
        lang: zig_libc.lang.clone(),
        glibc_version: zig_libc.glibc_version.clone(),
        cache_dir: cache_dir.clone(),
    };

    let artifacts = extract_zig_libc(&config).with_context(|| {
        format!(
            "Failed to extract Zig libc for target {} with zig at {:?}",
            zig_libc.target, zig_libc.zig_exe
        )
    })?;

    // Get libraries in correct link order
    let libraries = get_link_libraries(&artifacts);

    // Log what we found
    tracing::debug!("Extracted {} startup objects", artifacts.startup_objects.len());
    tracing::debug!("Extracted {} static libs", artifacts.static_libs.len());
    tracing::debug!("Extracted {} shared libs", artifacts.shared_libs.len());
    if artifacts.compiler_rt.is_some() {
        tracing::debug!("Found compiler_rt");
    }

    // Create marker file to indicate successful extraction
    std::fs::write(&marker_file, "").ok();

    Ok(JobOutcome::Success(Arc::new(ZigLibcArtifact {
        libraries,
        artifacts,
    })))
}

/// Loads cached artifacts from the libs directory.
fn load_cached_artifacts(libs_dir: &std::path::Path) -> anyhow::Result<ZigLibcArtifacts> {
    let mut artifacts = ZigLibcArtifacts::default();

    if !libs_dir.exists() {
        return Ok(artifacts);
    }

    for entry in std::fs::read_dir(libs_dir)? {
        let entry = entry?;
        let path = entry.path().slash_fix();

        if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
            crate::zig::categorize_lib_file(&path, filename, &mut artifacts);
        }
    }

    Ok(artifacts)
}

// ----------------------------------------------------------------------------
// Public Functions
// ----------------------------------------------------------------------------

/// Registers zig rule types with Anubis.
pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("zig_libc".to_owned()),
        parse_rule: parse_zig_libc,
    })?;

    Ok(())
}
