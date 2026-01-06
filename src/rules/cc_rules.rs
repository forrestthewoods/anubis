#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis::{self, AnubisTarget, JobCacheKey, RuleExt};
use crate::{job_system::*, toolchain};
use crate::rules::rule_utils::{ensure_directory, ensure_directory_for_file, run_command_verbose};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use anyhow::Context;
use indexmap::IndexSet;
use itertools::Itertools;
use serde::Deserialize;
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
            "c"  => Ok(CcLanguage::C),
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
    pub srcs: Vec<PathBuf>,

    #[serde(default)] pub deps: Vec<AnubisTarget>,
    #[serde(default)] pub compiler_flags: Vec<String>,
    #[serde(default)] pub compiler_defines: Vec<String>,
    #[serde(default)] pub include_dirs: Vec<PathBuf>,
    #[serde(default)] pub libraries: Vec<PathBuf>,
    #[serde(default)] pub library_dirs: Vec<PathBuf>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

/// Unified C/C++ static library rule. Use `cc_static_library` in ANUBIS files with
/// an explicit `lang` field set to "c" or "cpp" to select the toolchain.
#[rustfmt::skip]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CcStaticLibrary {
    pub name: String,
    pub lang: CcLanguage,
    pub srcs: Vec<PathBuf>,

    #[serde(default)] pub deps: Vec<AnubisTarget>,

    #[serde(default)] pub public_compiler_flags: Vec<String>,
    #[serde(default)] pub public_defines: Vec<String>,
    #[serde(default)] pub public_include_dirs: Vec<PathBuf>,
    #[serde(default)] pub public_libraries: Vec<PathBuf>,
    #[serde(default)] pub public_library_dirs: Vec<PathBuf>,

    #[serde(default)] pub private_compiler_flags: Vec<String>,
    #[serde(default)] pub private_defines: Vec<String>,
    #[serde(default)] pub private_include_dirs: Vec<PathBuf>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

#[derive(Debug)]
pub struct CcObjectArtifact {
    pub object_path: PathBuf,
}

#[derive(Debug)]
pub struct CcObjectsArtifact {
    pub object_paths: Vec<PathBuf>,
}

/// Unified artifact for C/C++ build outputs.
/// Contains the build output along with transitive dependency information.
#[derive(Debug, Clone, Default)]
pub struct CcBuildOutput {
    /// Object files produced (for compile steps)
    pub object_files: Vec<PathBuf>,

    /// This target's library file (for static library archive steps)
    pub library: Option<PathBuf>,

    /// Transitive library dependencies (accumulated from deps)
    pub transitive_libraries: Vec<PathBuf>,
}

// ----------------------------------------------------------------------------
// Private Structs
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Default)]
struct CcExtraArgs {
    pub compiler_flags: IndexSet<String>,
    pub defines: IndexSet<String>,
    pub include_dirs: IndexSet<PathBuf>,
    pub libraries: IndexSet<PathBuf>,
    pub library_dirs: IndexSet<PathBuf>,
}

/// Artifact produced when linking an executable
#[derive(Debug)]
pub struct CompileExeArtifact {
    pub output_file: PathBuf,
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
    fn get_linker_args(&self, lang: CcLanguage) -> anyhow::Result<Vec<String>>;
    fn get_compiler(&self, lang: CcLanguage) -> anyhow::Result<&Path>;
    fn get_linker(&self, lang: CcLanguage) -> anyhow::Result<&Path>;
    fn get_archiver(&self, lang: CcLanguage) -> anyhow::Result<&Path>;
}

// ----------------------------------------------------------------------------
// Struct Implementations
// ----------------------------------------------------------------------------
impl CcExtraArgs {
    fn extend_static_public(&mut self, other: &CcStaticLibrary) {
        self.compiler_flags.extend(other.public_compiler_flags.iter().cloned());
        self.defines.extend(other.public_defines.iter().cloned());
        self.include_dirs.extend(other.public_include_dirs.iter().cloned());
        self.libraries.extend(other.public_libraries.iter().cloned());
        self.library_dirs.extend(other.public_library_dirs.iter().cloned());
    }

    fn extend_static_private(&mut self, other: &CcStaticLibrary) {
        self.compiler_flags.extend(other.private_compiler_flags.iter().cloned());
        self.defines.extend(other.private_defines.iter().cloned());
        self.include_dirs.extend(other.private_include_dirs.iter().cloned());
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
            CcLanguage::C => Ok(&toolchain.c),
            CcLanguage::Cpp => Ok(&toolchain.cpp),
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
            args.push(inc_dir.to_string_lossy().into_owned());
        }
        for lib_dir in &cc_toolchain.library_dirs {
            args.push(format!("-L{}", &lib_dir.to_string_lossy()));
        }
        for lib in &cc_toolchain.libraries {
            args.push(format!("-l{}", &lib.to_string_lossy()));
        }
        for define in &cc_toolchain.defines {
            args.push(format!("-D{}", define));
        }

        Ok(args)
    }

    fn get_linker_args(&self, lang: CcLanguage) -> anyhow::Result<Vec<String>> {
        let cc_toolchain = self.get_cc_toolchain(lang)?;
        Ok(cc_toolchain.linker_flags.clone())
    }

    fn get_compiler(&self, lang: CcLanguage) -> anyhow::Result<&Path> {
        let cc_toolchain = self.get_cc_toolchain(lang)?;
        Ok(&cc_toolchain.compiler)
    }

    fn get_linker(&self, lang: CcLanguage) -> anyhow::Result<&Path> {
        let cc_toolchain = self.get_cc_toolchain(lang)?;
        Ok(&cc_toolchain.linker)
    }

    fn get_archiver(&self, lang: CcLanguage) -> anyhow::Result<&Path> {
        let cc_toolchain = self.get_cc_toolchain(lang)?;
        Ok(&cc_toolchain.archiver)
    }
}

impl anubis::Rule for CcBinary {
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

        Ok(ctx.new_job(
            format!("Build CcBinary Target {}", self.target.target_path()),
            Box::new(move |job| build_cc_binary(binary.clone(), job)),
        ))
    }
}

impl crate::papyrus::PapyrusObjectType for CcBinary {
    fn name() -> &'static str {
        &"cc_binary"
    }
}

impl anubis::Rule for CcStaticLibrary {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        bail_loc_if!(
            ctx.mode.is_none(),
            "Can not create CcStaticLibrary job without a mode"
        );

        let lib = arc_self
            .clone()
            .downcast_arc::<CcStaticLibrary>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to CcStaticLibrary", arc_self))?;

        Ok(ctx.new_job(
            format!("Build CcStaticLibrary Target {}", self.target.target_path()),
            Box::new(move |job| build_cc_static_library(lib.clone(), job)),
        ))
    }
}

impl crate::papyrus::PapyrusObjectType for CcStaticLibrary {
    fn name() -> &'static str {
        &"cc_static_library"
    }
}

impl JobArtifact for CompileExeArtifact {}
impl JobArtifact for CcObjectArtifact {}
impl JobArtifact for CcObjectsArtifact {}
impl JobArtifact for CcBuildOutput {}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------
fn parse_cc_binary(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut binary = CcBinary::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    binary.target = t;
    Ok(Arc::new(binary))
}

fn parse_cc_static_library(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut lib = CcStaticLibrary::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    lib.target = t;
    Ok(Arc::new(lib))
}

fn build_cc_binary(binary: Arc<CcBinary>, job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job.ctx.mode.as_ref().ok_or_else(|| anyhow_loc!("build_cc_binary called without a mode. [{:?}]", binary))?;
    let lang = binary.lang;
    let cc_toolchain = job.ctx.get_cc_toolchain(lang)?;

    let mut child_jobs: Vec<JobId> = Default::default();
    let mut extra_args: CcExtraArgs = Default::default();

    // Extend deps
    let deps = binary.deps.iter().chain(cc_toolchain.exe_deps.iter());

    // create child job to compile each dep
    for dep in deps {
        // Build child dep
        let result = || -> anyhow::Result<()> {
            let dep_rule = job.ctx.anubis.get_rule(dep, &mode)?;
            let dep_job = dep_rule.build(dep_rule.clone(), job.ctx.clone())?;

            // Get extra args from CcStaticLibrary
            if let Ok(static_lib) = dep_rule.downcast_arc::<CcStaticLibrary>() {
                extra_args.extend_static_public(&static_lib);
            }

            child_jobs.push(dep_job.id);
            job.ctx.job_system.add_job(dep_job)?;

            Ok(())
        }();
        if let Err(e) = result {
            bail_loc!(
                "Failed to build child dep [{dep}] due to error: {e}",
            )
        }
    }

    // Extend args from binary as well
    extra_args.extend_binary(&binary);

    // create child job to compile each src
    for src in &binary.srcs {
        let substep = build_cc_file(src.clone(), &binary.target, job.ctx.clone(), extra_args.clone(), lang)?;
        match substep {
            Substep::Job(child_job) => {
                child_jobs.push(child_job.id);
                job.ctx.job_system.add_job(child_job)?;
            }
            Substep::Id(child_job_id) => {
                child_jobs.push(child_job_id);
            }
        }
    }

    // create a continuation job to link all objects from child jobs into result
    let target = binary.target.clone();
    let name = binary.name.clone();
    let blocked_by = child_jobs.clone();
    let link_job = move |link_job: Job| -> anyhow::Result<JobOutcome> {
        // link all object files into an exe
        link_exe(&child_jobs, &target, &name, link_job.ctx.clone(), &extra_args, lang)
    };

    // Create continuation job to perform link
    let continuation_job = job.ctx.new_job(
        format!("{} (link)", job.desc),
        Box::new(link_job),
    );

    // Defer!
    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by,
        continuation_job,
    }))
}

fn build_cc_static_library(static_library: Arc<CcStaticLibrary>, job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job.ctx.mode.as_ref().unwrap(); // should have been validated previously

    let mut child_jobs: Vec<JobId> = Default::default();
    let mut extra_args: CcExtraArgs = Default::default();

    // create child job to compile each dep
    for dep in &static_library.deps {
        // Build child dep
        let dep_rule = job.ctx.anubis.get_rule(dep, &mode)?;
        let dep_job = dep_rule.build(dep_rule.clone(), job.ctx.clone())?;

        // Get extra args from CcStaticLibrary
        if let Ok(static_lib) = dep_rule.downcast_arc::<CcStaticLibrary>() {
            extra_args.extend_static_public(&static_lib);
        }

        child_jobs.push(dep_job.id);
        job.ctx.job_system.add_job(dep_job)?;
    }

    extra_args.extend_static_public(&static_library);
    extra_args.extend_static_private(&static_library);

    // Get the language from the rule
    let lang = static_library.lang;

    // create child job to compile each src
    for src in &static_library.srcs {
        let substep = build_cc_file(
            src.clone(),
            &static_library.target,
            job.ctx.clone(),
            extra_args.clone(),
            lang,
        )?;
        match substep {
            Substep::Job(child_job) => {
                child_jobs.push(child_job.id);
                job.ctx.job_system.add_job(child_job)?;
            }
            Substep::Id(child_job_id) => {
                child_jobs.push(child_job_id);
            }
        }
    }

    // create a continuation job to archive all objects from child jobs into result
    let target = static_library.target.clone();
    let name = static_library.name.clone();
    let blocked_by = child_jobs.clone();
    let archive_job = move |archive_job: Job| -> anyhow::Result<JobOutcome> {
        // archive all object files into a static library
        archive_static_library(&child_jobs, &target, &name, archive_job.ctx.clone(), lang)
    };

    // Create continuation job to perform archive
    let continuation_job = job.ctx.new_job(
        format!("{} (create archive)", job.desc),
        Box::new(archive_job),
    );

    // Defer!
    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by,
        continuation_job,
    }))
}

fn build_cc_file(
    src_path: PathBuf,
    target: &AnubisTarget,
    ctx: Arc<JobContext>,
    extra_args: CcExtraArgs,
    lang: CcLanguage,
) -> anyhow::Result<Substep> {
    let src = src_path.to_string_lossy().to_string();

    // See if job for (mode, target, compile_$src) already exists
    let job_key = JobCacheKey {
        mode: ctx.mode.as_ref().unwrap().target.clone(),
        target: target.clone(),
        substep: Some(format!("compile_{}", &src)),
    };

    let mut job_cache = ctx.anubis.job_cache.write().map_err(|e| anyhow_loc!("Lock poisoned: {}", e))?;
    let entry = job_cache.entry(job_key);
    let mut new_job = false;
    let job_id = *entry.or_insert_with(|| {
        new_job = true;
        ctx.get_next_id()
    });
    drop(job_cache);

    if !new_job {
        return Ok(Substep::Id(job_id));
    }

    // Create a new job that builds the file
    let ctx2 = ctx.clone();
    let src2 = src.clone();
    let job_fn = move |job| -> anyhow::Result<JobOutcome> {
        // Get initial args args
        let mut args = ctx2.get_args(lang)?;

        // Add extra args
        args.push("-c".into()); // compile object file, do not link

        for dir in &extra_args.include_dirs {
            args.push(format!("-I{}", &dir.to_string_lossy()));
        }

        for flag in &extra_args.compiler_flags {
            args.push(flag.clone());
        }

        for define in &extra_args.defines {
            args.push(format!("-D{}", define));
        }

        // Compute object output filepath
        let src_dir =
            src_path.parent().ok_or_else(|| anyhow_loc!("No parent dir for [{:?}]", src_path))?;
        let src_filename =
            src_path.file_name().ok_or_else(|| anyhow_loc!("No filename for [{:?}]", src_path))?;
        let reldir = pathdiff::diff_paths(&src_dir, &ctx2.anubis.root).ok_or_else(|| {
            anyhow_loc!(
                "Could not relpath from [{:?}] to [{:?}]",
                &ctx2.anubis.root,
                &src_path
            )
        })?;
        let mode_name = &ctx2.mode.as_ref().unwrap().name;
        let output_file = ctx2
            .anubis
            .build_dir(mode_name)
            .join(reldir)
            .join(src_filename)
            .with_extension("obj")
            .slash_fix();
        ensure_directory_for_file(&output_file)?;

        args.push("-o".into());
        args.push(output_file.to_string_lossy().into());
        args.push(src2.clone());

        // Add verbose flag if enabled
        if ctx2.anubis.verbose_tools {
            args.push("-v".into());
        }

        // run the command
        let compiler = ctx2.get_compiler(lang)?;
        let verbose = ctx2.anubis.verbose_tools;
        let (output, compile_duration) = {
            let _span = tracing::info_span!("compile", file = %src2).entered();
            let compile_start = std::time::Instant::now();
            let output = run_command_verbose(compiler, &args, verbose)?;
            (output, compile_start.elapsed())
        };

        if output.status.success() {
            Ok(JobOutcome::Success(Arc::new(CcBuildOutput {
                object_files: vec![output_file],
                library: None,
                transitive_libraries: Vec::new(),
            })))
        } else {
            tracing::error!(
                source_file = %src2,
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

    Ok(Substep::Job(ctx.new_job_with_id(
        job_id,
        format!("Compile {} file [{}]", lang.file_description(), src),
        Box::new(job_fn),
    )))
}

fn archive_static_library(
    child_jobs: &[JobId],
    target: &AnubisTarget,
    name: &str,
    ctx: Arc<JobContext>,
    lang: CcLanguage,
) -> anyhow::Result<JobOutcome> {
    // Collect object files and transitive libraries from all child jobs
    let mut object_files: Vec<PathBuf> = Default::default();
    let mut transitive_libraries: IndexSet<PathBuf> = Default::default();

    for job_id in child_jobs {
        let job_result = ctx.job_system.get_result(*job_id)?;
        if let Ok(r) = job_result.cast::<CcBuildOutput>() {
            // Collect object files
            object_files.extend(r.object_files.iter().cloned());
            // Collect this dep's library
            if let Some(lib) = &r.library {
                transitive_libraries.insert(lib.clone());
            }
            // Collect transitive libraries
            transitive_libraries.extend(r.transitive_libraries.iter().cloned());
        } else if let Ok(r) = job_result.cast::<CcObjectArtifact>() {
            // Handle single object/library from nasm_static_library
            // The object_path can be either a .obj or .lib depending on the rule
            transitive_libraries.insert(r.object_path.clone());
        } else if let Ok(r) = job_result.cast::<CcObjectsArtifact>() {
            // Handle multiple objects from nasm_objects
            object_files.extend(r.object_paths.iter().cloned());
        }
    }

    // Build args
    let mut args: Vec<String> = Default::default();
    // Use "rcsv" for verbose output if enabled, otherwise "rcs"
    if ctx.anubis.verbose_tools {
        args.push("rcsv".to_owned());
    } else {
        args.push("rcs".to_owned());
    }

    // Compute output filepath
    let relpath = target.get_relative_dir();
    let mode_name = &ctx.mode.as_ref().unwrap().name;
    let build_dir = ctx.anubis.build_dir(mode_name).join(relpath);
    ensure_directory(&build_dir)?;

    let output_file = build_dir.join(name).with_extension("lib").slash_fix();
    args.push(output_file.to_string_lossy().to_string());

    // put link args in a response file
    let response_filepath = build_dir.join(name).with_extension("rsp").slash_fix();

    let link_args_str: String = object_files.iter().map(|p| p.to_string_lossy()).join(" ");
    std::fs::write(&response_filepath, &link_args_str).with_context(|| {
        format!(
            "Failed to write link args into response file: [{:?}]",
            response_filepath
        )
    })?;
    args.push(format!("@{}", response_filepath.to_string_lossy()));

    // run the command
    let archiver = ctx.get_archiver(lang)?;
    let verbose = ctx.anubis.verbose_tools;
    let output = {
        let _span = tracing::info_span!("archive", target = %name).entered();
        run_command_verbose(archiver, &args, verbose)?
    };

    if output.status.success() {
        // Return CcBuildOutput with this library and accumulated transitive deps
        Ok(JobOutcome::Success(Arc::new(CcBuildOutput {
            object_files: Vec::new(), // Archive doesn't expose object files
            library: Some(output_file),
            transitive_libraries: transitive_libraries.into_iter().collect(),
        })))
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

fn link_exe(
    child_jobs: &[JobId],
    target: &AnubisTarget,
    name: &str,
    ctx: Arc<JobContext>,
    extra_args: &CcExtraArgs,
    lang: CcLanguage,
) -> anyhow::Result<JobOutcome> {
    // Collect object files and libraries from all child jobs
    let mut object_files: Vec<PathBuf> = Default::default();
    let mut library_files: IndexSet<PathBuf> = Default::default();

    for job_id in child_jobs {
        let job_result = ctx.job_system.get_result(*job_id)?;
        if let Ok(r) = job_result.cast::<CcBuildOutput>() {
            // Collect object files
            object_files.extend(r.object_files.iter().cloned());
            // Collect this dep's library
            if let Some(lib) = &r.library {
                library_files.insert(lib.clone());
            }
            // Collect transitive libraries
            library_files.extend(r.transitive_libraries.iter().cloned());
        } else if let Ok(r) = job_result.cast::<CcObjectArtifact>() {
            // Handle single object/library from nasm_static_library
            // The object_path can be either a .obj or .lib depending on the rule
            library_files.insert(r.object_path.clone());
        } else if let Ok(r) = job_result.cast::<CcObjectsArtifact>() {
            // Handle multiple objects from nasm_objects
            object_files.extend(r.object_paths.iter().cloned());
        }
    }

    // Determine target platform for linker flag formatting
    let mode = ctx.mode.as_ref().unwrap();
    let target_platform = mode.vars.get("target_platform").map(|s| s.as_str()).unwrap_or("windows");
    let is_msvc_linker = target_platform == "windows";

    // Build linker-specific arguments (NOT compiler args)
    let mut args: Vec<String> = Vec::new();
    let cc_toolchain = ctx.get_cc_toolchain(lang)?;

    // Add linker flags from toolchain, stripping -Wl, prefixes
    for flag in &cc_toolchain.linker_flags {
        if let Some(stripped) = flag.strip_prefix("-Wl,") {
            // Split comma-separated flags that were passed through -Wl,
            for part in stripped.split(',') {
                if !part.is_empty() {
                    args.push(part.to_string());
                }
            }
        } else {
            args.push(flag.clone());
        }
    }

    // Add toolchain library directories
    for lib_dir in &cc_toolchain.library_dirs {
        if is_msvc_linker {
            args.push(format!("/LIBPATH:{}", lib_dir.to_string_lossy()));
        } else {
            args.push(format!("-L{}", lib_dir.to_string_lossy()));
        }
    }

    // Add extra library directories from target
    for lib_dir in &extra_args.library_dirs {
        if is_msvc_linker {
            args.push(format!("/LIBPATH:{}", lib_dir.to_string_lossy()));
        } else {
            args.push(format!("-L{}", lib_dir.to_string_lossy()));
        }
    }

    // Add all object files
    args.extend(object_files.iter().map(|p| p.to_string_lossy().into_owned()));

    // Add all library files (direct deps + transitive deps in correct order)
    args.extend(library_files.iter().map(|p| p.to_string_lossy().into_owned()));

    // Add toolchain libraries
    for lib in &cc_toolchain.libraries {
        let lib_str = lib.to_string_lossy();
        if is_msvc_linker {
            // MSVC linker: pass library name directly (with .lib extension if needed)
            if lib_str.ends_with(".lib") {
                args.push(lib_str.into_owned());
            } else {
                args.push(format!("{}.lib", lib_str));
            }
        } else {
            args.push(format!("-l{}", lib_str));
        }
    }

    // Add extra libraries from target
    for lib in &extra_args.libraries {
        let lib_str = lib.to_string_lossy();
        if is_msvc_linker {
            if lib_str.ends_with(".lib") {
                args.push(lib_str.into_owned());
            } else {
                args.push(format!("{}.lib", lib_str));
            }
        } else {
            args.push(format!("-l{}", lib_str));
        }
    }

    // Compute output filepath with platform-appropriate extension
    let relpath = target.get_relative_dir();
    let mode_name = &mode.name;
    let output_file = ctx
        .anubis
        .out_dir(mode_name)
        .join(relpath)
        .join(name)
        .with_extension(if target_platform == "windows" { "exe" } else { "" })
        .slash_fix();
    ensure_directory_for_file(&output_file)?;

    // Add output file argument
    if is_msvc_linker {
        args.push(format!("/OUT:{}", output_file.to_string_lossy()));
    } else {
        args.push("-o".into());
        args.push(output_file.to_string_lossy().into());
    }

    // run the command
    let linker = ctx.get_linker(lang)?;
    let verbose = ctx.anubis.verbose_tools;
    let (output, link_duration) = {
        let _span = tracing::info_span!("link", target = %name).entered();
        let link_start = std::time::Instant::now();
        let output = run_command_verbose(linker, &args, verbose)?;
        (output, link_start.elapsed())
    };

    if output.status.success() {
        Ok(JobOutcome::Success(Arc::new(CompileExeArtifact { output_file })))
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

    Ok(())
}
