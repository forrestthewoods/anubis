#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis::{self, AnubisTarget, JobCacheKey, RuleExt};
use crate::job_system::*;
use crate::rules::rule_utils::{ensure_directory, ensure_directory_for_file, run_command};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use anyhow::Context;
use itertools::Itertools;
use serde::Deserialize;
use std::collections::HashSet;
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

// ----------------------------------------------------------------------------
// Private Structs
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Default)]
struct CcExtraArgs {
    pub compiler_flags: HashSet<String>,
    pub defines: HashSet<String>,
    pub include_dirs: HashSet<PathBuf>,
    pub libraries: HashSet<PathBuf>,
    pub library_dirs: HashSet<PathBuf>,
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
    let mode = job.ctx.mode.as_ref().unwrap(); // should have been validated previously

    let mut dep_jobs: Vec<JobId> = Default::default();
    let mut extra_args: CcExtraArgs = Default::default();

    // create child job to compile each dep
    for dep in &binary.deps {
        // Build child dep
        let result = || -> anyhow::Result<()> {
            let dep_rule = job.ctx.anubis.get_rule(dep, &mode)?;
            let dep_job = dep_rule.build(dep_rule.clone(), job.ctx.clone())?;

            // Get extra args from CcStaticLibrary
            if let Ok(static_lib) = dep_rule.downcast_arc::<CcStaticLibrary>() {
                extra_args.extend_static_public(&static_lib);
            }

            dep_jobs.push(dep_job.id);
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

    // Get the language from the rule
    let lang = binary.lang;

    // create child job to compile each src
    for src in &binary.srcs {
        let substep = build_cc_file(src.clone(), &binary.target, job.ctx.clone(), extra_args.clone(), lang)?;
        match substep {
            Substep::Job(child_job) => {
                // Add new job as a dependency
                dep_jobs.push(child_job.id);

                // Add new job
                job.ctx.job_system.add_job(child_job)?;
            }
            Substep::Id(child_job_id) => {
                // Create a dependency on an existing job
                dep_jobs.push(child_job_id);
            }
        }
    }

    // create a continuation job to link all objects from child jobs into result
    let link_arg_jobs = dep_jobs.clone();
    let target = binary.target.clone();
    let name = binary.name.clone();
    let link_job = move |link_job: Job| -> anyhow::Result<JobOutcome> {
        // link all object files into an exe
        link_exe(&link_arg_jobs, &target, &name, link_job.ctx.clone(), &extra_args, lang)
    };

    // Create continuation job to perform link
    let continuation_job = job.ctx.new_job(
        format!("{} (link)", job.desc),
        Box::new(link_job),
    );

    // Defer!
    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: dep_jobs,
        continuation_job,
    }))
}

fn build_cc_static_library(static_library: Arc<CcStaticLibrary>, job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job.ctx.mode.as_ref().unwrap(); // should have been validated previously

    let mut dep_jobs: Vec<JobId> = Default::default();
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

        dep_jobs.push(dep_job.id);
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
                // Add new job as a dependency
                dep_jobs.push(child_job.id);

                // Add new job
                job.ctx.job_system.add_job(child_job)?;
            }
            Substep::Id(child_job_id) => {
                // Create a dependency on an existing job
                dep_jobs.push(child_job_id);
            }
        }
    }

    // create a continuation job to archive all objects from child jobs into result
    let archive_arg_jobs = dep_jobs.clone();
    let target = static_library.target.clone();
    let name = static_library.name.clone();
    let archive_job = move |archive_job: Job| -> anyhow::Result<JobOutcome> {
        // archive all object files into a static library
        archive_static_library(&archive_arg_jobs, &target, &name, archive_job.ctx.clone(), lang)
    };

    // Create continuation job to perform archive
    let continuation_job = job.ctx.new_job(
        format!("{} (create archive)", job.desc),
        Box::new(archive_job),
    );

    // Defer!
    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: dep_jobs,
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

        args.push("-v".into());

        // run the command
        let compiler = ctx2.get_compiler(lang)?;
        let compile_start = std::time::Instant::now();
        let output = run_command(compiler, &args)?;
        let compile_duration = compile_start.elapsed();

        if output.status.success() {
            Ok(JobOutcome::Success(Arc::new(CcObjectArtifact {
                object_path: output_file,
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
    object_jobs: &[JobId],
    target: &AnubisTarget,
    name: &str,
    ctx: Arc<JobContext>,
    lang: CcLanguage,
) -> anyhow::Result<JobOutcome> {
    // Get all child jobs
    let mut link_args: Vec<Arc<CcObjectArtifact>> = Default::default();
    for link_arg_job in object_jobs {
        // TODO: what about different types? what about unexpected types? Blech.
        let job_result = ctx.job_system.get_result(*link_arg_job)?;
        if let Ok(r) = job_result.cast::<CcObjectArtifact>() {
            link_args.push(r);
        }
    }

    // Build args
    let mut args: Vec<String> = Default::default();
    args.push("rcs".to_owned());

    // Compute output filepath
    let relpath = target.get_relative_dir();
    let mode_name = &ctx.mode.as_ref().unwrap().name;
    let build_dir = ctx.anubis.build_dir(mode_name).join(relpath);
    ensure_directory(&build_dir)?;

    let output_file = build_dir.join(name).with_extension("lib").slash_fix();
    args.push(output_file.to_string_lossy().to_string());

    // put link args in a response file
    let response_filepath = build_dir.join(name).with_extension("rsp").slash_fix();

    let link_args_str: String = link_args.iter().map(|p| p.object_path.to_string_lossy()).join(" ");
    std::fs::write(&response_filepath, &link_args_str).with_context(|| {
        format!(
            "Failed to write link args into response file: [{:?}]",
            response_filepath
        )
    })?;
    args.push(format!("@{}", response_filepath.to_string_lossy()));

    // run the command
    let archiver = ctx.get_archiver(lang)?;
    let output = run_command(archiver, &args)?;

    if output.status.success() {
        Ok(JobOutcome::Success(Arc::new(CcObjectArtifact {
            object_path: output_file,
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
    link_arg_jobs: &[JobId],
    target: &AnubisTarget,
    name: &str,
    ctx: Arc<JobContext>,
    extra_args: &CcExtraArgs,
    lang: CcLanguage,
) -> anyhow::Result<JobOutcome> {
    // Get all child jobs
    let mut link_args: Vec<PathBuf> = Default::default();
    for link_arg_job in link_arg_jobs {
        let job_result = ctx.job_system.get_result(*link_arg_job)?;
        if let Ok(r) = job_result.cast::<CcObjectArtifact>() {
            link_args.push(r.object_path.clone());
        } else if let Ok(r) = job_result.cast::<CcObjectsArtifact>() {
            link_args.extend(r.object_paths.iter().cloned());
        } else {
            bail_loc!(
                "Unknown dependency result type: [{}]",
                std::any::type_name_of_val(&job_result)
            )
        }
    }

    // Build link command
    let mut args = ctx.get_args(lang)?;

    // Add linker-specific flags from toolchain
    args.extend(ctx.get_linker_args(lang)?);

    // Add extra args
    for lib_dir in &extra_args.library_dirs {
        args.push(format!("-L{}", &lib_dir.to_string_lossy()));
    }

    for lib in &extra_args.libraries {
        args.push(format!("-l{}", &lib.to_string_lossy()));
    }

    // Add all object files
    args.extend(link_args.iter().map(|p| p.to_string_lossy().into()));

    // args.push("C:/Users/lordc/AppData/Local/zig/o/03bca4392b84606eec3d46f80057cd4e/Scrt1.o".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/55dfa83a4f4b12116e23f4ec9777d4f8/crti.o".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/fad170fd298b8fd8bff1ba805a71756f/libc++abi.a".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/1bf1d8780ed85e68dd8d74d05e544265/libc++.a".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/85b568e3cd646bd03ffc524e8f933c62/libunwind.a".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libm.so.6".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libpthread.so.0".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libc.so.6".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libdl.so.2".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/librt.so.1".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libld.so.2".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libutil.so.1".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libresolv.so.2".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/e244f0af77d6abfb14cbc7be4d094091/libc_nonshared.a".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/d88abd594b039257747920427b18cc0c/libcompiler_rt.a".into());
    // args.push("C:/Users/lordc/AppData/Local/zig/o/026418d2b02a504673714dfd597c332d/crtn.o".into());

    // Compute output filepath
    let relpath = target.get_relative_dir();
    let mode_name = &ctx.mode.as_ref().unwrap().name;
    let output_file = ctx
        .anubis
        .out_dir(mode_name)
        .join(relpath)
        .join(name)
        .with_extension("exe")
        .slash_fix();
    ensure_directory_for_file(&output_file)?;

    args.push("-o".into());
    args.push(output_file.to_string_lossy().into());

    // run the command
    let compiler = ctx.get_compiler(lang)?;
    let link_start = std::time::Instant::now();
    let output = run_command(compiler, &args)?;
    let link_duration = link_start.elapsed();

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
