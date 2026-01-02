//! Command rules for Anubis.
//!
//! This module contains the `cmd` rule for running arbitrary commands that generate files.
//! It supports building tools for the host platform and then running them to generate outputs.

use crate::anubis::{self, AnubisTarget, JobCacheKey, RuleExt};
use crate::job_system::*;
use crate::rules::rule_utils::{ensure_directory_for_file, run_command};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use anyhow::Context;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::papyrus::*;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------

/// Command rule for running arbitrary commands that generate files.
///
/// The `cmd` rule allows building a tool executable and running it to generate
/// output files. This is useful for code generation workflows like bin2c.
///
/// Example usage in ANUBIS file:
/// ```
/// cmd(
///     name = "generate_graph_html",
///     tool = "//examples/ffmpeg:bin2c",
///     inputs = [RelPath("FFmpeg/fftools/resources/graph.html")],
///     outputs = [RelPath("generated/graph.html.c")],
///     args = ["graph_html", "$INPUT", "$OUTPUT"],
/// )
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cmd {
    pub name: String,

    /// Target path to the tool executable (e.g., "//examples/ffmpeg:bin2c")
    /// The tool will be built for the host platform automatically.
    pub tool: AnubisTarget,

    /// Input files that the command operates on
    #[serde(default)]
    pub inputs: Vec<PathBuf>,

    /// Output files that the command generates.
    /// These files will be tracked by the build system and can be used as
    /// dependencies by other rules.
    pub outputs: Vec<PathBuf>,

    /// Command line arguments. Special substitutions:
    /// - $INPUT or ${INPUT} - replaced with the first input file
    /// - $INPUT0, $INPUT1, etc. - replaced with specific input files
    /// - $OUTPUT or ${OUTPUT} - replaced with the first output file
    /// - $OUTPUT0, $OUTPUT1, etc. - replaced with specific output files
    #[serde(default)]
    pub args: Vec<String>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

/// Artifact produced by a cmd rule, containing paths to generated files.
#[derive(Debug)]
pub struct CmdArtifact {
    pub output_files: Vec<PathBuf>,
}

impl JobArtifact for CmdArtifact {}

// ----------------------------------------------------------------------------
// Trait Implementations
// ----------------------------------------------------------------------------
impl anubis::Rule for Cmd {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        bail_loc_if!(ctx.mode.is_none(), "Cannot create Cmd job without a mode");

        let cmd = arc_self
            .clone()
            .downcast_arc::<Cmd>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to Cmd", arc_self))?;

        Ok(ctx.new_job(
            format!("Build Cmd Target {}", self.target.target_path()),
            Box::new(move |job| build_cmd(cmd.clone(), job)),
        ))
    }
}

impl crate::papyrus::PapyrusObjectType for Cmd {
    fn name() -> &'static str {
        "cmd"
    }
}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------

fn parse_cmd(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut cmd = Cmd::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    cmd.target = t;
    Ok(Arc::new(cmd))
}

fn build_cmd(cmd: Arc<Cmd>, mut job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job.ctx.mode.as_ref().unwrap();

    // Get host mode for building the tool
    let host_mode = job.ctx.anubis.get_host_mode()?;

    // Get the tool rule and build it for the host platform
    let tool_rule = job.ctx.anubis.get_rule(&cmd.tool, &host_mode)?;

    // Create a new context with host mode for building the tool
    let host_toolchain_target = AnubisTarget::new("//toolchains:default")?;
    let host_toolchain = job.ctx.anubis.get_toolchain(host_mode.clone(), &host_toolchain_target)?;

    let host_ctx = Arc::new(JobContext {
        anubis: job.ctx.anubis.clone(),
        job_system: job.ctx.job_system.clone(),
        mode: Some(host_mode),
        toolchain: Some(host_toolchain),
    });

    let tool_job = tool_rule.build(tool_rule.clone(), host_ctx)?;
    let tool_job_id = tool_job.id;
    job.ctx.job_system.add_job(tool_job)?;

    // Create the run command job that waits for the tool to be built
    let cmd2 = cmd.clone();
    let ctx = job.ctx.clone();
    let run_job = move |job: Job| -> anyhow::Result<JobOutcome> {
        run_cmd_tool(cmd2, tool_job_id, job.ctx)
    };

    // Update this job to run the command
    job.desc.push_str(" (run command)");
    job.job_fn = Some(Box::new(run_job));

    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: vec![tool_job_id],
        deferred_job: job,
    }))
}

fn run_cmd_tool(cmd: Arc<Cmd>, tool_job_id: JobId, ctx: Arc<JobContext>) -> anyhow::Result<JobOutcome> {
    // Get the tool executable path from the job result
    let tool_result = ctx.job_system.get_result(tool_job_id)?;

    // Try to extract the executable path from the result
    // CcBinary produces CompileExeArtifact which has output_file
    let tool_path = if let Ok(exe_artifact) = tool_result.clone().downcast_arc::<crate::rules::cc_rules::CompileExeArtifact>() {
        exe_artifact.output_file.clone()
    } else {
        bail_loc!(
            "Tool job result for cmd rule could not be cast to a known artifact type. Got: {}",
            std::any::type_name_of_val(tool_result.as_any())
        );
    };

    // Build the argument list with substitutions
    let args = substitute_args(&cmd.args, &cmd.inputs, &cmd.outputs)?;

    // Ensure output directories exist
    for output in &cmd.outputs {
        ensure_directory_for_file(output)?;
    }

    // Run the command
    tracing::info!(
        tool = %tool_path.display(),
        args = ?args,
        "Running cmd tool"
    );

    let output = run_command(&tool_path, &args)?;

    if output.status.success() {
        tracing::debug!(
            target = %cmd.target.target_path(),
            outputs = ?cmd.outputs,
            "Cmd completed successfully"
        );

        Ok(JobOutcome::Success(Arc::new(CmdArtifact {
            output_files: cmd.outputs.clone(),
        })))
    } else {
        tracing::error!(
            target = %cmd.target.target_path(),
            exit_code = output.status.code(),
            stdout = %String::from_utf8_lossy(&output.stdout),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "Cmd failed"
        );

        bail_loc!(
            "Command completed with error status [{}].\n  Tool: {}\n  Args: {}\n  stdout: {}\n  stderr: {}",
            output.status,
            tool_path.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

/// Substitute special placeholders in command arguments.
fn substitute_args(
    args: &[String],
    inputs: &[PathBuf],
    outputs: &[PathBuf],
) -> anyhow::Result<Vec<String>> {
    let mut result = Vec::with_capacity(args.len());

    for arg in args {
        let substituted = substitute_single_arg(arg, inputs, outputs)?;
        result.push(substituted);
    }

    Ok(result)
}

fn substitute_single_arg(
    arg: &str,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
) -> anyhow::Result<String> {
    let mut result = arg.to_string();

    // Handle $INPUT or ${INPUT} (first input)
    if result.contains("$INPUT") && !result.contains("$INPUT0") && !result.contains("$INPUT1") {
        if inputs.is_empty() {
            bail_loc!("Argument contains $INPUT but no inputs were provided");
        }
        result = result.replace("${INPUT}", &inputs[0].to_string_lossy());
        result = result.replace("$INPUT", &inputs[0].to_string_lossy());
    }

    // Handle $OUTPUT or ${OUTPUT} (first output)
    if result.contains("$OUTPUT") && !result.contains("$OUTPUT0") && !result.contains("$OUTPUT1") {
        if outputs.is_empty() {
            bail_loc!("Argument contains $OUTPUT but no outputs were provided");
        }
        result = result.replace("${OUTPUT}", &outputs[0].to_string_lossy());
        result = result.replace("$OUTPUT", &outputs[0].to_string_lossy());
    }

    // Handle indexed inputs: $INPUT0, $INPUT1, etc.
    for (i, input) in inputs.iter().enumerate() {
        let placeholder = format!("$INPUT{}", i);
        let placeholder_braced = format!("${{INPUT{}}}", i);
        result = result.replace(&placeholder_braced, &input.to_string_lossy());
        result = result.replace(&placeholder, &input.to_string_lossy());
    }

    // Handle indexed outputs: $OUTPUT0, $OUTPUT1, etc.
    for (i, output) in outputs.iter().enumerate() {
        let placeholder = format!("$OUTPUT{}", i);
        let placeholder_braced = format!("${{OUTPUT{}}}", i);
        result = result.replace(&placeholder_braced, &output.to_string_lossy());
        result = result.replace(&placeholder, &output.to_string_lossy());
    }

    Ok(result)
}

// ----------------------------------------------------------------------------
// Public Functions
// ----------------------------------------------------------------------------

pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("cmd".to_owned()),
        parse_rule: parse_cmd,
    })?;

    Ok(())
}
