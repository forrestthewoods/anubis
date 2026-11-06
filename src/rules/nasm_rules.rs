//! NASM rules: `nasm_objects` (objects linked straight into a dependent) and
//! `nasm_static_library`. Assembling is cached like a C/C++ compile, using
//! NASM's `-MD` dependency file.

use crate::build_cache::{remove_outputs, ActionOutput};
use crate::rules::cc_rules::{validate_hermetic_deps, CcObjectArtifact, CcObjectsArtifact};
use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use serde::Deserialize;
use std::sync::Arc;

use crate::anubis::{self, AnubisTarget};
use crate::{job_system::*};
use crate::rules::rule_utils::{ensure_directory, ensure_directory_for_file, run_command_verbose};
use crate::util::{self, SlashFix};
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
        let nasm = arc_self
            .clone()
            .downcast_arc::<NasmObjects>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to NasmObjects", arc_self))?;

        let target_name = self.target.target_name().to_string();
        let target_path = self.target.target_path().to_string();
        Ok(ctx.new_job(
            format!("Build NasmObjects Target {}", &target_path),
            JobDisplayInfo { verb: "Building", short_name: target_name, detail: target_path },
            Box::new(move |job| build_nasm_objects(nasm.clone(), job)),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        Ok(vec![])
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

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        Ok(vec![])
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
    let opts = Arc::new(NasmOptions {
        target: nasm.target.clone(),
        include_dirs: nasm.include_dirs.clone(),
        preincludes: nasm.preincludes.clone(),
    });
    let dep_job_ids = queue_assemble_jobs(&job, &nasm.srcs, opts)?;

    // Continuation: collect the objects.
    let ctx = job.ctx.clone();
    let ids = dep_job_ids.clone();
    let aggregate_job = move |_agg_job: Job| -> anyhow::Result<JobOutcome> {
        let mut object_paths: Vec<Utf8PathBuf> = Default::default();
        for id in ids {
            object_paths.push(ctx.job_system.expect_result::<CcObjectArtifact>(id)?.object_path.clone());
        }
        Ok(JobOutcome::Success(Arc::new(CcObjectsArtifact { object_paths })))
    };
    let agg_display = JobDisplayInfo {
        verb: "Aggregating",
        short_name: job.display.short_name.clone(),
        detail: job.display.detail.clone(),
    };
    let continuation_job = job.ctx.new_job(format!("{} (aggregate)", job.desc), agg_display, Box::new(aggregate_job));

    Ok(JobOutcome::Deferred(JobDeferral { blocked_by: dep_job_ids, continuation_job }))
}

fn parse_nasm_static_library(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut nasm = NasmStaticLibrary::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    nasm.target = t;
    Ok(Arc::new(nasm))
}

fn build_nasm_static_library(nasm: Arc<NasmStaticLibrary>, job: Job) -> anyhow::Result<JobOutcome> {
    let opts = Arc::new(NasmOptions {
        target: nasm.target.clone(),
        include_dirs: nasm.include_dirs.clone(),
        preincludes: nasm.preincludes.clone(),
    });
    let dep_job_ids = queue_assemble_jobs(&job, &nasm.srcs, opts)?;

    // Continuation: archive the objects into a static library.
    let archive_job_ids = dep_job_ids.clone();
    let nasm_for_archive = nasm.clone();
    let archive_job = move |archive_job: Job| -> anyhow::Result<JobOutcome> {
        archive_nasm_static_library(&archive_job_ids, nasm_for_archive.as_ref(), archive_job.ctx.clone())
    };
    let archive_display = JobDisplayInfo {
        verb: "Archiving",
        short_name: job.display.short_name.clone(),
        detail: job.display.detail.clone(),
    };
    let continuation_job =
        job.ctx.new_job(format!("{} (create archive)", job.desc), archive_display, Box::new(archive_job));

    Ok(JobOutcome::Deferred(JobDeferral { blocked_by: dep_job_ids, continuation_job }))
}

/// What both NASM rules need to assemble a source file.
struct NasmOptions {
    target: AnubisTarget,
    include_dirs: Vec<Utf8PathBuf>,
    preincludes: Vec<Utf8PathBuf>,
}

/// Queue one assemble job per source. Returns their ids.
fn queue_assemble_jobs(job: &Job, srcs: &[Utf8PathBuf], nasm: Arc<NasmOptions>) -> anyhow::Result<Vec<JobId>> {
    let mut ids = Vec::with_capacity(srcs.len());
    for src in srcs {
        let nasm = nasm.clone();
        let ctx = job.ctx.clone();
        let src2 = src.clone();
        let job_fn = move |_j: Job| -> anyhow::Result<JobOutcome> { nasm_assemble(&nasm, ctx, &src2) };
        let filename = src.file_name().unwrap_or(src.as_str()).to_string();
        let display = JobDisplayInfo { verb: "Assembling", short_name: filename, detail: src.to_string() };
        let dep_job = job.ctx.new_job(format!("nasm [{:?}]", src), display, Box::new(job_fn));
        ids.push(dep_job.id);
        job.ctx.job_system.add_job(dep_job)?;
    }
    Ok(ids)
}

/// Assemble one file. Cached like a C/C++ compile: the manifest key covers
/// assembler, argv and source; the files NASM read (its `-MD` output) are
/// checked against the manifest entries.
fn nasm_assemble(nasm: &NasmOptions, ctx: Arc<JobContext>, src_abspath: &Utf8Path) -> anyhow::Result<JobOutcome> {
    let toolchain = ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref();
    let assembler = &toolchain.nasm()?.assembler;

    let config_root = nasm.target.get_config_absdir(&ctx.anubis.root);
    let src_relpath = util::strip_prefix(&src_abspath, &config_root)?;
    let build_dir = ctx.anubis.out_dir(&ctx.mode, &nasm.target, "nasm").slash_fix();
    let output_file = build_dir.join(&src_relpath).with_added_extension("obj").slash_fix(); // foo.asm -> foo.asm.obj
    ensure_directory_for_file(output_file.as_ref())?;
    let dep_file = output_file.with_extension("d");

    let mut args: Vec<String> = vec!["-f".to_owned(), toolchain.nasm()?.output_format.clone()];
    for inc in &nasm.include_dirs {
        args.push("-I".to_owned());
        args.push(format!("{}/", inc)); // NASM requires the trailing slash
    }
    for preinclude in &nasm.preincludes {
        args.push("-P".to_owned());
        args.push(preinclude.to_string());
    }
    args.push(src_abspath.to_string());
    args.push("-MD".to_owned());
    args.push(dep_file.to_string());
    args.push("-o".to_owned());
    args.push(output_file.to_string());

    let success = || -> anyhow::Result<JobOutcome> {
        Ok(JobOutcome::Success(Arc::new(CcObjectArtifact { object_path: output_file.clone() })))
    };
    let src = [src_abspath.to_owned()];

    let cache = ctx.anubis.build_cache.as_ref();
    let manifest_key = match cache {
        Some(cache) => {
            let mut kb = cache.key_for_command("nasm", assembler, &args, Some(&build_dir))?;
            cache.key_add_files(&mut kb, &src)?.then(|| kb.finish())
        }
        None => None,
    };
    if let (Some(cache), Some(mk)) = (cache, manifest_key.as_ref()) {
        if let Some(result_key) = cache.lookup_manifest(mk)? {
            if cache.restore(&result_key, &build_dir)? {
                cache.record_hit();
                return success();
            }
        }
        cache.record_miss();
        tracing::debug!("cache miss: {output_file}");
    }

    remove_outputs([output_file.as_path()])?;
    let output = run_command_verbose(assembler.as_ref(), &args, ctx.anubis.verbose_tools)?;
    if !output.status.success() {
        tracing::error!(
            source_file = %src_abspath,
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

    let dep_paths = validate_hermetic_deps(&dep_file, &ctx.anubis.root)?;
    if let (Some(cache), Some(mk)) = (cache, manifest_key.as_ref()) {
        // Don't cache if the source changed while NASM ran.
        cache.invalidate(src_abspath);
        let mut kb = cache.key_for_command("nasm", assembler, &args, Some(&build_dir))?;
        if cache.key_add_files(&mut kb, &src)? && kb.finish() == *mk {
            if let Some((result_key, inputs)) = cache.compile_result_key(mk, &dep_paths, &[])? {
                cache.store(&result_key, &build_dir, &[ActionOutput { path: output_file.clone(), optional: false }])?;
                cache.record_manifest(mk, inputs, &result_key)?;
            }
        }
    }
    success()
}

fn archive_nasm_static_library(
    object_jobs: &[JobId],
    nasm_static_lib: &NasmStaticLibrary,
    ctx: Arc<JobContext>,
) -> anyhow::Result<JobOutcome> {
    let toolchain = ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref();
    let archiver = &toolchain.nasm()?.archiver;

    let mut object_files: Vec<Utf8PathBuf> = Default::default();
    for job_id in object_jobs {
        object_files.push(ctx.job_system.expect_result::<CcObjectArtifact>(*job_id)?.object_path.clone());
    }

    let build_dir = ctx.anubis.out_dir(&ctx.mode, &nasm_static_lib.target, "lib").slash_fix();
    ensure_directory(build_dir.as_ref())?;
    let extension = match toolchain.nasm()?.output_format.as_str() {
        "win64" | "win32" => "lib",
        _ => "a", // elf64, elf32, macho64, etc.
    };
    let output_file = build_dir.join(&nasm_static_lib.name).with_extension(extension).slash_fix();
    let response_filepath = build_dir.join(&nasm_static_lib.name).with_extension("rsp").slash_fix();

    let mode = if ctx.anubis.verbose_tools { "rcsv" } else { "rcs" };
    let args = vec![mode.to_owned(), output_file.to_string(), format!("@{}", response_filepath)];
    let success = || -> anyhow::Result<JobOutcome> {
        Ok(JobOutcome::Success(Arc::new(CcObjectArtifact { object_path: output_file.clone() })))
    };

    // Cache: archiver + args + the ordered object digests.
    let cache = ctx.anubis.build_cache.as_ref();
    let cache_key = match cache {
        Some(cache) => {
            let mut kb = cache.key_for_command("nasm_archive", archiver, &args, Some(&build_dir))?;
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
    }

    // llvm-ar updates an existing archive in place, so start fresh.
    remove_outputs([output_file.as_path()])?;
    let object_args_str: String = object_files.iter().map(|p| p.as_str()).join(" ");
    std::fs::write(&response_filepath, &object_args_str)
        .with_context(|| format!("Failed to write object args into response file: [{:?}]", response_filepath))?;

    let output = run_command_verbose(archiver.as_ref(), &args, ctx.anubis.verbose_tools)?;
    if !output.status.success() {
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
    if let (Some(cache), Some(key)) = (cache, cache_key.as_ref()) {
        cache.store(key, &build_dir, &[ActionOutput { path: output_file.clone(), optional: false }])?;
    }
    success()
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
