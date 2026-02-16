use crate::rules::cc_rules::{CcObjectArtifact, CcObjectsArtifact};
use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use serde::Deserialize;
use std::sync::Arc;

use crate::anubis::{self, AnubisTarget};
use crate::job_system::*;
use crate::rules::rule_utils::{ensure_directory, ensure_directory_for_file, run_command_verbose};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};
use anyhow::Context;
use serde::{de, Deserializer};

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NasmObjects {
    pub name: String,
    pub srcs: Vec<Utf8PathBuf>,

    #[serde(default)]
    pub include_dirs: Vec<Utf8PathBuf>,

    /// Files to pre-include before each source file (NASM -P flag)
    #[serde(default)]
    pub preincludes: Vec<Utf8PathBuf>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NasmStaticLibrary {
    pub name: String,
    pub srcs: Vec<Utf8PathBuf>,

    #[serde(default)]
    pub include_dirs: Vec<Utf8PathBuf>,

    /// Files to pre-include before each source file (NASM -P flag)
    #[serde(default)]
    pub preincludes: Vec<Utf8PathBuf>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

// ----------------------------------------------------------------------------
// Trait Implementations
// ----------------------------------------------------------------------------
impl anubis::Rule for NasmObjects {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let cpp = arc_self
            .clone()
            .downcast_arc::<NasmObjects>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to NasmObjects", arc_self))?;

        let target_name = self.target.target_name().to_string();
        let target_path = self.target.target_path().to_string();
        Ok(ctx.new_job(
            format!("Build NasmObjects Target {}", &target_path),
            JobDisplayInfo { verb: "Building", short_name: target_name, detail: target_path },
            Box::new(move |job| build_nasm_objects(cpp.clone(), job)),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<()> {
        Ok(())
    }
}

impl anubis::Rule for NasmStaticLibrary {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        bail_loc_if!(
            ctx.mode.is_none(),
            "Can not create NasmStaticLibrary job without a mode"
        );

        let nasm = arc_self
            .clone()
            .downcast_arc::<NasmStaticLibrary>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to NasmStaticLibrary", arc_self))?;

        let target_name = self.target.target_name().to_string();
        let target_path = self.target.target_path().to_string();
        Ok(ctx.new_job(
            format!("Build NasmStaticLibrary Target {}", &target_path),
            JobDisplayInfo { verb: "Building", short_name: target_name, detail: target_path },
            Box::new(move |job| build_nasm_static_library(nasm.clone(), job)),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<()> {
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------
fn parse_nasm_objects(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut nasm = NasmObjects::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    nasm.target = t;
    Ok(Arc::new(nasm))
}

fn build_nasm_objects(nasm: Arc<NasmObjects>, job: Job) -> anyhow::Result<JobOutcome> {
    // create child job for each object
    let mut dep_job_ids: Vec<JobId> = Default::default();
    for src in &nasm.srcs {
        // create job fn
        let nasm2 = nasm.clone();
        let ctx = job.ctx.clone();
        let src2 = src.clone();
        let job_fn = move |_j: Job| -> anyhow::Result<JobOutcome> { nasm_assemble(nasm2, ctx, &src2) };

        // create job
        let filename = src.file_name().unwrap_or(src.as_str()).to_string();
        let asm_display = JobDisplayInfo { verb: "Assembling", short_name: filename, detail: src.to_string() };
        let dep_job = job.ctx.new_job(format!("nasm [{:?}]", src), asm_display, Box::new(job_fn));

        // Store job_id and queue job
        dep_job_ids.push(dep_job.id);
        job.ctx.job_system.add_job(dep_job)?;
    }

    // create continuation job to aggregate results
    let aggregate_job_ids = dep_job_ids.clone();
    let ctx = job.ctx.clone();
    let aggregate_job_ids2 = aggregate_job_ids.clone();
    let aggregate_job = move |_agg_job: Job| -> anyhow::Result<JobOutcome> {
        let mut object_paths: Vec<Utf8PathBuf> = Default::default();
        for agg_id in aggregate_job_ids2 {
            let job_result = ctx.job_system.expect_result::<CcObjectArtifact>(agg_id)?;
            object_paths.push(job_result.object_path.clone());
        }

        Ok(JobOutcome::Success(Arc::new(CcObjectsArtifact { object_paths })))
    };

    // Create continuation job to perform aggregation
    let agg_display = JobDisplayInfo {
        verb: "Aggregating",
        short_name: job.display.short_name.clone(),
        detail: job.display.detail.clone(),
    };
    let continuation_job = job.ctx.new_job(format!("{} (aggregate)", job.desc), agg_display, Box::new(aggregate_job));

    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: aggregate_job_ids,
        continuation_job,
    }))
}

fn nasm_assemble(nasm: Arc<NasmObjects>, ctx: Arc<JobContext>, src: &Utf8Path) -> anyhow::Result<JobOutcome> {
    // get toolchain
    let toolchain = ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref();
    let assembler = &toolchain.nasm.assembler;

    // compute some paths
    let src_filename = src.file_name().ok_or_else(|| anyhow_loc!("No filename for [{:?}]", src))?;
    let relpath = pathdiff::diff_paths(src.as_std_path(), ctx.anubis.root.as_std_path())
        .ok_or_else(|| anyhow_loc!("Could not relpath from [{:?}] to [{:?}]", &ctx.anubis.root, &src))?;
    let relpath = Utf8PathBuf::try_from(relpath)
        .map_err(|e| anyhow_loc!("Non-UTF8 path from diff_paths: {:?}", e))?;

    let mode_name = &ctx.mode.as_ref().unwrap().name;
    let object_path = ctx
        .anubis
        .build_dir(mode_name)
        .join(&relpath)
        .with_added_extension("obj") // result: foo.asm -> foo.asm.obj; avoid conflict with foo.c -> foo.obj
        .slash_fix();
    ensure_directory_for_file(object_path.as_ref())?;

    let mut args: Vec<String> = Default::default();
    args.push("-f".to_owned());
    args.push(toolchain.nasm.output_format.clone());

    // Add include paths from the rule
    for inc in &nasm.include_dirs {
        args.push("-I".to_owned());
        args.push(format!("{}/", inc)); // NASM requires trailing slash
    }

    // Add pre-include files (like config.asm for FFmpeg)
    for preinclude in &nasm.preincludes {
        args.push("-P".to_owned());
        args.push(preinclude.to_string());
    }

    args.push(src.to_string()); // input file
    args.push("-o".to_owned());
    args.push(object_path.to_string());

    let verbose = ctx.anubis.verbose_tools;
    let output = run_command_verbose(assembler.as_ref(), &args, verbose)?;

    if output.status.success() {
        Ok(JobOutcome::Success(Arc::new(CcObjectArtifact { object_path })))
    } else {
        tracing::error!(
            source_file = %src,
            exit_code = output.status.code(),
            stdout = %String::from_utf8_lossy(&output.stdout),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "Assembly failed"
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

fn parse_nasm_static_library(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut nasm = NasmStaticLibrary::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    nasm.target = t;
    Ok(Arc::new(nasm))
}

fn build_nasm_static_library(nasm: Arc<NasmStaticLibrary>, job: Job) -> anyhow::Result<JobOutcome> {
    // create child job for each source file to assemble
    let mut dep_job_ids: Vec<JobId> = Default::default();
    for src in &nasm.srcs {
        let nasm2 = nasm.clone();
        let ctx = job.ctx.clone();
        let src2 = src.clone();
        let job_fn =
            move |_j: Job| -> anyhow::Result<JobOutcome> { nasm_assemble_static_lib(&nasm2, ctx, &src2) };

        let filename = src.file_name().unwrap_or(src.as_str()).to_string();
        let asm_display = JobDisplayInfo { verb: "Assembling", short_name: filename, detail: src.to_string() };
        let dep_job = job.ctx.new_job(format!("nasm [{:?}]", src), asm_display, Box::new(job_fn));
        dep_job_ids.push(dep_job.id);
        job.ctx.job_system.add_job(dep_job)?;
    }

    // create a continuation job to archive all object files into a static library
    let archive_job_ids = dep_job_ids.clone();
    let nasm_for_archive = nasm.clone();
    let archive_job = move |archive_job: Job| -> anyhow::Result<JobOutcome> {
        archive_nasm_static_library(
            &archive_job_ids,
            nasm_for_archive.as_ref(),
            archive_job.ctx.clone(),
        )
    };

    // Create continuation job to perform archive
    let archive_display = JobDisplayInfo {
        verb: "Archiving",
        short_name: job.display.short_name.clone(),
        detail: job.display.detail.clone(),
    };
    let continuation_job = job.ctx.new_job(format!("{} (create archive)", job.desc), archive_display, Box::new(archive_job));

    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: dep_job_ids,
        continuation_job,
    }))
}

fn nasm_assemble_static_lib(
    nasm: &NasmStaticLibrary,
    ctx: Arc<JobContext>,
    src: &Utf8Path,
) -> anyhow::Result<JobOutcome> {
    let toolchain = ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref();
    let assembler = &toolchain.nasm.assembler;

    let relpath = pathdiff::diff_paths(src.as_std_path(), ctx.anubis.root.as_std_path())
        .ok_or_else(|| anyhow_loc!("Could not relpath from [{:?}] to [{:?}]", &ctx.anubis.root, &src))?;
    let relpath = Utf8PathBuf::try_from(relpath)
        .map_err(|e| anyhow_loc!("Non-UTF8 path from diff_paths: {:?}", e))?;

    let object_path = ctx
        .anubis
        .root
        .join(".anubis-build")
        .join(&ctx.mode.as_ref().unwrap().name)
        .join(&relpath)
        .with_added_extension("obj")
        .slash_fix();
    ensure_directory_for_file(object_path.as_ref())?;

    let mut args: Vec<String> = Default::default();
    args.push("-f".to_owned());
    args.push(toolchain.nasm.output_format.clone());

    for inc in &nasm.include_dirs {
        args.push("-I".to_owned());
        args.push(format!("{}/", inc));
    }

    for preinclude in &nasm.preincludes {
        args.push("-P".to_owned());
        args.push(preinclude.to_string());
    }

    args.push(src.to_string());
    args.push("-o".to_owned());
    args.push(object_path.to_string());

    let verbose = ctx.anubis.verbose_tools;
    let output = run_command_verbose(assembler.as_ref(), &args, verbose)?;

    if output.status.success() {
        Ok(JobOutcome::Success(Arc::new(CcObjectArtifact { object_path })))
    } else {
        tracing::error!(
            source_file = %src,
            exit_code = output.status.code(),
            stdout = %String::from_utf8_lossy(&output.stdout),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "NASM assembly failed"
        );

        bail_loc!(
            "NASM command completed with error status [{}].\n  Args: {}\n  stdout: {}\n  stderr: {}",
            output.status,
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

fn archive_nasm_static_library(
    object_jobs: &[JobId],
    nasm_static_lib: &NasmStaticLibrary,
    ctx: Arc<JobContext>,
) -> anyhow::Result<JobOutcome> {
    let toolchain = ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref();
    let archiver = &toolchain.nasm.archiver;

    // Get all object files from child jobs
    let mut object_paths: Vec<Arc<CcObjectArtifact>> = Default::default();
    for job_id in object_jobs {
        let job_result = ctx.job_system.expect_result::<CcObjectArtifact>(*job_id)?;
        object_paths.push(job_result);
    }

    // Build archiver args
    let mut args: Vec<String> = Default::default();
    // Use "rcsv" for verbose output if enabled, otherwise "rcs"
    if ctx.anubis.verbose_tools {
        args.push("rcsv".to_owned());
    } else {
        args.push("rcs".to_owned());
    }

    // Compute output filepath - use .lib on Windows (win64), .a on Linux (elf64)
    let relpath = nasm_static_lib.target.get_relative_dir();
    let build_dir =
        ctx.anubis.root.join(".anubis-build").join(&ctx.mode.as_ref().unwrap().name).join(relpath);
    ensure_directory(build_dir.as_ref())?;

    let extension = match toolchain.nasm.output_format.as_str() {
        "win64" | "win32" => "lib",
        _ => "a", // elf64, elf32, macho64, etc.
    };

    let output_file = build_dir.join(&nasm_static_lib.name).with_extension(extension).slash_fix();
    args.push(output_file.to_string());

    // Put object file args in a response file
    let response_filepath = build_dir.join(&nasm_static_lib.name).with_extension("rsp").slash_fix();

    let object_args_str: String = object_paths.iter().map(|p| p.object_path.as_str()).join(" ");
    std::fs::write(&response_filepath, &object_args_str).with_context(|| {
        format!(
            "Failed to write object args into response file: [{:?}]",
            response_filepath
        )
    })?;
    args.push(format!("@{}", response_filepath));

    // Run the archiver command
    let verbose = ctx.anubis.verbose_tools;
    let output = run_command_verbose(archiver.as_ref(), &args, verbose)?;

    if output.status.success() {
        Ok(JobOutcome::Success(Arc::new(CcObjectArtifact {
            object_path: output_file,
        })))
    } else {
        tracing::error!(
            target = %nasm_static_lib.target.target_path(),
            library_name = %nasm_static_lib.name,
            exit_code = output.status.code(),
            stdout = %String::from_utf8_lossy(&output.stdout),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "NASM static library archive creation failed"
        );

        bail_loc!(
            "Archive command completed with error status [{}].\n  Args: {}\n  stdout: {}\n  stderr: {}",
            output.status,
            args.join(" ") + " " + &object_args_str,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

// ----------------------------------------------------------------------------
// Public Functions
// ----------------------------------------------------------------------------
pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("nasm_objects".to_owned()),
        parse_rule: parse_nasm_objects,
    })?;

    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("nasm_static_library".to_owned()),
        parse_rule: parse_nasm_static_library,
    })?;

    Ok(())
}
