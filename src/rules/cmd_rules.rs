//! Command rules for Anubis.
//!
//! This module contains the `anubis_cmd` rule for running arbitrary commands.
//! It supports building tools for the host platform and then running them to generate outputs.

use crate::anubis::{self, AnubisTarget};
use crate::job_system::*;
use crate::rules::rule_utils::run_command_verbose;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

use crate::papyrus::*;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------

/// Command rule for running arbitrary commands.
///
/// The `anubis_cmd` rule allows building a tool executable and running it
/// with multiple command invocations in parallel.
///
/// Example usage in ANUBIS file:
/// ```
/// anubis_cmd(
///     name = "generate_resources",
///     tool = "//samples/external/ffmpeg:bin2c",
///     args = [
///         ["graph_html", "FFmpeg/fftools/resources/graph.html", "generated/graph_html.c"],
///         ["graph_css", "FFmpeg/fftools/resources/graph.css", "generated/graph_css.c"],
///     ],
/// )
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnubisCmd {
    pub name: String,

    /// Target path to the tool executable (e.g., "//samples/external/ffmpeg:bin2c")
    /// The tool will be built for the host platform automatically.
    pub tool: AnubisTarget,

    /// Command invocations. Each inner Vec<String> is a single command invocation.
    /// Each command runs the tool with the specified arguments.
    /// All commands run as parallel child jobs.
    #[serde(default)]
    pub args: Vec<Vec<String>>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

/// Artifact produced by an anubis_cmd rule.
#[derive(Debug)]
pub struct AnubisCmdArtifact {
    /// Number of commands that were executed
    pub commands_executed: usize,
}

impl JobArtifact for AnubisCmdArtifact {}

// ----------------------------------------------------------------------------
// Trait Implementations
// ----------------------------------------------------------------------------
impl anubis::Rule for AnubisCmd {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        bail_loc_if!(ctx.mode.is_none(), "Cannot create AnubisCmd job without a mode");

        let cmd = arc_self
            .clone()
            .downcast_arc::<AnubisCmd>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to AnubisCmd", arc_self))?;

        let target_name = self.target.target_name().to_string();
        let target_path = self.target.target_path().to_string();
        Ok(ctx.new_job(
            format!("Build AnubisCmd Target {}", &target_path),
            JobDisplayInfo { verb: "Building", short_name: target_name, detail: target_path },
            Box::new(move |job| build_anubis_cmd(cmd.clone(), job)),
        ))
    }
}

impl crate::papyrus::PapyrusObjectType for AnubisCmd {
    fn name() -> &'static str {
        "anubis_cmd"
    }
}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------

fn parse_anubis_cmd(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut cmd = AnubisCmd::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    cmd.target = t;
    Ok(Arc::new(cmd))
}

fn build_anubis_cmd(cmd: Arc<AnubisCmd>, mut job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job.ctx.mode.as_ref().unwrap();
    let toolchain = job
        .ctx
        .toolchain
        .as_ref()
        .ok_or_else(|| anyhow_loc!("Cannot build AnubisCmd without a toolchain"))?;

    // Get build mode for building the tool from the toolchain configuration.
    let build_mode = job.ctx.anubis.get_mode(&toolchain.build_mode)?;

    // Create a new context with build mode for building the tool
    let build_toolchain_target = AnubisTarget::new("//toolchains:default")?;
    let build_toolchain = job.ctx.anubis.get_toolchain(build_mode.clone(), &build_toolchain_target)?;

    let build_ctx = Arc::new(JobContext {
        anubis: job.ctx.anubis.clone(),
        job_system: job.ctx.job_system.clone(),
        mode: Some(build_mode),
        toolchain: Some(build_toolchain),
    });

    let tool_job_id = job.ctx.anubis.build_rule(&cmd.tool, &build_ctx)?;

    // Create the job that spawns command child jobs after the tool is built
    let cmd2 = cmd.clone();
    let ctx = job.ctx.clone();
    let spawn_job =
        move |job: Job| -> anyhow::Result<JobOutcome> { spawn_command_jobs(cmd2, tool_job_id, job) };

    // Update this job to spawn command jobs
    job.desc.push_str(" (spawn commands)");
    job.display.verb = "Spawning";
    job.job_fn = Some(Box::new(spawn_job));

    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: vec![tool_job_id],
        continuation_job: job,
    }))
}

fn spawn_command_jobs(cmd: Arc<AnubisCmd>, tool_job_id: JobId, mut job: Job) -> anyhow::Result<JobOutcome> {
    // Get the tool executable path from the job result
    let tool_result = job.ctx.job_system.get_result(tool_job_id)?;

    // Extract the executable path from the result
    let tool_path = if let Ok(exe_artifact) =
        tool_result.clone().downcast_arc::<crate::rules::cc_rules::CompileExeArtifact>()
    {
        exe_artifact.output_file.clone()
    } else {
        bail_loc!(
            "Tool job result for anubis_cmd rule could not be cast to a known artifact type. Got: {}",
            std::any::type_name_of_val(tool_result.as_any())
        );
    };

    // If no commands, we're done
    if cmd.args.is_empty() {
        return Ok(JobOutcome::Success(Arc::new(AnubisCmdArtifact {
            commands_executed: 0,
        })));
    }

    // Spawn a child job for each command invocation
    let mut child_job_ids = Vec::with_capacity(cmd.args.len());
    let verbose = job.ctx.anubis.verbose_tools;

    for (idx, command_args) in cmd.args.iter().enumerate() {
        let tool_path = tool_path.clone();
        let args = command_args.clone();
        let target_name = cmd.target.target_path().to_string();

        let run_display = JobDisplayInfo {
            verb: "Running",
            short_name: format!("cmd {}", idx),
            detail: format!("{} command {}", target_name, idx),
        };
        let child_job = job.ctx.new_job(
            format!("Run {} command {}", target_name, idx),
            run_display,
            Box::new(move |_job| run_single_command(tool_path.as_ref(), &args, &target_name, idx, verbose)),
        );

        let child_job_id = child_job.id;
        child_job_ids.push(child_job_id);
        job.ctx.job_system.add_job(child_job)?;
    }

    // Create a final job that waits for all command jobs to complete
    let num_commands = cmd.args.len();
    let finalize_job = move |_job: Job| -> anyhow::Result<JobOutcome> {
        Ok(JobOutcome::Success(Arc::new(AnubisCmdArtifact {
            commands_executed: num_commands,
        })))
    };

    job.desc = format!("Finalize AnubisCmd {}", cmd.target.target_path());
    job.display = JobDisplayInfo {
        verb: "Finalizing",
        short_name: cmd.target.target_name().to_string(),
        detail: cmd.target.target_path().to_string(),
    };
    job.job_fn = Some(Box::new(finalize_job));

    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: child_job_ids,
        continuation_job: job,
    }))
}

fn run_single_command(
    tool_path: &Path,
    args: &[String],
    target_name: &str,
    idx: usize,
    verbose: bool,
) -> anyhow::Result<JobOutcome> {
    tracing::info!(
        tool = %tool_path.display(),
        args = ?args,
        "Running anubis_cmd command {}",
        idx
    );

    let output = run_command_verbose(tool_path, args, verbose)?;

    if output.status.success() {
        tracing::debug!(
            target = %target_name,
            command_idx = idx,
            "Command completed successfully"
        );

        // Return a simple marker artifact for the individual command
        Ok(JobOutcome::Success(Arc::new(AnubisCmdArtifact {
            commands_executed: 1,
        })))
    } else {
        tracing::error!(
            target = %target_name,
            command_idx = idx,
            exit_code = output.status.code(),
            stdout = %String::from_utf8_lossy(&output.stdout),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "Command failed"
        );

        bail_loc!(
            "Command {} completed with error status [{}].\n  Tool: {}\n  Args: {}\n  stdout: {}\n  stderr: {}",
            idx,
            output.status,
            tool_path.display(),
            args.join(" "),
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
        name: RuleTypename("anubis_cmd".to_owned()),
        parse_rule: parse_anubis_cmd,
    })?;

    Ok(())
}
