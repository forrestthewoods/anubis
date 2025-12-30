use cpp_rules::{CcObjectResult, CcObjectsResult};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::anubis::{self, AnubisTarget};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};
use crate::{cpp_rules, job_system::*};
use serde::{de, Deserializer};

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NasmObjects {
    pub name: String,
    pub srcs: Vec<PathBuf>,

    #[serde(default)]
    pub include_dirs: Vec<PathBuf>,

    /// Files to pre-include before each source file (NASM -P flag)
    #[serde(default)]
    pub preincludes: Vec<PathBuf>,

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

        Ok(ctx.new_job(
            format!("Build NasmObjects Target {}", self.target.target_path()),
            Box::new(move |job| build_nasm_objects(cpp.clone(), job)),
        ))
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

fn build_nasm_objects(nasm: Arc<NasmObjects>, mut job: Job) -> JobFnResult {
    // create child job for each object
    let mut dep_job_ids: Vec<JobId> = Default::default();
    for src in &nasm.srcs {
        let result = || -> anyhow::Result<()> {
            // create job fn
            let nasm2 = nasm.clone();
            let ctx = job.ctx.clone();
            let src2 = src.clone();
            let job_fn = move |j: Job| -> JobFnResult { nasm_assemble(nasm2, ctx, &src2) };

            // create job
            let dep_job = job.ctx.new_job(format!("nasm [{:?}]", src), Box::new(job_fn));

            // Store job_id and queue job
            dep_job_ids.push(dep_job.id);
            job.ctx.job_system.add_job(dep_job)?;

            Ok(())
        }();

        if let Err(e) = result {
            return JobFnResult::Error(anyhow_loc!(
                "Failed to build child src [{:?}] due to error: {}",
                src,
                e
            ));
        }
    }

    // create mini-job to aggregate results
    let aggregate_job_ids = dep_job_ids.clone();
    let ctx = job.ctx.clone();
    let aggregate_job_ids2 = aggregate_job_ids.clone();
    let aggregate_job = move |job: Job| -> JobFnResult {
        let result = || -> anyhow::Result<_> {
            let mut object_paths: Vec<PathBuf> = Default::default();
            for agg_id in aggregate_job_ids2 {
                // TODO: make fallible
                let job_result = ctx.job_system.expect_result::<CcObjectResult>(agg_id)?;
                object_paths.push(job_result.object_path.clone());
            }
            Ok(object_paths)
        }();

        match result {
            Ok(object_paths) => JobFnResult::Success(Arc::new(CcObjectsResult { object_paths })),
            Err(e) => JobFnResult::Error(e),
        }
    };

    job.desc.push_str(" (aggregate)");
    job.job_fn = Some(Box::new(aggregate_job));

    JobFnResult::Deferred(JobDeferral {
        blocked_by: aggregate_job_ids,
        deferred_job: job,
    })
}

fn nasm_assemble(nasm: Arc<NasmObjects>, ctx: Arc<JobContext>, src: &Path) -> JobFnResult {
    let result = || -> anyhow::Result<PathBuf> {
        // get toolchain
        let toolchain = ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref();
        let assembler = &toolchain.nasm.assembler;

        // compute some paths
        let src_filename = src.file_name().ok_or_else(|| anyhow_loc!("No filename for [{:?}]", src))?;
        let relpath = pathdiff::diff_paths(&src, &ctx.anubis.root)
            .ok_or_else(|| anyhow_loc!("Could not relpath from [{:?}] to [{:?}]", &ctx.anubis.root, &src))?;

        let output_filepath = ctx
            .anubis
            .root
            .join(".anubis-build")
            .join(&ctx.mode.as_ref().unwrap().name)
            .join(relpath)
            .with_extension("o")
            .slash_fix();
        cpp_rules::ensure_directory_for_file(&output_filepath)?;

        let mut args: Vec<String> = Default::default();
        args.push("-f".to_owned());
        args.push(toolchain.nasm.output_format.clone());

        // Add include paths from the rule
        for inc in &nasm.include_dirs {
            args.push("-I".to_owned());
            args.push(format!("{}/", inc.to_string_lossy())); // NASM requires trailing slash
        }

        // Add pre-include files (like config.asm for FFmpeg)
        for preinclude in &nasm.preincludes {
            args.push("-P".to_owned());
            args.push(preinclude.to_string_lossy().into());
        }

        args.push(src.to_string_lossy().into()); // input file
        args.push("-o".to_owned());
        args.push(output_filepath.to_string_lossy().into());

        let output = std::process::Command::new(assembler)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();

        match output {
            Ok(o) => {
                if o.status.success() {
                    Ok(output_filepath)
                } else {
                    tracing::error!(
                        source_file = %src.to_string_lossy(),
                        exit_code = o.status.code(),
                        stdout = %String::from_utf8_lossy(&o.stdout),
                        stderr = %String::from_utf8_lossy(&o.stderr),
                        "Assembly failed"
                    );

                    bail_loc!("Command completed with error status [{}].\n  Args: {:#?}\n  stdout: {}\n  stderr: {}", o.status, args, String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
                }
            }
            Err(e) => {
                tracing::error!(
                    source_file = %src.display(),
                    compiler = %assembler.display(),
                    error = %e,
                    "Compiler execution failed"
                );

                bail_loc!(
                    "Command failed unexpectedly\n  Proc: [{:?}]\n  Cmd: [{:#?}]\n  Err: [{}]",
                    &assembler,
                    &args,
                    e
                )
            }
        }
    }();

    match result {
        Ok(object_path) => JobFnResult::Success(Arc::new(CcObjectResult { object_path })),
        Err(e) => JobFnResult::Error(e),
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

    Ok(())
}
