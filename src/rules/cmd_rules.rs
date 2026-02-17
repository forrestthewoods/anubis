//! Command rules for Anubis.
//!
//! This module contains the `anubis_cmd` rule for running arbitrary commands.
//! It supports building tools for the host platform and then running them to generate outputs.

use crate::anubis::{self, AnubisTarget};
use crate::job_system::*;
use crate::rules::rule_utils::{ensure_directory, run_command_verbose};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use camino::Utf8PathBuf;
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
    /// Output directory where command outputs were written
    pub out_dir: Utf8PathBuf,
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

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        let toolchain = ctx
            .toolchain
            .as_ref()
            .ok_or_else(|| anyhow_loc!("Cannot build AnubisCmd without a toolchain"))?;

        // Get host toolchain
        let host_mode = ctx.anubis.get_mode(&toolchain.host_mode)?;
        let host_toolchain = ctx.anubis.get_toolchain(&host_mode, &toolchain.target)?;
        
        // Create new context for host
        let host_ctx = JobContext {
            anubis: ctx.anubis.clone(),
            job_system: ctx.job_system.clone(),
            mode: Some(host_mode),
            toolchain: Some(host_toolchain)
        };
        
        let dep = ctx.anubis.preload_rule(self.tool.clone(), &Arc::new(host_ctx))?;
        Ok(vec![dep])
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
    let toolchain = job
        .ctx
        .toolchain
        .as_ref()
        .ok_or_else(|| anyhow_loc!("Cannot build AnubisCmd without a toolchain"))?;

    // Compute output directory for $OUTDIR substitution
    let out_dir = job.ctx.anubis.out_dir(&job.ctx.mode, &cmd.target, "cmd").slash_fix();

    // Get host mode for building the tool from the toolchain configuration.
    let host_mode = job.ctx.anubis.get_mode(&toolchain.host_mode)?;

    // Create a new context with host mode for building the tool
    let host_toolchain_target = AnubisTarget::new("//toolchains:default")?;
    let host_toolchain = job.ctx.anubis.get_toolchain(&host_mode, &host_toolchain_target)?;

    let host_ctx = Arc::new(JobContext {
        anubis: job.ctx.anubis.clone(),
        job_system: job.ctx.job_system.clone(),
        mode: Some(host_mode),
        toolchain: Some(host_toolchain),
    });

    let tool_job_id = job.ctx.anubis.build_rule(&cmd.tool, &host_ctx)?;

    // Create the job that spawns command child jobs after the tool is built
    let cmd2 = cmd.clone();
    let ctx = job.ctx.clone();
    let spawn_job =
        move |job: Job| -> anyhow::Result<JobOutcome> { spawn_command_jobs(cmd2, tool_job_id, out_dir, job) };

    // Update this job to spawn command jobs
    job.desc.push_str(" (spawn commands)");
    job.display.verb = "Spawning";
    job.job_fn = Some(Box::new(spawn_job));

    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: vec![tool_job_id],
        continuation_job: job,
    }))
}

fn spawn_command_jobs(cmd: Arc<AnubisCmd>, tool_job_id: JobId, out_dir: Utf8PathBuf, mut job: Job) -> anyhow::Result<JobOutcome> {
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

    // Ensure the output directory exists for $OUTDIR
    ensure_directory(out_dir.as_ref())?;
    let out_dir_str = out_dir.to_string();

    // If no commands, we're done
    if cmd.args.is_empty() {
        return Ok(JobOutcome::Success(Arc::new(AnubisCmdArtifact {
            commands_executed: 0,
            out_dir,
        })));
    }

    // Spawn a child job for each command invocation
    let mut child_job_ids = Vec::with_capacity(cmd.args.len());
    let verbose = job.ctx.anubis.verbose_tools;

    for (idx, command_args) in cmd.args.iter().enumerate() {
        let tool_path = tool_path.clone();
        let tool_filename = tool_path.file_name().ok_or_else(|| anyhow_loc!("No filename for [{:?}]", tool_path))?.to_string();

        // Replace $OUTDIR in each argument
        let args: Vec<String> = command_args
            .iter()
            .map(|arg| arg.replace("$OUTDIR", &out_dir_str))
            .collect();
        let target_name = cmd.target.target_path().to_string();

        let run_display = JobDisplayInfo {
            verb: "Running",
            short_name: format!("cmd {tool_filename}"),
            detail: format!("{} command {tool_filename}", target_name),
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
            out_dir,
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
            out_dir: Utf8PathBuf::default(),
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
// DeployIntoWorkspace Rule
// ----------------------------------------------------------------------------

/// How files are deployed into the workspace.
#[derive(Clone, Debug, Default)]
pub enum DeployMethod {
    #[default]
    Copy,
    Symlink,
    Hardlink,
}

impl<'de> Deserialize<'de> for DeployMethod {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "copy" => Ok(DeployMethod::Copy),
            "symlink" => Ok(DeployMethod::Symlink),
            "hardlink" => Ok(DeployMethod::Hardlink),
            _ => Err(serde::de::Error::unknown_variant(&s, &["copy", "symlink", "hardlink"])),
        }
    }
}

/// Rule that deploys outputs from a dependency's out_dir into the workspace.
///
/// This allows `anubis_cmd` rules to write into `.anubis-out/` and then
/// explicitly deploy results back into the source tree when needed.
///
/// Example usage in ANUBIS file:
/// ```
/// deploy_into_workspace(
///     name = "deploy_resources",
///     dep = Target(":generate_resources"),
///     dst = RelPath("FFmpeg/fftools/resources"),
///     method = "copy",
/// )
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployIntoWorkspace {
    pub name: String,

    /// Source rule whose outputs to deploy (must produce AnubisCmdArtifact)
    pub dep: AnubisTarget,

    /// Destination directory in the workspace
    pub dst: Utf8PathBuf,

    /// How to deploy files (copy, symlink, or hardlink). Defaults to copy.
    #[serde(default)]
    pub method: DeployMethod,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

/// Artifact produced by a deploy_into_workspace rule.
#[derive(Debug)]
pub struct DeployIntoWorkspaceArtifact {
    /// List of output files that were deployed into the workspace
    pub output_files: Vec<Utf8PathBuf>,
}

impl JobArtifact for DeployIntoWorkspaceArtifact {}

impl anubis::Rule for DeployIntoWorkspace {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn is_pure(&self) -> bool { 
        return false; 
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        bail_loc_if!(ctx.mode.is_none(), "Cannot create DeployIntoWorkspace job without a mode");

        let rule = arc_self
            .clone()
            .downcast_arc::<DeployIntoWorkspace>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to DeployIntoWorkspace", arc_self))?;

        let target_name = self.target.target_name().to_string();
        let target_path = self.target.target_path().to_string();
        Ok(ctx.new_job(
            format!("Build DeployIntoWorkspace Target {}", &target_path),
            JobDisplayInfo { verb: "Deploying", short_name: target_name, detail: target_path },
            Box::new(move |job| build_deploy_into_workspace(rule.clone(), job)),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        let dep = ctx.anubis.preload_rule(self.dep.clone(), &ctx)?;
        Ok(vec![dep])
    }
}

impl crate::papyrus::PapyrusObjectType for DeployIntoWorkspace {
    fn name() -> &'static str {
        "deploy_into_workspace"
    }
}

fn parse_deploy_into_workspace(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut rule = DeployIntoWorkspace::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    rule.target = t;
    Ok(Arc::new(rule))
}

fn build_deploy_into_workspace(rule: Arc<DeployIntoWorkspace>, mut job: Job) -> anyhow::Result<JobOutcome> {
    // Build the dependency first
    let dep_job_id = job.ctx.anubis.build_rule(&rule.dep, &job.ctx)?;

    let rule2 = rule.clone();
    let continuation = move |job: Job| -> anyhow::Result<JobOutcome> {
        deploy_into_workspace_continuation(rule2, dep_job_id, job)
    };

    job.desc = format!("Deploy outputs for {}", rule.target.target_path());
    job.display = JobDisplayInfo {
        verb: "Deploying",
        short_name: rule.target.target_name().to_string(),
        detail: rule.target.target_path().to_string(),
    };
    job.job_fn = Some(Box::new(continuation));

    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: vec![dep_job_id],
        continuation_job: job,
    }))
}

fn deploy_into_workspace_continuation(
    rule: Arc<DeployIntoWorkspace>,
    dep_job_id: JobId,
    _job: Job,
) -> anyhow::Result<JobOutcome> {
    // Get the dependency's artifact to find its out_dir
    let dep_result = _job.ctx.job_system.get_result(dep_job_id)?;
    let cmd_artifact = dep_result
        .clone()
        .downcast_arc::<AnubisCmdArtifact>()
        .map_err(|_| anyhow_loc!(
            "deploy_into_workspace dep must produce AnubisCmdArtifact. Got: {}",
            std::any::type_name_of_val(dep_result.as_any())
        ))?;

    let src_dir = &cmd_artifact.out_dir;
    let dst_dir = &rule.dst;

    // Ensure destination directory exists
    ensure_directory(dst_dir.as_ref())?;

    // Deploy all files from src_dir to dst_dir
    let mut output_files = Vec::new();
    if src_dir.exists() {
        for entry in std::fs::read_dir(src_dir.as_std_path())? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_file() {
                let file_name = entry.file_name();
                let dst_path = dst_dir.join(file_name.to_string_lossy().as_ref());
                let src_path = entry.path();
                let dst_std_path: &std::path::Path = dst_path.as_ref();

                match rule.method {
                    DeployMethod::Copy => {
                        std::fs::copy(&src_path, dst_std_path)?;
                    }
                    DeployMethod::Symlink => {
                        // Remove existing file first so symlink creation succeeds
                        let _ = std::fs::remove_file(dst_std_path);
                        #[cfg(windows)]
                        std::os::windows::fs::symlink_file(&src_path, dst_std_path)?;
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(&src_path, dst_std_path)?;
                    }
                    DeployMethod::Hardlink => {
                        // Remove existing file first so hard link creation succeeds
                        let _ = std::fs::remove_file(dst_std_path);
                        std::fs::hard_link(&src_path, dst_std_path)?;
                    }
                }

                output_files.push(dst_path);
            }
        }
    }

    tracing::trace!(
        target = %rule.target.target_path(),
        files_deployed = output_files.len(),
        method = ?rule.method,
        src = %src_dir,
        dst = %dst_dir,
        "Deployed files into workspace"
    );

    Ok(JobOutcome::Success(Arc::new(DeployIntoWorkspaceArtifact {
        output_files,
    })))
}

// ----------------------------------------------------------------------------
// Public Functions
// ----------------------------------------------------------------------------

pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("anubis_cmd".to_owned()),
        parse_rule: parse_anubis_cmd,
    })?;

    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("deploy_into_workspace".to_owned()),
        parse_rule: parse_deploy_into_workspace,
    })?;

    Ok(())
}
