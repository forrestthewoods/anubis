//! C and C++ rules: `cc_binary`, `cc_static_library`, `cc_shared_library`
//! and `cc_prebuilt_library`.
//!
//! One build path serves all three: compile each source (cached through the
//! `.d` file), then archive, link a shared library, or link an executable.
//! Libraries pass include dirs, defines and libraries on to dependents;
//! executables get the shared libraries they need placed next to them.

#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis::{self, AnubisTarget, JobCacheKey};
use crate::util::normalize_path;
use crate::rules::rule_utils::{ensure_directory, ensure_directory_for_file, run_command_verbose, write_response_file};
use crate::util::{self, SlashFix, format_duration};
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{job_system::*, toolchain};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use indexmap::IndexSet;
use itertools::Itertools;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::papyrus::*;
use crate::toolchain::Toolchain;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};
use crate::{anyhow_with_context, bail_with_context, timed_span};
use serde::{de, Deserializer};

// ----------------------------------------------------------------------------
// Public Enums
// ----------------------------------------------------------------------------
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CcLanguage {
    C,
    Cpp,
}

impl CcLanguage {
    fn file_description(&self) -> &'static str {
        match self {
            CcLanguage::C => "c",
            CcLanguage::Cpp => "cpp",
        }
    }
}

impl<'de> Deserialize<'de> for CcLanguage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "c" => Ok(CcLanguage::C),
            "cpp" => Ok(CcLanguage::Cpp),
            _ => Err(de::Error::custom(format!(
                "unknown language '{}', expected 'c' or 'cpp'",
                s
            ))),
        }
    }
}

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------
/// Unified C/C++ binary rule. Use `cc_binary` in ANUBIS files with an explicit
/// `lang` field set to "c" or "cpp" to select the toolchain.
#[rustfmt::skip]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CcBinary {
    pub name: String,
    pub lang: CcLanguage,
    pub srcs: Vec<Utf8PathBuf>,

    #[serde(default)] pub deps: Vec<AnubisTarget>,
    #[serde(default)] pub compiler_flags: Vec<String>,
    #[serde(default)] pub compiler_defines: Vec<String>,
    #[serde(default)] pub include_dirs: Vec<Utf8PathBuf>,
    #[serde(default)] pub libraries: Vec<Utf8PathBuf>,
    #[serde(default)] pub library_dirs: Vec<Utf8PathBuf>,
    #[serde(default)] pub exe_name: Option<String>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

/// C/C++ library rule, declared as `cc_static_library` or `cc_shared_library`
/// in ANUBIS files (same fields; `kind` records which). `lang` is "c" or "cpp".
///
/// A shared library links its static deps into itself, so dependents link
/// only the shared library (its import lib on Windows), never those deps.
#[rustfmt::skip]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CcLibrary {
    pub name: String,
    pub lang: CcLanguage,
    pub srcs: Vec<Utf8PathBuf>,

    #[serde(default)] pub deps: Vec<AnubisTarget>,

    #[serde(default)] pub public_compiler_flags: Vec<String>,
    #[serde(default)] pub public_defines: Vec<String>,
    #[serde(default)] pub public_include_dirs: Vec<Utf8PathBuf>,
    #[serde(default)] pub public_libraries: Vec<Utf8PathBuf>,
    #[serde(default)] pub public_library_dirs: Vec<Utf8PathBuf>,

    #[serde(default)] pub private_compiler_flags: Vec<String>,
    #[serde(default)] pub private_defines: Vec<String>,
    #[serde(default)] pub private_include_dirs: Vec<Utf8PathBuf>,

    #[serde(skip_deserializing)]
    pub kind: CcLibraryKind,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

/// A library Anubis doesn't build, e.g. the MSVC CRT DLLs or a vendor SDK.
/// Dependents link `libraries` (import libs, static libs, `.so` files),
/// compile with its public include dirs and defines, and get
/// `runtime_files` (`.dll`/`.so`) staged next to their executables, exactly
/// as for a `cc_shared_library`.
#[rustfmt::skip]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CcPrebuiltLibrary {
    pub name: String,
    #[serde(default)] pub libraries: Vec<Utf8PathBuf>,
    #[serde(default)] pub runtime_files: Vec<Utf8PathBuf>,
    #[serde(default)] pub public_include_dirs: Vec<Utf8PathBuf>,
    #[serde(default)] pub public_defines: Vec<String>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CcLibraryKind {
    #[default]
    Static,
    Shared,
}

#[derive(Debug)]
pub struct CcObjectArtifact {
    pub object_path: Utf8PathBuf,
}

#[derive(Debug)]
pub struct CcObjectsArtifact {
    pub object_paths: Vec<Utf8PathBuf>,
}

/// Unified artifact for C/C++ build outputs.
/// Contains the build output along with transitive dependency information.
#[derive(Debug, Clone, Default)]
pub struct CcBuildOutput {
    /// Object files produced (for compile steps)
    pub object_files: Vec<Utf8PathBuf>,

    /// This target's library file (for static library archive steps)
    pub library: Option<Utf8PathBuf>,

    /// Transitive library dependencies (accumulated from deps)
    pub transitive_libraries: Vec<Utf8PathBuf>,

    /// Startup objects (e.g. glibc's `crt1.o`) linked into executables only,
    /// never into shared libraries.
    pub startup_objects: Vec<Utf8PathBuf>,

    /// Shared libraries (`.so`/`.dll`) needed at run time, transitively.
    /// Executables get them copied next to themselves.
    pub runtime_files: Vec<Utf8PathBuf>,
}

// ----------------------------------------------------------------------------
// Private Structs
// ----------------------------------------------------------------------------

/// Marker artifact for the deps blocker job.
/// This job simply waits for all dependencies to complete before allowing
/// source compilation to begin (ensures generated files exist).
#[derive(Debug)]
struct DepsCompleteMarker;

#[derive(Clone, Debug, Default)]
struct CcExtraArgs {
    pub compiler_flags: IndexSet<String>,
    pub defines: IndexSet<String>,
    pub include_dirs: IndexSet<Utf8PathBuf>,
    pub libraries: IndexSet<Utf8PathBuf>,
    pub library_dirs: IndexSet<Utf8PathBuf>,
}

/// Artifact produced when linking an executable
#[derive(Debug)]
pub struct CompileExeArtifact {
    pub output_file: Utf8PathBuf,
}

/// What a C/C++ target produces once its sources are compiled. Everything
/// before that final step (deps, args, compiles) is shared.
#[derive(Clone, Debug)]
enum CcOutputKind {
    Executable,
    StaticLibrary,
    SharedLibrary,
}

/// Inputs to [`build_cc_target`], filled in by each rule type.
struct CcTargetSpec {
    target: AnubisTarget,
    /// Output base name (exe name or library name)
    name: String,
    lang: CcLanguage,
    srcs: Vec<Utf8PathBuf>,
    deps: Vec<AnubisTarget>,
    /// The target's own args, added after its deps' public args.
    own_args: CcExtraArgs,
    kind: CcOutputKind,
}

// ----------------------------------------------------------------------------
// Private Enums
// ----------------------------------------------------------------------------
enum Substep {
    Id(JobId),
    Job(Job),
}

// ----------------------------------------------------------------------------
// Private Traits
// ----------------------------------------------------------------------------
trait CcContextExt<'a> {
    fn get_toolchain(&'a self) -> anyhow::Result<&'a Toolchain>;
    fn get_cc_toolchain(&'a self, lang: CcLanguage) -> anyhow::Result<&'a crate::toolchain::CcToolchain>;
    fn get_args(&self, lang: CcLanguage) -> anyhow::Result<Vec<String>>;
    fn get_compiler(&self, lang: CcLanguage) -> anyhow::Result<&Utf8Path>;
    fn get_linker(&self, lang: CcLanguage) -> anyhow::Result<&Utf8Path>;
    fn get_archiver(&self, lang: CcLanguage) -> anyhow::Result<&Utf8Path>;
}

// ----------------------------------------------------------------------------
// Struct Implementations
// ----------------------------------------------------------------------------
impl CcExtraArgs {
    fn extend_library_public(&mut self, other: &CcLibrary) {
        self.compiler_flags.extend(other.public_compiler_flags.iter().cloned());
        self.defines.extend(other.public_defines.iter().cloned());
        self.include_dirs.extend(other.public_include_dirs.iter().cloned());
        self.libraries.extend(other.public_libraries.iter().cloned());
        self.library_dirs.extend(other.public_library_dirs.iter().cloned());
    }

    fn extend_library_private(&mut self, other: &CcLibrary) {
        self.compiler_flags.extend(other.private_compiler_flags.iter().cloned());
        self.defines.extend(other.private_defines.iter().cloned());
        self.include_dirs.extend(other.private_include_dirs.iter().cloned());
    }

    fn extend(&mut self, other: &CcExtraArgs) {
        self.compiler_flags.extend(other.compiler_flags.iter().cloned());
        self.defines.extend(other.defines.iter().cloned());
        self.include_dirs.extend(other.include_dirs.iter().cloned());
        self.libraries.extend(other.libraries.iter().cloned());
        self.library_dirs.extend(other.library_dirs.iter().cloned());
    }

    fn extend_binary(&mut self, other: &CcBinary) {
        self.compiler_flags.extend(other.compiler_flags.iter().cloned());
        self.defines.extend(other.compiler_defines.iter().cloned());
        self.include_dirs.extend(other.include_dirs.iter().cloned());
        self.libraries.extend(other.libraries.iter().cloned());
        self.library_dirs.extend(other.library_dirs.iter().cloned());
    }
}

// ----------------------------------------------------------------------------
// Trait Implementations
// ----------------------------------------------------------------------------
impl<'a> CcContextExt<'a> for Arc<JobContext> {
    fn get_toolchain(&'a self) -> anyhow::Result<&'a Toolchain> {
        Ok(self.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref())
    }

    fn get_cc_toolchain(&'a self, lang: CcLanguage) -> anyhow::Result<&'a crate::toolchain::CcToolchain> {
        let toolchain = self.get_toolchain()?;
        match lang {
            CcLanguage::C => toolchain.c(),
            CcLanguage::Cpp => toolchain.cpp(),
        }
    }

    fn get_args(&self, lang: CcLanguage) -> anyhow::Result<Vec<String>> {
        let cc_toolchain = self.get_cc_toolchain(lang)?;

        let mut args: Vec<String> = Default::default();
        for flag in &cc_toolchain.compiler_flags {
            args.push(flag.clone());
        }
        for inc_dir in &cc_toolchain.system_include_dirs {
            args.push("-isystem".to_owned());
            args.push(inc_dir.to_string());
        }
        // Library dirs and libraries are link-only; link_exe adds them.
        // Passing them here would also make link-only changes invalidate
        // every cached compile.
        for define in &cc_toolchain.defines {
            args.push(format!("-D{}", define));
        }

        Ok(args)
    }

    fn get_compiler(&self, lang: CcLanguage) -> anyhow::Result<&Utf8Path> {
        let cc_toolchain = self.get_cc_toolchain(lang)?;
        Ok(&cc_toolchain.compiler)
    }

    fn get_linker(&self, lang: CcLanguage) -> anyhow::Result<&Utf8Path> {
        let cc_toolchain = self.get_cc_toolchain(lang)?;
        Ok(&cc_toolchain.linker)
    }

    fn get_archiver(&self, lang: CcLanguage) -> anyhow::Result<&Utf8Path> {
        let cc_toolchain = self.get_cc_toolchain(lang)?;
        Ok(&cc_toolchain.archiver)
    }
}

impl anubis::Rule for CcBinary {
    fn is_runnable(&self) -> bool {
        true
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        bail_loc_if!(ctx.mode.is_none(), "Can not create CcBinary job without a mode");

        let binary = arc_self
            .clone()
            .downcast_arc::<CcBinary>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to CcBinary", arc_self))?;

        let target_name = self.target.target_name().to_string();
        let target_path = self.target.target_path().to_string();
        let mode_name = ctx.as_ref().mode.as_ref().map_or("modeless", |m| &m.name).to_string();
        Ok(ctx.new_job(
            format!("Build CcBinary Target {} with mode {}", &target_path, &mode_name),
            JobDisplayInfo { verb: "Building", short_name: target_name, detail: target_path },
            Box::new(move |job| build_cc_binary(&binary, job)),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        let results : Vec<JobId> = self.deps.iter()  
            .cloned()
            .map(|dep| ctx.anubis.preload_rule(dep, &ctx))
            .collect::<anyhow::Result<Vec<_>, _>>()?;
        Ok(results)
    }
}

impl crate::papyrus::PapyrusObjectType for CcBinary {
    fn name() -> &'static str {
        &"cc_binary"
    }
}

impl anubis::Rule for CcLibrary {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        bail_loc_if!(ctx.mode.is_none(), "Can not create CcLibrary job without a mode");

        let lib = arc_self
            .clone()
            .downcast_arc::<CcLibrary>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to CcLibrary", arc_self))?;

        let target_name = self.target.target_name().to_string();
        let target_path = self.target.target_path().to_string();
        let mode_name = ctx.as_ref().mode.as_ref().map_or("modeless", |m| &m.name).to_string();
        Ok(ctx.new_job(
            format!("Build {:?} CcLibrary Target {} with mode {}", self.kind, &target_path, &mode_name),
            JobDisplayInfo { verb: "Building", short_name: target_name, detail: target_path },
            Box::new(move |job| build_cc_library(&lib, job)),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        let results : Vec<JobId> = self.deps.iter()
            .cloned()
            .map(|dep| ctx.anubis.preload_rule(dep, &ctx))
            .collect::<anyhow::Result<Vec<_>, _>>()?;
        Ok(results)
    }
}

impl anubis::Rule for CcPrebuiltLibrary {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let rule = arc_self
            .clone()
            .downcast_arc::<CcPrebuiltLibrary>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to CcPrebuiltLibrary", arc_self))?;
        let target_name = self.target.target_name().to_string();
        let target_path = self.target.target_path().to_string();
        Ok(ctx.new_job(
            format!("Build CcPrebuiltLibrary Target {}", &target_path),
            JobDisplayInfo { verb: "Checking", short_name: target_name, detail: target_path },
            Box::new(move |job| {
                let missing: Vec<&Utf8PathBuf> =
                    rule.libraries.iter().chain(&rule.runtime_files).filter(|f| !f.is_file()).collect();
                bail_loc_if!(
                    !missing.is_empty(),
                    "cc_prebuilt_library {}: files do not exist:\n  {}",
                    rule.target,
                    missing.iter().join("\n  ")
                );
                job.ctx.anubis.verify_directories(&rule.public_include_dirs, "Include")?;
                Ok(JobOutcome::Success(Arc::new(CcBuildOutput {
                    transitive_libraries: rule.libraries.clone(),
                    runtime_files: rule.runtime_files.clone(),
                    ..Default::default()
                })))
            }),
        ))
    }

    fn preload(&self, _ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        Ok(vec![])
    }
}

impl crate::papyrus::PapyrusObjectType for CcPrebuiltLibrary {
    fn name() -> &'static str {
        "cc_prebuilt_library"
    }
}

fn parse_cc_prebuilt_library(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut rule = CcPrebuiltLibrary::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    rule.target = t;
    Ok(Arc::new(rule))
}

impl JobArtifact for CompileExeArtifact {}
impl JobArtifact for CcObjectArtifact {}
impl JobArtifact for CcObjectsArtifact {}
impl JobArtifact for CcBuildOutput {}
impl JobArtifact for DepsCompleteMarker {}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------
fn parse_cc_binary(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut binary = CcBinary::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    binary.target = t;
    Ok(Arc::new(binary))
}

fn parse_cc_library(t: AnubisTarget, v: &crate::papyrus::Value, kind: CcLibraryKind) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut lib = CcLibrary::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    lib.target = t;
    lib.kind = kind;
    Ok(Arc::new(lib))
}

fn parse_cc_static_library(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    parse_cc_library(t, v, CcLibraryKind::Static)
}

fn parse_cc_shared_library(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    parse_cc_library(t, v, CcLibraryKind::Shared)
}

fn build_cc_binary(binary: &CcBinary, job: Job) -> anyhow::Result<JobOutcome> {
    let mut own_args = CcExtraArgs::default();
    own_args.extend_binary(binary);
    build_cc_target(
        CcTargetSpec {
            target: binary.target.clone(),
            name: binary.exe_name.clone().unwrap_or_else(|| binary.name.clone()),
            lang: binary.lang,
            srcs: binary.srcs.clone(),
            deps: binary.deps.clone(),
            own_args,
            kind: CcOutputKind::Executable,
        },
        job,
    )
}

fn build_cc_library(lib: &CcLibrary, job: Job) -> anyhow::Result<JobOutcome> {
    let mut own_args = CcExtraArgs::default();
    own_args.extend_library_public(lib);
    own_args.extend_library_private(lib);
    build_cc_target(
        CcTargetSpec {
            target: lib.target.clone(),
            name: lib.name.clone(),
            lang: lib.lang,
            srcs: lib.srcs.clone(),
            deps: lib.deps.clone(),
            own_args,
            kind: match lib.kind {
                CcLibraryKind::Static => CcOutputKind::StaticLibrary,
                CcLibraryKind::Shared => CcOutputKind::SharedLibrary,
            },
        },
        job,
    )
}

/// Shared by every C/C++ rule: build deps, compile sources, then defer to a
/// final archive (static library) or link (executable, shared library) step.
fn build_cc_target(spec: CcTargetSpec, job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job
        .ctx
        .mode
        .as_ref()
        .ok_or_else(|| anyhow_loc!("build_cc_target called without a mode. [{}]", spec.target.target_path()))?;
    let lang = spec.lang;
    let cc_toolchain = job.ctx.get_cc_toolchain(lang)?;

    // A C target that links C++ static libraries links as C++, so it gets
    // the C++ runtime those objects need (as clang++ or Bazel would).
    let link_lang = if lang == CcLanguage::C && links_cpp_statically(&job.ctx, mode, &spec.deps)? {
        CcLanguage::Cpp
    } else {
        lang
    };

    // Linked outputs also depend on the toolchain's runtime (libc etc.).
    // A runtime the toolchain builds itself (e.g. a shared libc++) is one
    // of those exe_deps, and can't depend on itself.
    let mut deps = spec.deps.clone();
    if !matches!(spec.kind, CcOutputKind::StaticLibrary) {
        let link_toolchain = job.ctx.get_cc_toolchain(link_lang)?;
        deps.extend(link_toolchain.exe_deps.iter().filter(|dep| **dep != spec.target).cloned());
    }

    // Public args of every library reachable through deps, then our own.
    let mut extra_args = CcExtraArgs::default();
    collect_transitive_public_args(&job.ctx, mode, &deps, &mut extra_args)?;
    extra_args.extend(&spec.own_args);

    let mut child_jobs: Vec<JobId> = Default::default();
    for dep in &deps {
        child_jobs.push(job.ctx.anubis.build_rule(dep, &job.ctx)?);
    }

    job.ctx.anubis.verify_directories(&extra_args.include_dirs, "Include")?;
    job.ctx.anubis.verify_directories(&extra_args.library_dirs, "Library")?;
    job.ctx.anubis.verify_directories(&cc_toolchain.system_include_dirs, "System include")?;
    job.ctx.anubis.verify_directories(&cc_toolchain.library_dirs, "Toolchain library")?;

    for src in &spec.srcs {
        match build_cc_file(src.clone(), &spec.target, job.ctx.clone(), extra_args.clone(), lang)? {
            Substep::Job(child_job) => {
                child_jobs.push(child_job.id);
                job.ctx.job_system.add_job(child_job)?;
            }
            Substep::Id(child_job_id) => child_jobs.push(child_job_id),
        }
    }

    // Continuation: archive or link everything the child jobs produced.
    let (verb, step) = match spec.kind {
        CcOutputKind::StaticLibrary => ("Archiving", "create archive"),
        CcOutputKind::Executable | CcOutputKind::SharedLibrary => ("Linking", "link"),
    };
    let display = JobDisplayInfo {
        verb,
        short_name: spec.name.clone(),
        detail: spec.target.target_path().to_string(),
    };
    let blocked_by = child_jobs.clone();
    let final_step = move |final_job: Job| -> anyhow::Result<JobOutcome> {
        let ctx = final_job.ctx.clone();
        match spec.kind {
            CcOutputKind::StaticLibrary => archive_static_library(&child_jobs, &spec.target, &spec.name, ctx, lang),
            _ => link_cc(&child_jobs, &spec.target, &spec.kind, &spec.name, ctx, &extra_args, link_lang),
        }
    };
    let continuation_job = job.ctx.new_job(format!("{} ({})", job.desc, step), display, Box::new(final_step));

    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by,
        continuation_job,
    }))
}

/// Add the public args (flags, defines, include dirs, libraries) of every
/// `CcLibrary` reachable from `deps`, not just the direct ones, the way
/// CMake's PUBLIC and Bazel's `cc_library` propagate. Pre-order DFS, so a
/// direct dep's args come before those of its own deps; each library is
/// visited once.
/// True if linking `deps` pulls in C++ objects: a C++ static library
/// reachable through static libraries. A shared library brings its own
/// runtime along, so the walk stops there.
fn links_cpp_statically(ctx: &Arc<JobContext>, mode: &crate::toolchain::Mode, deps: &[AnubisTarget]) -> anyhow::Result<bool> {
    let mut visited: HashSet<AnubisTarget> = HashSet::new();
    let mut stack: Vec<AnubisTarget> = deps.to_vec();
    while let Some(dep) = stack.pop() {
        if !visited.insert(dep.clone()) {
            continue;
        }
        if let Ok(lib) = ctx.anubis.get_rule(&dep, mode)?.downcast_arc::<CcLibrary>() {
            if lib.kind == CcLibraryKind::Static {
                if lib.lang == CcLanguage::Cpp {
                    return Ok(true);
                }
                stack.extend(lib.deps.iter().cloned());
            }
        }
    }
    Ok(false)
}

fn collect_transitive_public_args(
    ctx: &Arc<JobContext>,
    mode: &crate::toolchain::Mode,
    deps: &[AnubisTarget],
    extra_args: &mut CcExtraArgs,
) -> anyhow::Result<()> {
    let mut visited: HashSet<AnubisTarget> = HashSet::new();
    let mut stack: Vec<AnubisTarget> = deps.iter().rev().cloned().collect();
    while let Some(dep) = stack.pop() {
        if !visited.insert(dep.clone()) {
            continue;
        }
        let dep_rule = ctx.anubis.get_rule(&dep, mode)?;
        if let Some(lib) = dep_rule.clone().downcast_arc::<CcLibrary>().ok() {
            extra_args.extend_library_public(&lib);
            stack.extend(lib.deps.iter().rev().cloned());
        } else if let Ok(lib) = dep_rule.downcast_arc::<CcPrebuiltLibrary>() {
            extra_args.include_dirs.extend(lib.public_include_dirs.iter().cloned());
            extra_args.defines.extend(lib.public_defines.iter().cloned());
        }
    }
    Ok(())
}

/// What the child jobs of a C/C++ target produced, gathered for the final
/// archive or link step.
#[derive(Default)]
struct CollectedInputs {
    object_files: Vec<Utf8PathBuf>,
    /// Libraries in link order: direct deps first, then theirs.
    libraries: IndexSet<Utf8PathBuf>,
    startup_objects: IndexSet<Utf8PathBuf>,
    runtime_files: IndexSet<Utf8PathBuf>,
}

fn collect_inputs(ctx: &Arc<JobContext>, child_jobs: &[JobId]) -> anyhow::Result<CollectedInputs> {
    let mut c = CollectedInputs::default();
    for job_id in child_jobs {
        let job_result = ctx.job_system.get_result(*job_id)?;
        if let Ok(r) = job_result.cast::<CcBuildOutput>() {
            c.object_files.extend(r.object_files.iter().cloned());
            if let Some(lib) = &r.library {
                c.libraries.insert(lib.clone());
            }
            c.libraries.extend(r.transitive_libraries.iter().cloned());
            c.startup_objects.extend(r.startup_objects.iter().cloned());
            c.runtime_files.extend(r.runtime_files.iter().cloned());
        } else if let Ok(r) = job_result.cast::<CcObjectArtifact>() {
            // A single object or library (nasm_static_library, ifs_stub_library)
            c.libraries.insert(r.object_path.clone());
        } else if let Ok(r) = job_result.cast::<CcObjectsArtifact>() {
            // Multiple objects (nasm_objects)
            c.object_files.extend(r.object_paths.iter().cloned());
        }
    }
    Ok(c)
}

fn build_cc_file(
    src_abspath: Utf8PathBuf,
    target: &AnubisTarget,
    ctx: Arc<JobContext>,
    extra_args: CcExtraArgs,
    lang: CcLanguage,
) -> anyhow::Result<Substep> {
    let config_root = target.get_config_absdir(&ctx.anubis.root);
    let src_relpath = util::strip_prefix(&src_abspath, &config_root)?;

    let src_reldir = crate::parent_dir!(src_relpath)?;
    let src_filename = crate::file_name!(src_abspath)?.to_string();
    let build_dir = ctx.anubis.out_dir(&ctx.mode, &target, "cc_build")
        .join(src_relpath)
        .slash_fix();
    ensure_directory(build_dir.as_ref())?;

    // Scheduling hint key: the build dir already names mode, target and
    // source; make it workspace-relative so it survives moving the checkout.
    let hint_key = format!(
        "cc_compile:{}",
        build_dir.strip_prefix(&ctx.anubis.root).map(|p| p.as_str()).unwrap_or(build_dir.as_str())
    );
    let priority = ctx.anubis.hints.expected_ms(&hint_key);

    let ctx2 = ctx.clone();
    let src_abspath2 = src_abspath.clone();
    let job_fn = move |job| -> anyhow::Result<JobOutcome> {
        let mut args = ctx2.get_args(lang)?;

        args.push("-c".into()); // compile object file, do not link

        for dir in &extra_args.include_dirs {
            args.push(format!("-I{}", dir));
        }

        for flag in &extra_args.compiler_flags {
            args.push(flag.clone());
        }

        for define in &extra_args.defines {
            args.push(format!("-D{}", define));
        }

        let output_file = build_dir.join(&src_filename).with_extension("obj").slash_fix();
        ensure_directory_for_file(output_file.as_ref())?;

        // Add dependency file generation for hermetic validation
        let dep_file = output_file.with_extension("d");
        args.push("-MF".into());
        args.push(dep_file.to_string());

        args.push("-o".into());
        args.push(output_file.to_string());

        if ctx2.anubis.verbose_tools {
            args.push("-v".into()); // verbose
            args.push("-H".into()); // include hierarchy
        }

        args.push(src_abspath2.to_string());

        let compiler = ctx2.get_compiler(lang)?;
        let success = || -> anyhow::Result<JobOutcome> {
            Ok(JobOutcome::Success(Arc::new(CcBuildOutput {
                object_files: vec![output_file.clone()],
                ..Default::default()
            })))
        };

        // Cache lookup. The manifest key covers tool, argv (with this
        // action's output dir abstracted away) and the source; the headers
        // the compile read are checked against the manifest's entries.
        let cache = ctx2.anubis.build_cache.as_ref();
        let manifest_key = match cache {
            Some(cache) => {
                let mut kb = cache.key_for_command("cc_compile", compiler, &args, Some(&build_dir))?;
                if cache.key_add_files(&mut kb, std::slice::from_ref(&src_abspath2))? {
                    Some(kb.finish())
                } else {
                    None // missing source: let the compiler report it
                }
            }
            None => None,
        };
        if let (Some(cache), Some(mk)) = (cache, manifest_key.as_ref()) {
            if let Some(result_key) = cache.lookup_manifest(mk)? {
                if cache.restore(&result_key, &build_dir)? {
                    cache.record_hit();
                    tracing::debug!("cache hit: {output_file}");
                    return success();
                }
            }
            cache.record_miss();
            tracing::debug!("cache miss: {output_file}");
        }

        crate::build_cache::remove_outputs([output_file.as_path()])?;
        let verbose = ctx2.anubis.verbose_tools;
        let (output, compile_duration) = {
            let _span = tracing::info_span!("compile", file = %src_filename).entered();
            let compile_start = std::time::Instant::now();
            let output = run_command_verbose(compiler.as_ref(), &args, verbose)?;
            (output, compile_start.elapsed())
        };

        if output.status.success() {
            ctx2.anubis.hints.record(&hint_key, compile_duration);
            let dep_paths = validate_hermetic_deps(&dep_file, &ctx2.anubis.root)?;

            let cmd_filepath = build_dir.join("command.txt");
            let cmd_str : String = std::iter::once(compiler.as_str())
                .chain(args.iter().map(|a| a.as_str()))
                .join(" ");
            fs::write(cmd_filepath, cmd_str)?;

            if let (Some(cache), Some(mk)) = (cache, manifest_key.as_ref()) {
                // Inputs are hashed after the compile. The source is part of
                // the manifest key, hashed before; if it changed meanwhile,
                // don't cache this result.
                cache.invalidate(&src_abspath2);
                let mut kb = cache.key_for_command("cc_compile", compiler, &args, Some(&build_dir))?;
                let src_unchanged = cache.key_add_files(&mut kb, std::slice::from_ref(&src_abspath2))?
                    && kb.finish() == *mk;
                if src_unchanged {
                    if let Some((result_key, inputs)) = cache.compile_result_key(mk, &dep_paths)? {
                        cache.store(
                            &result_key,
                            &build_dir,
                            &[crate::build_cache::ActionOutput { path: output_file.clone(), optional: false }],
                        )?;
                        cache.record_manifest(mk, inputs, &result_key)?;
                    }
                }
            }

            success()
        } else {
            tracing::error!(
                source_file = %src_filename,
                exit_code = output.status.code(),
                compile_time_ms = compile_duration.as_millis(),
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
    };

    let filename = src_abspath.file_name().unwrap_or(src_abspath.as_str()).to_string();
    let compile_display = JobDisplayInfo {
        verb: "Compiling",
        short_name: filename,
        detail: src_abspath.to_string(),
    };
    Ok(Substep::Job(ctx.new_job(
        format!("Compile {} file [{}]", lang.file_description(), &src_abspath),
        compile_display,
        Box::new(job_fn),
    ).with_priority(priority)))
}

fn archive_static_library(
    child_jobs: &[JobId],
    target: &AnubisTarget,
    name: &str,
    ctx: Arc<JobContext>,
    lang: CcLanguage,
) -> anyhow::Result<JobOutcome> {
    // Objects go in the archive; everything else passes through to dependents.
    let CollectedInputs { object_files, libraries: transitive_libraries, startup_objects, runtime_files } =
        collect_inputs(&ctx, child_jobs)?;

    let mut args: Vec<String> = Default::default();
    if ctx.anubis.verbose_tools {
        args.push("rcsv".to_owned());
    } else {
        args.push("rcs".to_owned());
    }

    let build_dir = ctx.anubis.out_dir(&ctx.mode, target, "lib").slash_fix();
    ensure_directory(build_dir.as_ref())?;

    let output_file = build_dir.join(name).with_extension("lib").slash_fix();
    args.push(output_file.to_string());

    let response_filepath = build_dir.join(name).with_extension("rsp").slash_fix();
    args.push(format!("@{}", response_filepath));

    let archiver = ctx.get_archiver(lang)?;
    let success = || -> anyhow::Result<JobOutcome> {
        Ok(JobOutcome::Success(Arc::new(CcBuildOutput {
            object_files: Vec::new(), // Archive doesn't expose object files
            library: Some(output_file.clone()),
            transitive_libraries: transitive_libraries.iter().cloned().collect(),
            startup_objects: startup_objects.iter().cloned().collect(),
            runtime_files: runtime_files.iter().cloned().collect(),
        })))
    };

    // Cache: archiver + args + the ordered object digests.
    let cache = ctx.anubis.build_cache.as_ref();
    let cache_key = match cache {
        Some(cache) => {
            let mut kb = cache.key_for_command("cc_archive", archiver, &args, Some(&build_dir))?;
            cache.key_add_files(&mut kb, &object_files)?.then(|| kb.finish())
        }
        None => None,
    };
    if let (Some(cache), Some(key)) = (cache, cache_key.as_ref()) {
        if cache.restore(key, &build_dir)? {
            cache.record_hit();
            return success();
        }
        cache.record_miss();
        tracing::debug!("cache miss: {output_file}");
    }

    // Delete existing archive to ensure clean build (llvm-ar updates in place)
    crate::build_cache::remove_outputs([output_file.as_path()])?;

    let link_args_str: String = object_files.iter().map(|p| p.to_string()).join(" ");
    std::fs::write(&response_filepath, &link_args_str).with_context(|| {
        format!(
            "Failed to write link args into response file: [{:?}]",
            response_filepath
        )
    })?;

    let verbose = ctx.anubis.verbose_tools;
    let output = {
        let _span = tracing::info_span!("archive", target = %name).entered();
        run_command_verbose(archiver.as_ref(), &args, verbose)?
    };

    if output.status.success() {
        if let (Some(cache), Some(key)) = (cache, cache_key.as_ref()) {
            cache.store(key, &build_dir, &[crate::build_cache::ActionOutput { path: output_file.clone(), optional: false }])?;
        }
        success()
    } else {
        tracing::error!(
            target = %target.target_path(),
            binary_name = %name,
            exit_code = output.status.code(),
            stdout = %String::from_utf8_lossy(&output.stdout),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "Archive creation failed"
        );

        bail_loc!(
            "Archive command completed with error status [{}].\n  Args: {}\n  stdout: {}\n  stderr: {}",
            output.status,
            args.join(" ") + " " + &link_args_str,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

/// Link an executable or a shared library from the child jobs' outputs.
///
/// Output layout, in the target's `bin` dir:
/// - executable: `<name>` / `<name>.exe` (+ `.pdb`)
/// - Linux shared library: `lib<name>.so` (soname `lib<name>.so`)
/// - Windows shared library: `<name>.dll` + import lib `<name>.lib` (+ `.pdb`)
///
/// Shared libraries an executable needs at run time are copied (hard
/// linked) next to it; on Linux, executables and shared libraries also get
/// `RUNPATH=$ORIGIN` so they find each other there.
fn link_cc(
    child_jobs: &[JobId],
    target: &AnubisTarget,
    kind: &CcOutputKind,
    name: &str,
    ctx: Arc<JobContext>,
    extra_args: &CcExtraArgs,
    lang: CcLanguage,
) -> anyhow::Result<JobOutcome> {
    let shared = matches!(kind, CcOutputKind::SharedLibrary);
    let inputs = collect_inputs(&ctx, child_jobs)?;

    let mode = ctx.mode.as_ref().unwrap();
    let target_platform = mode.vars.get("target_platform").map(|s| s.as_str()).unwrap_or("windows");
    let is_msvc_linker = target_platform == "windows";

    // Build linker-specific arguments (NOT compiler args)
    let mut args: Vec<String> = Vec::new();
    let cc_toolchain = ctx.get_cc_toolchain(lang)?;

    for flag in &cc_toolchain.linker_flags {
        if let Some(stripped) = flag.strip_prefix("-Wl,") {
            // Split comma-separated flags that were passed through -Wl,
            args.extend(stripped.split(',').filter(|p| !p.is_empty()).map(str::to_owned));
        } else if shared && flag.starts_with("--dynamic-linker") {
            // The program interpreter belongs to executables only.
        } else {
            args.push(flag.clone());
        }
    }

    let lib_dir_arg = |dir: &Utf8Path| {
        if is_msvc_linker { format!("/LIBPATH:{}", dir) } else { format!("-L{}", dir) }
    };
    args.extend(cc_toolchain.library_dirs.iter().map(|d| lib_dir_arg(d)));
    args.extend(extra_args.library_dirs.iter().map(|d| lib_dir_arg(d)));

    args.extend(inputs.object_files.iter().map(|p| p.to_string()));
    if !shared {
        args.extend(inputs.startup_objects.iter().map(|p| p.to_string()));
    }
    // Libraries: direct deps + transitive deps in order
    args.extend(inputs.libraries.iter().map(|p| p.to_string()));

    // Libraries by name: toolchain, then target
    for lib in cc_toolchain.libraries.iter().chain(extra_args.libraries.iter()) {
        let lib_str = lib.as_str();
        if !is_msvc_linker {
            args.push(format!("-l{}", lib_str));
        } else if lib_str.ends_with(".lib") {
            args.push(lib_str.to_owned());
        } else {
            args.push(format!("{}.lib", lib_str));
        }
    }

    let bin_dir = ctx.anubis.out_dir(&ctx.mode, target, "bin").slash_fix();
    ensure_directory(bin_dir.as_ref())?;
    let output_file = match (kind, is_msvc_linker) {
        (CcOutputKind::SharedLibrary, true) => bin_dir.join(format!("{name}.dll")),
        (CcOutputKind::SharedLibrary, false) => bin_dir.join(format!("lib{name}.so")),
        (_, true) => bin_dir.join(format!("{name}.exe")),
        (_, false) => bin_dir.join(name),
    };
    // What dependents link against: the .so itself, or the DLL's import lib.
    let link_library = match (shared, is_msvc_linker) {
        (true, true) => Some(output_file.with_extension("lib")),
        (true, false) => Some(output_file.clone()),
        (false, _) => None,
    };

    let mut outputs = vec![crate::build_cache::ActionOutput { path: output_file.clone(), optional: false }];
    if shared {
        if is_msvc_linker {
            let implib = output_file.with_extension("lib");
            args.push("/DLL".into());
            args.push(format!("/IMPLIB:{}", implib));
            outputs.push(crate::build_cache::ActionOutput { path: implib, optional: false });
            outputs.push(crate::build_cache::ActionOutput { path: output_file.with_extension("exp"), optional: true });
        } else {
            args.push("-shared".into());
            args.push(format!("-soname=lib{name}.so"));
        }
    }
    if !is_msvc_linker && (shared || !inputs.runtime_files.is_empty()) {
        args.push("-rpath=$ORIGIN".into());
    }
    if is_msvc_linker {
        outputs.push(crate::build_cache::ActionOutput { path: output_file.with_extension("pdb"), optional: true });
        args.push(format!("/OUT:{}", output_file));
    } else {
        args.push("-o".into());
        args.push(output_file.to_string());
    }

    let finish = || -> anyhow::Result<JobOutcome> {
        if shared {
            let mut runtime_files = vec![output_file.clone()];
            runtime_files.extend(inputs.runtime_files.iter().cloned());
            Ok(JobOutcome::Success(Arc::new(CcBuildOutput {
                library: link_library.clone(),
                runtime_files,
                ..Default::default()
            })))
        } else {
            stage_runtime_files(&ctx, &inputs.runtime_files, &bin_dir)?;
            Ok(JobOutcome::Success(Arc::new(CompileExeArtifact { output_file: output_file.clone() })))
        }
    };

    let linker = ctx.get_linker(lang)?;

    // Cache: linker + args + digests of every input the link can read:
    // explicit object/library paths and libraries found by name in the
    // library search path. The output path stays in the key (no `out:`),
    // because a PE embeds its PDB path.
    let cache = ctx.anubis.build_cache.as_ref();
    let cache_key = match cache {
        Some(cache) => {
            let link_inputs = link_inputs(&args, is_msvc_linker, &output_file);
            let mut kb = cache.key_for_command("cc_link", linker, &args, None)?;
            cache.key_add_files(&mut kb, &link_inputs)?.then(|| kb.finish())
        }
        None => None,
    };
    if let (Some(cache), Some(key)) = (cache, cache_key.as_ref()) {
        if cache.restore(key, &bin_dir)? {
            cache.record_hit();
            return finish();
        }
        cache.record_miss();
        tracing::debug!("cache miss: {output_file}");
    }

    crate::build_cache::remove_outputs(outputs.iter().map(|o| o.path.as_path()))?;
    let verbose = ctx.anubis.verbose_tools;
    // Big links overflow Windows' 32K command line, so pass a response file.
    let response_file = ctx.anubis.out_dir(&ctx.mode, target, "link").join(format!("{name}.rsp"));
    ensure_directory(response_file.parent().unwrap().as_ref())?;
    write_response_file(&response_file, &args, is_msvc_linker)?;
    let (output, link_duration) = {
        let _span = tracing::info_span!("link", target = %name).entered();
        let link_start = std::time::Instant::now();
        let output = run_command_verbose(linker.as_ref(), &[format!("@{response_file}")], verbose)?;
        (output, link_start.elapsed())
    };

    if output.status.success() {
        if let (Some(cache), Some(key)) = (cache, cache_key.as_ref()) {
            cache.store(key, &bin_dir, &outputs)?;
        }
        finish()
    } else {
        tracing::error!(
            target = %target.target_path(),
            binary_name = %name,
            exit_code = output.status.code(),
            link_time_ms = link_duration.as_millis(),
            stdout = %String::from_utf8_lossy(&output.stdout),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "Linking failed"
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

/// Put each runtime shared library next to an executable (hard link, or a
/// copy where that fails), so it runs from its output dir on any platform.
fn stage_runtime_files(ctx: &Arc<JobContext>, files: &IndexSet<Utf8PathBuf>, bin_dir: &Utf8Path) -> anyhow::Result<()> {
    for src in files {
        let file_name = crate::file_name!(src)?;
        let dst = bin_dir.join(file_name);
        if dst == *src {
            continue;
        }
        if same_file(ctx, src, &dst) {
            continue;
        }
        crate::build_cache::remove_outputs([dst.as_path()])?;
        if std::fs::hard_link(src, &dst).is_err() {
            std::fs::copy(src, &dst).with_context(|| format!("copying {src} to {dst}"))?;
        }
        if let Some(cache) = &ctx.anubis.build_cache {
            cache.invalidate(&dst);
        }
    }
    Ok(())
}

/// True when `a` and `b` are already the same file (hard links), or have
/// the same contents, so staging can skip them. Contents come from the
/// build cache's stat-cached digests, so a no-op build doesn't reread every
/// staged DLL.
fn same_file(ctx: &JobContext, a: &Utf8Path, b: &Utf8Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else { return false };
    if ma.len() != mb.len() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if ma.dev() == mb.dev() && ma.ino() == mb.ino() {
            return true;
        }
    }
    if let Some(cache) = &ctx.anubis.build_cache {
        return matches!((cache.file_digest(a), cache.file_digest(b)), (Ok(Some(x)), Ok(Some(y))) if x == y);
    }
    matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// Files a link reads, in command-line order: every argument that names an
/// existing file, plus libraries given by name (`foo.lib`, `-lfoo`)
/// resolved against the library search path the way the linker would.
/// The output itself is excluded.
fn link_inputs(args: &[String], is_msvc_linker: bool, output_file: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut lib_dirs: Vec<Utf8PathBuf> = Vec::new();
    for a in args {
        let dir = if is_msvc_linker {
            a.strip_prefix("/LIBPATH:").or_else(|| a.strip_prefix("-LIBPATH:"))
        } else {
            a.strip_prefix("-L")
        };
        if let Some(d) = dir {
            lib_dirs.push(Utf8PathBuf::from(d));
        }
    }
    let find = |names: &[String]| -> Option<Utf8PathBuf> {
        lib_dirs.iter().find_map(|d| {
            names.iter().map(|n| d.join(n)).find(|p| p.is_file())
        })
    };

    let mut inputs = Vec::new();
    for a in args {
        let p = Utf8Path::new(a);
        if p == output_file {
            continue;
        } else if p.is_absolute() && p.is_file() {
            inputs.push(p.to_owned());
        } else if is_msvc_linker && !a.starts_with('/') && !a.starts_with('-') && a.to_ascii_lowercase().ends_with(".lib") {
            if let Some(found) = find(&[a.clone(), a.to_ascii_lowercase()]) {
                inputs.push(found);
            }
        } else if let Some(name) = a.strip_prefix("-l").filter(|_| !is_msvc_linker) {
            if let Some(found) = find(&[format!("lib{name}.so"), format!("lib{name}.a")]) {
                inputs.push(found);
            }
        }
    }
    inputs
}

/// Validates that all dependencies listed in a Makefile-style .d file are
/// located under the Anubis root directory. This ensures hermetic builds
/// with no accidental system header dependencies.
///
/// Note: We intentionally do NOT canonicalize dependency paths because that
/// would resolve symlinks to their real locations. A symlink inside the Anubis
/// root pointing elsewhere is a deliberate developer choice and should be allowed.
pub(crate) fn validate_hermetic_deps(dep_file: &Utf8Path, anubis_root: &Utf8Path) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let content = std::fs::read_to_string(dep_file)
        .with_context(|| format!("Failed to read dependency file: {:?}", dep_file))?;

    // Normalize root path for comparison (forward slashes, lowercase on Windows)
    let root_normalized = normalize_path(anubis_root);

    let mut dep_paths = Vec::new();
    let mut violations = Vec::new();

    for dep in crate::build_cache::parse_depfile(&content) {
        let dep_path = Utf8PathBuf::from(dep);
        let dep_normalized = normalize_path(&dep_path);

        if dep_normalized.starts_with(&root_normalized) {
            dep_paths.push(dep_normalized);
        } else {
            violations.push(dep_path);
        }
    }

    if !violations.is_empty() {
        bail_loc!(
            "External dependencies detected (files outside Anubis root):\n  {}",
            violations
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }

    Ok(dep_paths)
}

// ----------------------------------------------------------------------------
// Public functions
// ----------------------------------------------------------------------------
pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("cc_binary".to_owned()),
        parse_rule: parse_cc_binary,
    })?;

    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("cc_static_library".to_owned()),
        parse_rule: parse_cc_static_library,
    })?;

    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("cc_shared_library".to_owned()),
        parse_rule: parse_cc_shared_library,
    })?;

    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("cc_prebuilt_library".to_owned()),
        parse_rule: parse_cc_prebuilt_library,
    })?;

    Ok(())
}
