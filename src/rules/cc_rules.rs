#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis::{self, AnubisTarget, JobCacheKey, RuleExt};
use crate::rules::rule_utils::{ensure_directory, ensure_directory_for_file, run_command_verbose};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{job_system::*, toolchain};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
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

/// Unified C/C++ static library rule. Use `cc_static_library` in ANUBIS files with
/// an explicit `lang` field set to "c" or "cpp" to select the toolchain.
#[rustfmt::skip]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CcStaticLibrary {
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
    target: anubis::AnubisTarget,
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
    fn get_compiler(&self, lang: CcLanguage) -> anyhow::Result<&Utf8Path>;
    fn get_linker(&self, lang: CcLanguage) -> anyhow::Result<&Utf8Path>;
    fn get_archiver(&self, lang: CcLanguage) -> anyhow::Result<&Utf8Path>;
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
            args.push(inc_dir.to_string());
        }
        for lib_dir in &cc_toolchain.library_dirs {
            args.push(format!("-L{}", lib_dir));
        }
        for lib in &cc_toolchain.libraries {
            args.push(format!("-l{}", lib));
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
            format!(
                "Build CcBinary Target {} with mode {}",
                self.target.target_path(),
                ctx.as_ref().mode.as_ref().map_or("modeless", |m| &m.name)
            ),
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
            format!(
                "Build CcStaticLibrary Target {} with mode {}",
                self.target.target_path(),
                ctx.as_ref().mode.as_ref().map_or("modeless", |m| &m.name)
            ),
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

fn parse_cc_static_library(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut lib = CcStaticLibrary::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    lib.target = t;
    Ok(Arc::new(lib))
}

fn build_cc_binary(binary: Arc<CcBinary>, job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job
        .ctx
        .mode
        .as_ref()
        .ok_or_else(|| anyhow_loc!("build_cc_binary called without a mode. [{:?}]", binary))?;
    let lang = binary.lang;
    let cc_toolchain = job.ctx.get_cc_toolchain(lang)?;

    let mut child_jobs: Vec<JobId> = Default::default();
    let mut extra_args: CcExtraArgs = Default::default();

    // Extend deps
    let deps = binary.deps.iter().chain(cc_toolchain.exe_deps.iter());

    // create child job to compile each dep
    for dep in deps {
        // Get rule to extract extra args
        let dep_rule = job.ctx.anubis.get_rule(dep, &mode)?;

        // Get extra args from CcStaticLibrary
        if let Ok(static_lib) = dep_rule.downcast_arc::<CcStaticLibrary>() {
            extra_args.extend_static_public(&static_lib);
        }

        // Use build_rule to leverage the job cache (avoids duplicate jobs)
        let job_id = job.ctx.anubis.build_rule(dep, &job.ctx)?;
        child_jobs.push(job_id);
    }

    // Extend args from binary as well
    extra_args.extend_binary(&binary);

    // Validate that all directories exist before compiling
    job.ctx.anubis.verify_directories(&extra_args.include_dirs, "Include")?;
    job.ctx.anubis.verify_directories(&extra_args.library_dirs, "Library")?;
    job.ctx.anubis.verify_directories(&cc_toolchain.system_include_dirs, "System include")?;
    job.ctx.anubis.verify_directories(&cc_toolchain.library_dirs, "Toolchain library")?;

    // Create a blocker job that waits for all dependencies to complete.
    // This ensures any generated source files exist before compilation starts.
    let deps_blocker_id = if !child_jobs.is_empty() {
        let blocker = job.ctx.new_job(
            format!("{} (await deps)", job.desc),
            Box::new(|_| Ok(JobOutcome::Success(Arc::new(DepsCompleteMarker)))),
        );
        let blocker_id = blocker.id;
        job.ctx.job_system.add_job_with_deps(blocker, &child_jobs)?;
        Some(blocker_id)
    } else {
        None
    };

    // create child job to compile each src
    for src in &binary.srcs {
        let substep = build_cc_file(
            src.clone(),
            &binary.target,
            job.ctx.clone(),
            extra_args.clone(),
            lang,
        )?;
        match substep {
            Substep::Job(child_job) => {
                child_jobs.push(child_job.id);
                // If we have a deps blocker, compile jobs wait for it
                if let Some(blocker_id) = deps_blocker_id {
                    job.ctx.job_system.add_job_with_deps(child_job, &[blocker_id])?;
                } else {
                    job.ctx.job_system.add_job(child_job)?;
                }
            }
            Substep::Id(child_job_id) => {
                child_jobs.push(child_job_id);
            }
        }
    }

    // create a continuation job to link all objects from child jobs into result
    let target = binary.target.clone();
    let output_name = binary.exe_name.clone().unwrap_or_else(|| binary.name.clone());
    let blocked_by = child_jobs.clone();
    let link_job = move |link_job: Job| -> anyhow::Result<JobOutcome> {
        // link all object files into an exe
        link_exe(
            &child_jobs,
            &target,
            &output_name,
            link_job.ctx.clone(),
            &extra_args,
            lang,
        )
    };

    // Create continuation job to perform link
    let continuation_job = job.ctx.new_job(format!("{} (link)", job.desc), Box::new(link_job));

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
        // Get rule to extract extra args
        let dep_rule = job.ctx.anubis.get_rule(dep, &mode)?;

        // Get extra args from CcStaticLibrary
        if let Ok(static_lib) = dep_rule.downcast_arc::<CcStaticLibrary>() {
            extra_args.extend_static_public(&static_lib);
        }

        // Use build_rule to leverage the job cache (avoids duplicate jobs)
        let job_id = job.ctx.anubis.build_rule(dep, &job.ctx)?;
        child_jobs.push(job_id);
    }

    extra_args.extend_static_public(&static_library);
    extra_args.extend_static_private(&static_library);

    // Get the language from the rule
    let lang = static_library.lang;
    let cc_toolchain = job.ctx.get_cc_toolchain(lang)?;

    // Validate that all directories exist before compiling
    job.ctx.anubis.verify_directories(&extra_args.include_dirs, "Include")?;
    job.ctx.anubis.verify_directories(&extra_args.library_dirs, "Library")?;
    job.ctx.anubis.verify_directories(&cc_toolchain.system_include_dirs, "System include")?;
    job.ctx.anubis.verify_directories(&cc_toolchain.library_dirs, "Toolchain library")?;

    // Create a blocker job that waits for all dependencies to complete.
    // This ensures any generated source files exist before compilation starts.
    let deps_blocker_id = if !child_jobs.is_empty() {
        let blocker = job.ctx.new_job(
            format!("{} (await deps)", job.desc),
            Box::new(|_| Ok(JobOutcome::Success(Arc::new(DepsCompleteMarker)))),
        );
        let blocker_id = blocker.id;
        job.ctx.job_system.add_job_with_deps(blocker, &child_jobs)?;
        Some(blocker_id)
    } else {
        None
    };

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
                // If we have a deps blocker, compile jobs wait for it
                if let Some(blocker_id) = deps_blocker_id {
                    job.ctx.job_system.add_job_with_deps(child_job, &[blocker_id])?;
                } else {
                    job.ctx.job_system.add_job(child_job)?;
                }
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
    let continuation_job = job.ctx.new_job(format!("{} (create archive)", job.desc), Box::new(archive_job));

    // Defer!
    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by,
        continuation_job,
    }))
}

fn build_cc_file(
    src_path: Utf8PathBuf,
    target: &AnubisTarget,
    ctx: Arc<JobContext>,
    extra_args: CcExtraArgs,
    lang: CcLanguage,
) -> anyhow::Result<Substep> {
    let src = src_path.to_string();

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
            args.push(format!("-I{}", dir));
        }

        for flag in &extra_args.compiler_flags {
            args.push(flag.clone());
        }

        for define in &extra_args.defines {
            args.push(format!("-D{}", define));
        }

        // Compute object output filepath
        let src_dir = src_path.parent().ok_or_else(|| anyhow_loc!("No parent dir for [{:?}]", src_path))?;
        let src_filename = src_path
            .file_name()
            .ok_or_else(|| anyhow_loc!("No filename for [{:?}]", src_path))?;
        let reldir = pathdiff::diff_paths(src_dir.as_std_path(), ctx2.anubis.root.as_std_path())
            .ok_or_else(|| {
                anyhow_loc!(
                    "Could not relpath from [{:?}] to [{:?}]",
                    &ctx2.anubis.root,
                    &src_path
                )
            })?;
        let reldir = Utf8PathBuf::try_from(reldir)
            .map_err(|e| anyhow_loc!("Non-UTF8 path from diff_paths: {:?}", e))?;
        let mode_name = &ctx2.mode.as_ref().unwrap().name;
        let output_file = ctx2
            .anubis
            .build_dir(mode_name)
            .join(reldir)
            .join(src_filename)
            .with_extension("obj")
            .slash_fix();
        ensure_directory_for_file(&output_file)?;

        // Add dependency file generation for hermetic validation
        let dep_file = output_file.with_extension("d");
        args.push("-MF".into());
        args.push(dep_file.to_string());

        // Specify output file
        args.push("-o".into());
        args.push(output_file.to_string());
        args.push(src2.clone());

        // Add verbose flag if enabled
        if ctx2.anubis.verbose_tools {
            args.push("-v".into()); // verbose
            args.push("-H".into()); // include hierarchy
        }

        // Run the command
        let compiler = ctx2.get_compiler(lang)?;
        let verbose = ctx2.anubis.verbose_tools;
        let (output, compile_duration) = {
            let _span = tracing::info_span!("compile", file = %src2).entered();
            let compile_start = std::time::Instant::now();
            let output = run_command_verbose(compiler, &args, verbose)?;
            (output, compile_start.elapsed())
        };

        if output.status.success() {
            // Validate hermetic dependencies
            validate_hermetic_deps(dep_file.as_std_path(), ctx2.anubis.root.as_std_path())?;

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
    let mut object_files: Vec<Utf8PathBuf> = Default::default();
    let mut transitive_libraries: IndexSet<Utf8PathBuf> = Default::default();

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

    // Delete existing archive to ensure clean build (llvm-ar updates in place)
    if output_file.exists() {
        std::fs::remove_file(&output_file)?;
    }

    args.push(output_file.to_string());

    // put link args in a response file
    let response_filepath = build_dir.join(name).with_extension("rsp").slash_fix();

    let link_args_str: String = object_files.iter().map(|p| p.to_string()).join(" ");
    std::fs::write(&response_filepath, &link_args_str).with_context(|| {
        format!(
            "Failed to write link args into response file: [{:?}]",
            response_filepath
        )
    })?;
    args.push(format!("@{}", response_filepath));

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
    let mut object_files: Vec<Utf8PathBuf> = Default::default();
    let mut library_files: IndexSet<Utf8PathBuf> = Default::default();

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
            args.push(format!("/LIBPATH:{}", lib_dir));
        } else {
            args.push(format!("-L{}", lib_dir));
        }
    }

    // Add extra library directories from target
    for lib_dir in &extra_args.library_dirs {
        if is_msvc_linker {
            args.push(format!("/LIBPATH:{}", lib_dir));
        } else {
            args.push(format!("-L{}", lib_dir));
        }
    }

    // Add all object files
    args.extend(object_files.iter().map(|p| p.to_string()));

    // Add all library files (direct deps + transitive deps in correct order)
    args.extend(library_files.iter().map(|p| p.to_string()));

    // Add toolchain libraries
    for lib in &cc_toolchain.libraries {
        let lib_str = lib.as_str();
        if is_msvc_linker {
            // MSVC linker: pass library name directly (with .lib extension if needed)
            if lib_str.ends_with(".lib") {
                args.push(lib_str.to_owned());
            } else {
                args.push(format!("{}.lib", lib_str));
            }
        } else {
            args.push(format!("-l{}", lib_str));
        }
    }

    // Add extra libraries from target
    for lib in &extra_args.libraries {
        let lib_str = lib.as_str();
        if is_msvc_linker {
            if lib_str.ends_with(".lib") {
                args.push(lib_str.to_owned());
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
        .bin_dir(mode_name)
        .join(relpath)
        .join(name)
        .with_extension(if target_platform == "windows" { "exe" } else { "" })
        .slash_fix();
    ensure_directory_for_file(&output_file)?;

    // Add output file argument
    if is_msvc_linker {
        args.push(format!("/OUT:{}", output_file));
    } else {
        args.push("-o".into());
        args.push(output_file.to_string());
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

/// Validates that all dependencies listed in a Makefile-style .d file are
/// located under the Anubis root directory. This ensures hermetic builds
/// with no accidental system header dependencies.
///
/// Note: We intentionally do NOT canonicalize dependency paths because that
/// would resolve symlinks to their real locations. A symlink inside the Anubis
/// root pointing elsewhere is a deliberate developer choice and should be allowed.
fn validate_hermetic_deps(dep_file: &Path, anubis_root: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(dep_file)
        .with_context(|| format!("Failed to read dependency file: {:?}", dep_file))?;

    // Normalize root path for comparison (forward slashes, lowercase on Windows)
    let root_normalized = normalize_path_for_comparison(anubis_root);

    let mut violations = Vec::new();

    // .d file format:
    //   target.obj: \
    //     dep1.cpp \
    //     dep2.h \
    //     dep3.h
    // Skip first line (target), then each line is a dependency path with trailing backslash
    for line in content.lines().skip(1) {
        let dep = line.trim().trim_end_matches('\\').trim();
        if dep.is_empty() {
            continue;
        }

        let dep_path = PathBuf::from(dep);
        let dep_normalized = normalize_path_for_comparison(&dep_path);

        if !dep_normalized.starts_with(&root_normalized) {
            violations.push(dep_path);
        }
    }

    if !violations.is_empty() {
        bail_loc!(
            "External dependencies detected (files outside Anubis root):\n  {}",
            violations
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }

    Ok(())
}

/// Normalize a path for comparison: forward slashes, lowercase on Windows.
fn normalize_path_for_comparison(path: &Path) -> String {
    let s = path.to_string_lossy().to_string().slash_fix();
    #[cfg(windows)]
    {
        s.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s
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
