#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis::{self, AnubisTarget, JobCacheKey, RuleExt};
use crate::job_system::*;
use crate::rule_utils::{ensure_directory, ensure_directory_for_file};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use anyhow::Context;
use itertools::Itertools;
use serde::Deserialize;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;

use crate::papyrus::*;
use crate::toolchain::Toolchain;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};
use crate::{anyhow_with_context, bail_with_context, timed_span};
use serde::{de, Deserializer};

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------
#[rustfmt::skip]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CcBinary {
    pub name: String,
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

#[rustfmt::skip]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CcStaticLibrary {
    pub name: String,
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
pub struct CcObjectResult {
    pub object_path: PathBuf,
}

#[derive(Debug)]
pub struct CcObjectsResult {
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

// TODO: replace with CcObject
#[derive(Debug)]
struct CompileExeResult {
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
    fn get_args(&self) -> anyhow::Result<Vec<String>>;
    fn get_compiler(&self) -> anyhow::Result<&Path>;
    fn get_archiver(&self) -> anyhow::Result<&Path>;
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

    fn get_args(&self) -> anyhow::Result<Vec<String>> {
        let toolchain = self.get_toolchain()?;

        let mut args: Vec<String> = Default::default();
        for flag in &toolchain.cpp.compiler_flags {
            args.push(flag.clone());
        }
        for inc_dir in &toolchain.cpp.system_include_dirs {
            args.push("-isystem".to_owned());
            args.push(inc_dir.to_string_lossy().into_owned());
        }
        for lib_dir in &toolchain.cpp.library_dirs {
            args.push(format!("-L{}", &lib_dir.to_string_lossy()));
        }
        for lib in &toolchain.cpp.libraries {
            args.push(format!("-l{}", &lib.to_string_lossy()));
        }
        for define in &toolchain.cpp.defines {
            args.push(format!("-D{}", define));
        }

        Ok(args)
    }

    fn get_compiler(&self) -> anyhow::Result<&Path> {
        let toolchain = self.get_toolchain()?;
        let compiler: &Path = &toolchain.cpp.compiler;
        Ok(compiler)
    }

    fn get_archiver(&self) -> anyhow::Result<&Path> {
        let toolchain = self.get_toolchain()?;
        let archiver: &Path = &toolchain.cpp.archiver;
        Ok(archiver)
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

        let cpp = arc_self
            .clone()
            .downcast_arc::<CcBinary>()
            .map_err(|_| anyhow::anyhow_loc!("Failed to downcast rule [{:?}] to CcBinary", arc_self))?;

        Ok(ctx.new_job(
            format!("Build CcBinary Target {}", self.target.target_path()),
            Box::new(move |job| build_cc_binary(cpp.clone(), job)),
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
            .map_err(|_| anyhow::anyhow_loc!("Failed to downcast rule [{:?}] to CcStaticLibrary", arc_self))?;

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

impl JobResult for CompileExeResult {}
impl JobResult for CcObjectResult {}
impl JobResult for CcObjectsResult {}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------
fn parse_cc_binary(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut cpp = CcBinary::deserialize(de).map_err(|e| anyhow::anyhow_loc!("{}", e))?;
    cpp.target = t;
    Ok(Arc::new(cpp))
}

fn parse_cc_static_library(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut lib = CcStaticLibrary::deserialize(de).map_err(|e| anyhow::anyhow_loc!("{}", e))?;
    lib.target = t;
    Ok(Arc::new(lib))
}

fn build_cc_binary(cpp: Arc<CcBinary>, mut job: Job) -> JobFnResult {
    let mode = job.ctx.mode.as_ref().unwrap(); // should have been validated previously

    // check cache
    // let job_key = JobCacheKey {
    //     mode: mode.target.clone(),
    //     target: cpp.target.clone(),
    //     substep: None
    // };

    // if let Ok(job_cache) = job.ctx.anubis.job_cache.read() {
    //     if let Some(job_id) = job_cache.get(&job_key) {
    //         if let Some(maybe_result) = job.ctx.job_system.try_get_result(*job_id) {
    //             match maybe_result {
    //                 Ok(result) => {
    //                     return JobFnResult::Success(result)
    //                 },
    //                 Err(e) => {
    //                     return JobFnResult::Error(e)
    //                 }
    //             }
    //         }
    //     }
    // } else {
    //     return JobFnResult::Error(anyhow_loc!("job_cache poisoned"));
    // }

    let mut dep_jobs: Vec<JobId> = Default::default();
    let mut extra_args: CcExtraArgs = Default::default();

    // create child job to compile each dep
    for dep in &cpp.deps {
        let dep_rule = job.ctx.anubis.get_rule(dep, &mode);

        // Build child dep
        let result = || -> anyhow::Result<()> {
            let dep_rule = job.ctx.anubis.get_rule(dep, &mode)?;
            let dep_job = dep_rule.build(dep_rule.clone(), job.ctx.clone())?;

            // Get extra args from CcStaticLibrary
            // TODO: figure out how to deal with lack of trait -> trait casting in rust :(
            if let Ok(static_lib) = dep_rule.downcast_arc::<CcStaticLibrary>() {
                extra_args.extend_static_public(&static_lib);
            }

            dep_jobs.push(dep_job.id);
            job.ctx.job_system.add_job(dep_job)?;

            Ok(())
        }();
        if let Err(e) = result {
            return JobFnResult::Error(anyhow_loc!(
                "Failed to build child dep [{}] due to error: {}",
                dep,
                e
            ));
        }
    }

    // Extend args from cpp_binary as well
    extra_args.extend_binary(&cpp);

    // create child job to compile each src
    for src in &cpp.srcs {
        let substep = build_cc_file(src.clone(), &cpp.target, job.ctx.clone(), extra_args.clone());
        match substep {
            Ok(Substep::Job(child_job)) => {
                // Add new job as a dependency
                dep_jobs.push(child_job.id);

                // Add new job
                if let Err(e) = job.ctx.job_system.add_job(child_job) {
                    return JobFnResult::Error(anyhow_loc!("{}", e));
                }
            }
            Ok(Substep::Id(child_job_id)) => {
                // Create a dependency on an existing job
                dep_jobs.push(child_job_id);
            }
            Err(e) => {
                return JobFnResult::Error(anyhow_loc!("{}", e));
            }
        }
    }

    // create a job to link all objects from child job into result
    let link_arg_jobs = dep_jobs.clone();
    let link_job = move |job: Job| -> JobFnResult {
        // link all object files into an exe
        let link_result = link_exe(&link_arg_jobs, cpp.as_ref(), job.ctx.clone(), &extra_args);
        match link_result {
            Ok(result) => result,
            Err(e) => JobFnResult::Error(anyhow_loc!("{}", e)),
        }
    };

    // Update this job to perform link
    job.desc.push_str(" (link)");
    job.job_fn = Some(Box::new(link_job));

    // Defer!
    JobFnResult::Deferred(JobDeferral {
        blocked_by: dep_jobs,
        deferred_job: job,
    })
}

fn build_cc_static_library(cpp_static_library: Arc<CcStaticLibrary>, mut job: Job) -> JobFnResult {
    let mode = job.ctx.mode.as_ref().unwrap(); // should have been validated previously

    let mut dep_jobs: Vec<JobId> = Default::default();
    let mut extra_args: CcExtraArgs = Default::default();

    // create child job to compile each dep
    for dep in &cpp_static_library.deps {
        let dep_rule = job.ctx.anubis.get_rule(dep, &mode);

        // Build child dep
        let result = || -> anyhow::Result<()> {
            let dep_rule = job.ctx.anubis.get_rule(dep, &mode)?;
            let dep_job = dep_rule.build(dep_rule.clone(), job.ctx.clone())?;

            // Get extra args from CcStaticLibrary
            // TODO: figure out how to deal with lack of trait -> trait casting in rust :(
            if let Ok(static_lib) = dep_rule.downcast_arc::<CcStaticLibrary>() {
                extra_args.extend_static_public(&static_lib);
            }

            dep_jobs.push(dep_job.id);
            job.ctx.job_system.add_job(dep_job)?;

            Ok(())
        }();
        if let Err(e) = result {
            return JobFnResult::Error(anyhow_loc!(
                "Failed to build child dep [{}] due to error: {}",
                dep,
                e
            ));
        }
    }

    extra_args.extend_static_public(&cpp_static_library);
    extra_args.extend_static_private(&cpp_static_library);

    // create child job to compile each src
    for src in &cpp_static_library.srcs {
        let substep = build_cc_file(
            src.clone(),
            &cpp_static_library.target,
            job.ctx.clone(),
            extra_args.clone(),
        );
        match substep {
            Ok(Substep::Job(child_job)) => {
                // Add new job as a dependency
                dep_jobs.push(child_job.id);

                // Run new job
                if let Err(e) = job.ctx.job_system.add_job(child_job) {
                    return JobFnResult::Error(anyhow_loc!("{}", e));
                }
            }
            Ok(Substep::Id(child_job_id)) => {
                // Create a dependency on an existing job
                dep_jobs.push(child_job_id);
            }
            Err(e) => {
                return JobFnResult::Error(anyhow_loc!("{}", e));
            }
        }
    }

    // create a job to link all objects from child jobs into result
    let archive_arg_jobs = dep_jobs.clone();
    let archive_job = move |job: Job| -> JobFnResult {
        // archive all object files into a static library
        let archive_result =
            archive_static_library(&archive_arg_jobs, cpp_static_library.as_ref(), job.ctx.clone());
        match archive_result {
            Ok(result) => result,
            Err(e) => JobFnResult::Error(anyhow_loc!("{}", e)),
        }
    };

    // Update this job to perform archive
    job.desc.push_str(" (create archive)");
    job.job_fn = Some(Box::new(archive_job));

    // Defer!
    JobFnResult::Deferred(JobDeferral {
        blocked_by: dep_jobs,
        deferred_job: job,
    })
}

fn build_cc_file(
    src_path: PathBuf,
    target: &AnubisTarget,
    ctx: Arc<JobContext>,
    extra_args: CcExtraArgs,
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
    let job_fn = move |job| {
        let result = || -> anyhow::Result<JobFnResult> {
            // Get initial args args
            let mut args = ctx2.get_args()?;

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
            let output_file = ctx2
                .anubis
                .root
                .join(".anubis-build")
                .join(&ctx2.mode.as_ref().unwrap().name)
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
            let compiler = ctx2.get_compiler()?;

            tracing::trace!(
                source_file = %src2,
                compiler_args = ?args,
                "Executing compiler command"
            );

            let compile_start = std::time::Instant::now();
            let output = std::process::Command::new(&compiler)
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output();

            let compile_duration = compile_start.elapsed();

            match output {
                Ok(o) => {
                    if o.status.success() {
                        Ok(JobFnResult::Success(Arc::new(CcObjectResult {
                            object_path: output_file,
                        })))
                    } else {
                        tracing::error!(
                            source_file = %src2,
                            exit_code = o.status.code(),
                            compile_time_ms = compile_duration.as_millis(),
                            stdout = %String::from_utf8_lossy(&o.stdout),
                            stderr = %String::from_utf8_lossy(&o.stderr),
                            "Compilation failed"
                        );

                        Ok(JobFnResult::Error(anyhow_loc!("Command completed with error status [{}].\n  Args: {}\n  stdout: {}\n  stderr: {}", o.status, args.join(" "), String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
                    }
                }
                Err(e) => {
                    tracing::error!(
                        source_file = %src2,
                        compiler = %compiler.display(),
                        error = %e,
                        "Compiler execution failed"
                    );

                    Ok(JobFnResult::Error(anyhow_loc!(
                        "Command failed unexpectedly\n  Proc: [{:?}]\n  Cmd: [{:#?}]\n  Err: [{}]",
                        &compiler,
                        &args,
                        e
                    )))
                }
            }
        }();

        match result {
            Ok(r) => r,
            Err(e) => JobFnResult::Error(anyhow_loc!(
                "Failed to compile.\n    Src: [{:?}]    \n  Error: [{:?}]",
                src2,
                e
            )),
        }
    };

    Ok(Substep::Job(ctx.new_job_with_id(
        job_id,
        format!("Compile cpp file [{}]", src),
        Box::new(job_fn),
    )))
}

fn archive_static_library(
    object_jobs: &[JobId],
    cpp_static_library: &CcStaticLibrary,
    ctx: Arc<JobContext>,
) -> anyhow::Result<JobFnResult> {
    // Get all child jobs
    let mut link_args: Vec<Arc<CcObjectResult>> = Default::default();
    for link_arg_job in object_jobs {
        // TODO: make fallible
        let job_result = ctx.job_system.expect_result::<CcObjectResult>(*link_arg_job)?;
        link_args.push(job_result);
    }

    // Build args
    let mut args: Vec<String> = Default::default();
    args.push("rcs".to_owned());

    // Compute output filepath
    let relpath = cpp_static_library.target.get_relative_dir();
    let build_dir =
        ctx.anubis.root.join(".anubis-build").join(&ctx.mode.as_ref().unwrap().name).join(relpath);
    ensure_directory(&build_dir)?;

    let output_file = build_dir.join(&cpp_static_library.name).with_extension("lib").slash_fix();
    args.push(output_file.to_string_lossy().to_string());

    // put link args in a response file
    let response_filepath = build_dir.join(&cpp_static_library.name).with_extension("rsp").slash_fix();

    let link_args_str: String = link_args.iter().map(|p| p.object_path.to_string_lossy()).join(" ");
    std::fs::write(&response_filepath, &link_args_str).with_context(|| {
        format!(
            "Failed to write link args into response file: [{:?}]",
            response_filepath
        )
    })?;
    args.push(format!("@{}", response_filepath.to_string_lossy()));

    tracing::trace!(
        target = %cpp_static_library.target.target_path(),
        linker_args = ?args,
        "Executing archiver command"
    );

    // run the command
    let archiver = ctx.get_archiver()?;
    let output = std::process::Command::new(&archiver)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    match output {
        Ok(o) => {
            if o.status.success() {
                Ok(JobFnResult::Success(Arc::new(CcObjectResult {
                    object_path: output_file,
                })))
            } else {
                tracing::error!(
                    target = %cpp_static_library.target.target_path(),
                    binary_name = %cpp_static_library.name,
                    exit_code = o.status.code(),
                    stdout = %String::from_utf8_lossy(&o.stdout),
                    stderr = %String::from_utf8_lossy(&o.stderr),
                    "Archive creation failed"
                );

                Ok(JobFnResult::Error(anyhow_loc!(
                    "Archive command completed with error status [{}].\n  Args: {}\n  stdout: {}\n  stderr: {}",
                    o.status,
                    args.join(" ") + " " + &link_args_str,
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )))
            }
        }
        Err(e) => {
            tracing::error!(
                target = %cpp_static_library.target.target_path(),
                binary_name = %cpp_static_library.name,
                archiver = %archiver.display(),
                error = %e,
                "Archiver execution failed"
            );

            Ok(JobFnResult::Error(anyhow_loc!(
                "Command failed unexpectedly\n  Proc: [{:?}]\n  Cmd: [{:#?}]\n  Err: [{}]",
                &archiver,
                &args,
                e
            )))
        }
    }
}

fn link_exe(
    link_arg_jobs: &[JobId],
    cpp: &CcBinary,
    ctx: Arc<JobContext>,
    extra_args: &CcExtraArgs,
) -> anyhow::Result<JobFnResult> {
    // Get all child jobs
    let mut link_args: Vec<PathBuf> = Default::default();
    for link_arg_job in link_arg_jobs {
        let job_result = ctx.job_system.get_result(*link_arg_job)?;
        if let Ok(r) = job_result.cast::<LinkArgsResult>() {
            link_args.push(r.filepath.clone());
        } else if let Ok(r) = job_result.cast::<CcObjectResult>() {
            link_args.push(r.object_path.clone());
        } else if let Ok(r) = job_result.cast::<CcObjectsResult>() {
            link_args.extend(r.object_paths.iter().cloned());
        } else {
            bail_loc!(
                "Unknown dependency result type: [{}]",
                std::any::type_name_of_val(&job_result)
            )
        }
    }

    // Build link command
    let mut args = ctx.get_args()?;

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
    let relpath = cpp.target.get_relative_dir();
    let output_file = ctx
        .anubis
        .root
        .join(".anubis-out")
        .join(&ctx.mode.as_ref().unwrap().name)
        .join(relpath)
        .join(&cpp.name)
        .with_extension("exe")
        .slash_fix();
    ensure_directory_for_file(&output_file)?;

    args.push("-o".into());
    args.push(output_file.to_string_lossy().into());

    let compiler_path = ctx.get_compiler()?;
    // Don't log link command start - already logged above

    tracing::trace!(
        target = %cpp.target.target_path(),
        linker_args = ?args,
        "Executing linker command"
    );

    // run the command
    let compiler = ctx.get_compiler()?;
    let link_start = std::time::Instant::now();
    let output = std::process::Command::new(&compiler)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    let link_duration = link_start.elapsed();

    match output {
        Ok(o) => {
            if o.status.success() {
                let binary_size = output_file.metadata().map(|m| m.len()).unwrap_or(0);
                Ok(JobFnResult::Success(Arc::new(CompileExeResult { output_file })))
            } else {
                tracing::error!(
                    target = %cpp.target.target_path(),
                    binary_name = %cpp.name,
                    exit_code = o.status.code(),
                    link_time_ms = link_duration.as_millis(),
                    stdout = %String::from_utf8_lossy(&o.stdout),
                    stderr = %String::from_utf8_lossy(&o.stderr),
                    "Linking failed"
                );

                Ok(JobFnResult::Error(anyhow_loc!(
                    "Command completed with error status [{}].\n  Args: {}\n  stdout: {}\n  stderr: {}",
                    o.status,
                    args.join(" "),
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )))
            }
        }
        Err(e) => {
            tracing::error!(
                target = %cpp.target.target_path(),
                binary_name = %cpp.name,
                linker = %compiler.display(),
                error = %e,
                "Linker execution failed"
            );

            Ok(JobFnResult::Error(anyhow_loc!(
                "Command failed unexpectedly [{}]",
                e
            )))
        }
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
