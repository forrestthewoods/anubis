//! Command rules for Anubis.
//!
//! `anubis_cmd` builds a tool for the build platform and runs it, and
//! `deploy_into_workspace` copies a command's outputs into the source tree.
//!
//! An `anubis_cmd` is one cached action (#174). Anubis can't see what an
//! arbitrary tool reads or writes, so the rule declares both: `srcs` are the
//! files it reads, `outputs` the files it writes under `$OUTDIR`. The key is
//! the tool (the bytes of the staged executable, its runtime shared libraries
//! and `resources`), the arguments, the Natron installs they point into, and
//! the digests of `srcs`. After a run, the outputs are checked exactly: each
//! declared one must exist, and nothing else may be in `$OUTDIR`. Without a
//! sandbox (#209) a file the tool reads that isn't in `srcs` doesn't cause a
//! rerun; any argument naming an existing file must be in `srcs`, which
//! catches the common case.

use crate::anubis::{self, AnubisTarget};
use crate::build_cache::{ActionOutput, BuildCache, Digest, KeyBuilder, PATHS_CASE_INSENSITIVE};
use crate::job_system::*;
use crate::rules::cc_rules::CompileExeArtifact;
use crate::rules::rule_utils::{ensure_directory, run_command_verbose, ws_relative};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::papyrus::*;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};

/// Written in `args`, replaced by the rule's output directory, relative to
/// the workspace root (every tool's working directory).
const OUTDIR_VAR: &str = "$OUTDIR";

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------

/// Runs a tool built for the build platform, once per entry of `args`.
///
/// ```text
/// anubis_cmd(
///     name = "generate_resources",
///     tool = Target(":bin2c"),
///     srcs = RelPaths([ "res/graph.html", "res/graph.css" ]),
///     args = [
///         [ RelPath("res/graph.html"), "$OUTDIR/graph.html.c", "graph_html" ],
///         [ RelPath("res/graph.css"), "$OUTDIR/graph.css.c", "graph_css" ],
///     ],
///     outputs = [ "graph.html.c", "graph.css.c" ],
/// )
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnubisCmd {
    pub name: String,

    /// The tool, a `cc_binary`. Built in the toolchain's `build_mode` (the
    /// host, release) and run from its staged `bin/` directory.
    pub tool: AnubisTarget,

    /// Every file the tool reads. Part of the cache key.
    pub srcs: Vec<Utf8PathBuf>,

    /// Command invocations, run in parallel. `$OUTDIR` in an argument is the
    /// rule's output directory, relative to the workspace root. Workspace
    /// paths (`RelPath`) are passed relative to the root as well, so outputs
    /// don't depend on where the workspace is (#272).
    pub args: Vec<Vec<String>>,

    /// The files the commands write, relative to `$OUTDIR`.
    pub outputs: Vec<String>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

/// What an `anubis_cmd` produced.
#[derive(Debug)]
pub struct AnubisCmdArtifact {
    /// The rule's output directory (`$OUTDIR`).
    pub out_dir: Utf8PathBuf,
    /// The declared outputs, relative to `out_dir`, in declared order.
    pub outputs: Vec<String>,
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

    /// The tool (built for the build machine) and the arguments: modes that
    /// resolve this command to the same thing share one run of it.
    fn shared_key(&self, ctx: &Arc<JobContext>) -> anyhow::Result<Option<String>> {
        Ok(Some(self.inputs_id(ctx)?))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        Ok(vec![ctx.anubis.preload_rule(self.tool.clone(), &tool_context(&ctx)?)?])
    }
}

impl AnubisCmd {
    /// Names everything the command's outputs depend on: the tool, the mode
    /// and toolchain it is built with, and the arguments, `srcs` and
    /// `outputs` as resolved for the mode asking (workspace-relative, so the id, which names the output
    /// directory, is the same wherever the workspace is).
    fn inputs_id(&self, ctx: &Arc<JobContext>) -> anyhow::Result<String> {
        let tool_ctx = tool_context(ctx)?;
        let mut h = xxhash_rust::xxh3::Xxh3::new();
        let mut add = |part: &str| {
            h.update(part.as_bytes());
            h.update(&[0]);
        };
        add(self.tool.target_path());
        add(tool_ctx.mode.as_ref().map_or("", |m| m.target.target_path()));
        add(tool_ctx.toolchain.as_ref().map_or("", |t| t.target.target_path()));
        for command in &self.args {
            add("\x01");
            // As the tool gets them, but with `$OUTDIR` left in (it is
            // named by this id): the cache key takes them verbatim, so two
            // commands that differ only in a separator must not share a job.
            for arg in resolve_args(command, &ctx.anubis.root, OUTDIR_VAR) {
                add(&arg);
            }
        }
        // `srcs` and `outputs` can be `select`s too.
        add("\x02");
        for src in &self.srcs {
            add(src.strip_prefix(&ctx.anubis.root).unwrap_or(src).as_str());
        }
        add("\x02");
        for output in &self.outputs {
            add(output);
        }
        Ok(format!("{:016x}", h.digest()))
    }
}

/// The context the tool builds in: the toolchain's `build_mode` (the build
/// machine), with the same toolchain target.
fn tool_context(ctx: &Arc<JobContext>) -> anyhow::Result<Arc<JobContext>> {
    let toolchain = ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("Cannot build AnubisCmd without a toolchain"))?;
    let build_mode = ctx.anubis.get_mode(&toolchain.build_mode)?;
    let build_toolchain = ctx.anubis.get_toolchain(&build_mode, &toolchain.target)?;
    Ok(Arc::new(JobContext {
        anubis: ctx.anubis.clone(),
        job_system: ctx.job_system.clone(),
        mode: Some(build_mode),
        toolchain: Some(build_toolchain),
    }))
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
    // `glob(["res/**"])` also matches directories.
    cmd.srcs.retain(|p| !p.is_dir());
    cmd.srcs = std::mem::take(&mut cmd.srcs).into_iter().map(|p| p.slash_fix()).collect();
    cmd.srcs.sort();
    cmd.srcs.dedup();
    cmd.outputs = check_output_names(&cmd.outputs).with_context(|| format!("anubis_cmd {}", t.target_path()))?;
    bail_loc_if!(cmd.args.is_empty(), "anubis_cmd {}: `args` is empty", t.target_path());
    cmd.target = t;
    Ok(Arc::new(cmd))
}

/// `outputs` normalized to `/`: at least one, each a plain relative name.
fn check_output_names(outputs: &[String]) -> anyhow::Result<Vec<String>> {
    bail_loc_if!(outputs.is_empty(), "`outputs` is empty: a command must write at least one file");
    let outputs: Vec<String> = outputs.iter().map(|o| o.replace('\\', "/")).collect();
    for name in &outputs {
        let plain = |c: &str| !c.is_empty() && c != "." && c != ".." && !c.contains(':');
        bail_loc_if!(
            !name.split('/').all(plain),
            "bad output name [{name}]: use a path relative to $OUTDIR, without `.` or `..`"
        );
    }
    Ok(outputs)
}

fn build_anubis_cmd(cmd: Arc<AnubisCmd>, mut job: Job) -> anyhow::Result<JobOutcome> {
    let tool_job_id = job.ctx.anubis.build_rule(&cmd.tool, &tool_context(&job.ctx)?)?;

    let cmd2 = cmd.clone();
    job.desc.push_str(" (spawn commands)");
    job.display.verb = "Spawning";
    job.job_fn = Some(Box::new(move |job: Job| spawn_command_jobs(cmd2, tool_job_id, job)));

    Ok(JobOutcome::Deferred(JobDeferral {
        blocked_by: vec![tool_job_id],
        continuation_job: job,
    }))
}

/// The tool's files that are part of its identity: the executable, then
/// (name relative to its directory, path) for each runtime shared library
/// and resource, sorted.
fn tool_files(tool: &CompileExeArtifact) -> Vec<(String, Utf8PathBuf)> {
    let mut files: Vec<(String, Utf8PathBuf)> = tool
        .runtime_files
        .iter()
        .map(|f| (f.file_name().unwrap_or_default().to_owned(), f.clone()))
        .chain(tool.resources.iter().map(|(staged, rel)| (rel.clone(), staged.clone())))
        .collect();
    files.sort();
    files.dedup();
    files
}

/// `args` as the tool gets them: `$OUTDIR` replaced by `out_rel`, and an
/// argument that is a workspace path (a `RelPath` is absolute) made relative
/// to the root, the tool's working directory, so outputs don't depend on
/// where the workspace is (#272). Nothing else is rewritten.
fn resolve_args(args: &[String], root: &Utf8Path, out_rel: &str) -> Vec<String> {
    let fold = |s: &str| {
        let s = s.replace('\\', "/");
        if PATHS_CASE_INSENSITIVE { s.to_ascii_lowercase() } else { s }
    };
    let prefix = format!("{}/", fold(root.as_str()).trim_end_matches('/'));
    args.iter()
        .map(|a| {
            // Folding keeps byte offsets.
            let a = if fold(a).starts_with(&prefix) { &a[prefix.len()..] } else { a.as_str() };
            a.replace(OUTDIR_VAR, out_rel)
        })
        .collect()
}

/// The cache key: the tool, the resolved arguments (per invocation), the
/// Natron installs they point into, `srcs` and `outputs`. The resolved
/// arguments name the output directory, so each (mode, target) has its own
/// entry: a tool that writes its output path into a file can't hand one
/// mode's path to another.
fn cache_key(
    cache: &BuildCache,
    cmd: &AnubisCmd,
    tool: &CompileExeArtifact,
    invocations: &[Vec<String>],
) -> anyhow::Result<Digest> {
    let mut kb = KeyBuilder::new("anubis_cmd-v1");
    // The tool job has finished, so the staged files are final for this
    // run (`tool_identity` memoizes).
    kb.field(cache.tool_identity(&tool.output_file)?);
    let files = tool_files(tool);
    kb.field((files.len() as u64).to_le_bytes());
    for (name, path) in &files {
        let d = cache
            .file_digest(path)?
            .ok_or_else(|| anyhow_loc!("{path}, a file of the tool {}, is missing", cmd.tool.target_path()))?;
        kb.field(name).digest(&d);
    }
    kb.field((invocations.len() as u64).to_le_bytes());
    for args in invocations {
        kb.field((args.len() as u64).to_le_bytes());
        for a in args {
            // As the tool gets it, byte for byte: already relative to the
            // workspace, and `--sep=\` is not `--sep=/`.
            kb.field(a);
        }
    }
    let installs = cache.natron_installs_in_args(&invocations.concat());
    kb.field((installs.len() as u64).to_le_bytes());
    for i in &installs {
        kb.field(i);
    }
    if !cache.key_add_files(&mut kb, &cmd.srcs)? {
        let missing: Vec<&str> = cmd.srcs.iter().filter(|p| !p.is_file()).map(|p| p.as_str()).collect();
        bail_loc!("anubis_cmd {}: files in `srcs` do not exist: {}", cmd.target.target_path(), missing.join(", "));
    }
    kb.field((cmd.outputs.len() as u64).to_le_bytes());
    let mut outputs = cmd.outputs.clone();
    outputs.sort();
    for o in &outputs {
        kb.field(o);
    }
    Ok(kb.finish())
}

/// After a run: every declared output is a regular file. (The cache stores
/// bytes, so a symlink would come back as a plain file on a hit.)
fn check_outputs(cmd: &AnubisCmd, out_dir: &Utf8Path) -> anyhow::Result<()> {
    let is_file = |o: &&String| std::fs::symlink_metadata(out_dir.join(o)).is_ok_and(|m| m.is_file());
    let missing: Vec<&str> = cmd.outputs.iter().filter(|o| !is_file(o)).map(String::as_str).collect();
    bail_loc_if!(
        !missing.is_empty(),
        "anubis_cmd {}: the commands did not write these declared outputs in {out_dir}: {}",
        cmd.target.target_path(),
        missing.join(", ")
    );
    Ok(())
}

fn spawn_command_jobs(cmd: Arc<AnubisCmd>, tool_job_id: JobId, mut job: Job) -> anyhow::Result<JobOutcome> {
    let tool_result = job.ctx.job_system.get_result(tool_job_id)?;
    let tool = tool_result.clone().downcast_arc::<CompileExeArtifact>().map_err(|_| {
        anyhow_loc!(
            "anubis_cmd {}: the tool {} is not an executable. Got: {}",
            cmd.target.target_path(),
            cmd.tool.target_path(),
            std::any::type_name_of_val(tool_result.as_any())
        )
    })?;

    let anubis = job.ctx.anubis.clone();
    let root = anubis.root.clone();
    // One job per `inputs_id`, shared by every mode that resolves the
    // command to it, so the output directory is named by the id, not a mode.
    let out_dir = anubis.out_dir(&None, &cmd.target, "cmd").join(cmd.inputs_id(&job.ctx)?).slash_fix();
    let out_rel = ws_relative(&out_dir, &root).as_str().trim_end_matches('/').to_owned();
    let invocations: Vec<Vec<String>> = cmd.args.iter().map(|a| resolve_args(a, &root, &out_rel)).collect();
    let artifact = || AnubisCmdArtifact { out_dir: out_dir.clone(), outputs: cmd.outputs.clone() };

    // Taken before `srcs` are hashed for the key: an edit after this is
    // caught by the check after the run, an edit before it is in the key.
    let started = std::time::SystemTime::now();
    let mut key = None;
    if let Some(cache) = &anubis.build_cache {
        // This run may have hashed them earlier, before an edit.
        for src in &cmd.srcs {
            cache.invalidate(src);
        }
        let k = cache_key(cache, &cmd, &tool, &invocations)?;
        // What an older version of the command left can be in the way (a
        // file `gen` where `gen/a.h` goes): empty the directory and retry.
        let restored = match cache.restore_outputs(&k, &out_dir) {
            Ok(r) => r,
            Err(_) => {
                crate::rules::stage::remove_dir_if_exists(&out_dir)?;
                cache.restore_outputs(&k, &out_dir)?
            }
        };
        if let Some(restored) = restored {
            if restored.len() == cmd.outputs.len() {
                cache.record_hit();
                return Ok(JobOutcome::Success(Arc::new(artifact())));
            }
        }
        cache.record_miss();
        key = Some(k);
    }

    // Start from an empty directory: the tool must never write through a
    // read-only link to a cache blob.
    crate::rules::stage::remove_dir_if_exists(&out_dir)?;
    ensure_directory(out_dir.as_ref())?;
    // Tools rarely create directories: make each output's parent.
    for o in &cmd.outputs {
        if let Some(parent) = out_dir.join(o).parent() {
            ensure_directory(parent.as_ref())?;
        }
    }

    let verbose = anubis.verbose_tools;
    let tool_path = tool.output_file.clone();
    let tool_filename = crate::file_name!(tool_path)?.to_string();
    let target_path = cmd.target.target_path().to_string();
    let mut child_job_ids = Vec::with_capacity(invocations.len());
    for (idx, args) in invocations.into_iter().enumerate() {
        let tool_path = tool_path.clone();
        let target_path = target_path.clone();
        let root = root.clone();
        let child_job = job.ctx.new_job(
            format!("Run {target_path} command {idx}"),
            JobDisplayInfo {
                verb: "Running",
                short_name: format!("cmd {tool_filename}"),
                detail: format!("{target_path} command {tool_filename}"),
            },
            Box::new(move |_job| run_single_command(&tool_path, &args, &target_path, idx, verbose, &root)),
        );
        child_job_ids.push(child_job.id);
        job.ctx.job_system.add_job(child_job)?;
    }

    let cmd2 = cmd.clone();
    let finalize_job = move |_job: Job| -> anyhow::Result<JobOutcome> {
        check_outputs(&cmd2, &out_dir)?;
        if let (Some(cache), Some(key)) = (&anubis.build_cache, key) {
            // `srcs`, and the tool's `resources`: those are staged as hard
            // links to source files, so an edit in place reaches the tool.
            // Their mtime only: staging (a new link) bumps the ctime just
            // before the run. The tool's other files are build outputs.
            let resources: Vec<Utf8PathBuf> = tool.resources.iter().map(|(staged, _)| staged.clone()).collect();
            if crate::build_cache::any_modified_since(&cmd2.srcs, started)
                || cache.written_since(&resources, started).is_some()
            {
                tracing::debug!("not caching {}: an input changed while it ran", cmd2.target.target_path());
            } else {
                let outputs: Vec<ActionOutput> =
                    cmd2.outputs.iter().map(|o| ActionOutput { path: out_dir.join(o), optional: false }).collect();
                cache.store(&key, &out_dir, &outputs)?;
            }
        }
        Ok(JobOutcome::Success(Arc::new(AnubisCmdArtifact { out_dir, outputs: cmd2.outputs.clone() })))
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

/// Result of one command invocation; the finalize job reads the files.
#[derive(Debug)]
struct CommandRan;
impl JobArtifact for CommandRan {}

fn run_single_command(
    tool_path: &Utf8Path,
    args: &[String],
    target_path: &str,
    idx: usize,
    verbose: bool,
    root: &Utf8Path,
) -> anyhow::Result<JobOutcome> {
    let output = run_command_verbose(tool_path.as_ref(), args, verbose, root)?;
    if output.status.success() {
        tracing::debug!(target = %target_path, command_idx = idx, "Command completed successfully");
        return Ok(JobOutcome::Success(Arc::new(CommandRan)));
    }
    bail_loc!(
        "anubis_cmd {target_path}: command {idx} failed [{}].\n  Tool: {tool_path}\n  Args: {}\n  stdout: {}\n  stderr: {}",
        output.status,
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// ----------------------------------------------------------------------------
// DeployIntoWorkspace Rule
// ----------------------------------------------------------------------------

/// How files are deployed into the workspace: always a copy. A cached
/// output is a read-only hard link to a cache blob, so a hard link or a
/// symlink to it would put the blob itself in the source tree.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployMethod {
    #[default]
    Copy,
}

/// Rule that deploys outputs from a dependency's out_dir into the workspace.
///
/// This allows `anubis_cmd` rules to write into `.anubis-out/` and then
/// explicitly deploy results back into the source tree when needed.
///
/// Example usage in ANUBIS file:
/// ```text
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

    /// How to deploy files. Only "copy", the default.
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

    /// Compiles read the deployed files by workspace path.
    fn blocks_dependents(&self) -> bool { true }

    /// The command it deploys, where to and how: modes that resolve to the
    /// same deploy share one job, so nothing reads `dst` while another
    /// mode's copy of the same deploy writes it.
    fn shared_key(&self, ctx: &Arc<JobContext>) -> anyhow::Result<Option<String>> {
        let dep = ctx.anubis.get_rule(&self.dep, ctx.anubis.mode_or_modeless(&ctx.mode))?;
        let dep_key = dep.shared_key(ctx)?.ok_or_else(|| {
            anyhow_loc!("deploy_into_workspace [{}]: dep [{}] must be an anubis_cmd", self.target.target_path(), self.dep.target_path())
        })?;
        let dst = self.dst.strip_prefix(&ctx.anubis.root).unwrap_or(&self.dst).to_owned().slash_fix();
        Ok(Some(crate::util::path_hash(&format!("{dep_key}\0{dst}\0{:?}", self.method))))
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
    // There is one workspace: a deploy that resolves to different commands
    // in two modes of one build can't put both in the same `dst`.
    let key = rule.shared_key(&job.ctx)?.unwrap_or_default();
    let first = job.ctx.anubis.deployed.entry((rule.target.clone(), rule.dst.clone())).or_insert_with(|| key.clone()).clone();
    bail_loc_if!(
        first != key,
        "[{}] deploys different commands to [{}] in the modes of this build. Make `dst` differ wherever the command does \
         (the same select), or make the command the same in every mode.",
        rule.target.target_path(),
        rule.dst
    );

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

/// Write each of the command's outputs to `dst`, at its name relative to
/// `$OUTDIR`, only where the file there differs (#246). A no-op build writes
/// nothing into the source tree and, with the stat cache, hashes nothing.
/// `dst` is a source directory: nothing in it is ever deleted.
fn deploy_into_workspace_continuation(
    rule: Arc<DeployIntoWorkspace>,
    dep_job_id: JobId,
    job: Job,
) -> anyhow::Result<JobOutcome> {
    let dep_result = job.ctx.job_system.get_result(dep_job_id)?;
    let cmd_artifact = dep_result.clone().downcast_arc::<AnubisCmdArtifact>().map_err(|_| {
        anyhow_loc!(
            "deploy_into_workspace dep must produce AnubisCmdArtifact. Got: {}",
            std::any::type_name_of_val(dep_result.as_any())
        )
    })?;

    let anubis = &job.ctx.anubis;
    let cache = anubis.build_cache.as_ref();
    let mut output_files = Vec::with_capacity(cmd_artifact.outputs.len());
    let mut written = 0usize;
    for name in &cmd_artifact.outputs {
        let src = cmd_artifact.out_dir.join(name);
        let dst = rule.dst.join(name).slash_fix();
        let digest = match cache {
            Some(cache) => cache.file_digest(&src)?,
            None => Some(Digest::of_file(&src).with_context(|| format!("reading {src}"))?),
        }
        .ok_or_else(|| anyhow_loc!("{src}, an output of {}, is missing", rule.dep.target_path()))?;

        if deploy_file(cache, &anubis.root, &src, &dst, &digest)? {
            written += 1;
            crate::rules::rule_utils::count_in_process_action();
            // Anubis wrote into the source tree: forget what this run knew
            // about the path (e.g. a header that didn't exist).
            if let Some(cache) = cache {
                cache.wrote(&dst);
            }
        }
        output_files.push(dst);
    }

    tracing::trace!(
        target = %rule.target.target_path(),
        files = output_files.len(),
        written,
        dst = %rule.dst,
        "Deployed files into workspace"
    );

    Ok(JobOutcome::Success(Arc::new(DeployIntoWorkspaceArtifact { output_files })))
}

/// Put a writable copy of `src` at `dst` unless one is already there.
/// Returns whether it wrote.
fn deploy_file(
    cache: Option<&BuildCache>,
    root: &Utf8Path,
    src: &Utf8Path,
    dst: &Utf8Path,
    digest: &Digest,
) -> anyhow::Result<bool> {
    if let Some(parent) = dst.parent() {
        ensure_directory(parent.as_ref())?;
    }
    // Same bytes in a plain, writable file: nothing to do. A symlink or a
    // read-only file (what the removed `symlink` and `hardlink` methods
    // left: links to a cache blob) is replaced by a copy.
    let plain = std::fs::symlink_metadata(dst).is_ok_and(|m| m.is_file() && !m.permissions().readonly());
    if let (true, Some(cache)) = (plain, cache) {
        if cache.file_digest(dst)? == Some(*digest) {
            return Ok(false);
        }
    }
    let tmp = crate::build_cache::place_tmp(root, dst)?;
    std::fs::copy(src, &tmp).with_context(|| format!("copying {src} to {tmp}"))?;
    // Outputs are read-only once cached, and `copy` copies the permissions;
    // a source file stays writable.
    make_writable(&tmp)?;
    // Windows copies the modification time too, and gives a file made where
    // one was just deleted the old one's creation time: stamp it now, so
    // an action already reading the old file sees that it changed.
    let stamp = std::fs::File::options().write(true).open(&tmp).and_then(|f| f.set_modified(std::time::SystemTime::now()));
    stamp.with_context(|| format!("setting the modification time of {tmp}"))?;
    crate::build_cache::replace_file(&tmp, dst)?;
    Ok(true)
}

/// Give the owner write permission (only that, on Unix).
fn make_writable(path: &Utf8Path) -> anyhow::Result<()> {
    let mut perms = std::fs::metadata(path).with_context(|| format!("reading {path}"))?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(perms.mode() | 0o200);
    }
    #[cfg(not(unix))]
    {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
    }
    std::fs::set_permissions(path, perms).with_context(|| format!("making {path} writable"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_cache::LocalDiskBackend;

    fn utf8(p: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap()
    }

    fn cmd(ws: &Utf8Path, srcs: &[&str], args: Vec<Vec<String>>, outputs: &[&str]) -> AnubisCmd {
        AnubisCmd {
            name: "gen".into(),
            tool: AnubisTarget::new("//x:tool").unwrap(),
            srcs: srcs.iter().map(|s| ws.join(s)).collect(),
            args,
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            target: AnubisTarget::new("//x:gen").unwrap(),
        }
    }

    #[test]
    fn output_names() {
        let ok = |v: &[&str]| check_output_names(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(ok(&["a.c", "gen\\b.h"]).unwrap(), vec!["a.c", "gen/b.h"]);
        assert!(ok(&[]).is_err(), "no outputs");
        for bad in ["../a", "/abs", "a//b", "./a", "C:/a", ""] {
            assert!(ok(&[bad]).is_err(), "{bad} should be rejected");
        }
        assert!(ok(&["gen/a.h", "gen/b.h"]).is_ok());
    }

    #[test]
    fn args_are_workspace_relative() {
        let args: Vec<String> = ["/src/res/src/a.txt", "$OUTDIR/src/a.c", "--in=/src/b", "name"].map(String::from).into();
        assert_eq!(
            resolve_args(&args, Utf8Path::new("/src"), "out/cmd"),
            vec!["res/src/a.txt", "out/cmd/src/a.c", "--in=/src/b", "name"]
        );
        // A Windows root as `current_dir()` spells it, arguments spelled
        // either way or mixed; other backslashes are left alone.
        let args: Vec<String> = ["C:/w/ws/a.txt", "C:\\w\\ws\\res\\b.txt", "C:\\w\\ws/c.txt", "--sep=\\t"].map(String::from).into();
        assert_eq!(
            resolve_args(&args, Utf8Path::new("C:\\w\\ws"), "out"),
            vec!["a.txt", "res\\b.txt", "c.txt", "--sep=\\t"]
        );
    }

    #[test]
    fn declared_outputs_must_exist() {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = utf8(tmp.path());
        std::fs::create_dir_all(out.join("gen")).unwrap();
        std::fs::write(out.join("a.c"), "").unwrap();
        std::fs::write(out.join("gen/b.h"), "").unwrap();
        let c = |outputs: &[&str]| cmd(&out, &[], vec![vec![]], outputs);
        check_outputs(&c(&["a.c", "gen/b.h"]), &out).unwrap();
        let err = check_outputs(&c(&["a.c", "c.c"]), &out).unwrap_err().to_string();
        assert!(err.contains("did not write") && err.contains("c.c"), "{err}");
    }

    #[test]
    fn deploy_copies_only_what_differs_and_never_links() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = utf8(tmp.path()).join("ws");
        std::fs::create_dir_all(ws.join("out")).unwrap();
        let cache = BuildCache::new(&ws, Box::new(LocalDiskBackend::new(ws.join(".anubis-cache")).unwrap()));
        let src = ws.join("out/a.c");
        let dst = ws.join("gen/sub/a.c");
        std::fs::write(&src, "v1").unwrap();
        // As a cached output is: read-only.
        let mut perms = std::fs::metadata(&src).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&src, perms).unwrap();
        let digest = Digest::of_file(&src).unwrap();

        assert!(deploy_file(Some(&cache), &ws, &src, &dst, &digest).unwrap(), "first deploy writes");
        cache.wrote(&dst);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "v1");
        assert!(!std::fs::metadata(&dst).unwrap().permissions().readonly(), "the copy is writable");
        // A write to the deployed file must not reach the output.
        std::fs::write(&dst, "edited").unwrap();
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "v1");
        cache.invalidate(&dst);
        assert!(deploy_file(Some(&cache), &ws, &src, &dst, &digest).unwrap(), "an edited file is put back");
        cache.wrote(&dst);
        assert!(!deploy_file(Some(&cache), &ws, &src, &dst, &digest).unwrap(), "same bytes: nothing written");

        // What the removed link methods left behind is replaced by a copy.
        std::fs::remove_file(&dst).unwrap();
        std::fs::hard_link(&src, &dst).unwrap();
        cache.invalidate(&dst);
        assert!(deploy_file(Some(&cache), &ws, &src, &dst, &digest).unwrap(), "a read-only link is replaced");
        assert!(!std::fs::metadata(&dst).unwrap().permissions().readonly());
    }

    /// A workspace with a tool (exe, one runtime library, one resource) and
    /// two source files, and a cache over it.
    struct Fixture {
        _tmp: tempfile::TempDir,
        ws: Utf8PathBuf,
        tool: CompileExeArtifact,
    }

    fn fixture(dir: &str) -> Fixture {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = utf8(tmp.path()).join(dir);
        let bin = ws.join(".anubis-out/host/tool/bin");
        std::fs::create_dir_all(bin.join("data")).unwrap();
        std::fs::create_dir_all(ws.join("lib")).unwrap();
        std::fs::write(bin.join("tool"), "tool v1").unwrap();
        std::fs::write(ws.join("lib/libdep.so"), "dep v1").unwrap();
        std::fs::write(bin.join("data/table.txt"), "table v1").unwrap();
        std::fs::write(ws.join("a.txt"), "a").unwrap();
        std::fs::write(ws.join("b.txt"), "b").unwrap();
        let tool = CompileExeArtifact {
            output_file: bin.join("tool"),
            runtime_files: vec![ws.join("lib/libdep.so")],
            resources: vec![(bin.join("data/table.txt"), "data/table.txt".into())],
            debug_files: Vec::new(),
        };
        Fixture { _tmp: tmp, ws, tool }
    }

    fn key(f: &Fixture, srcs: &[&str], args: &[&[&str]], outputs: &[&str]) -> Digest {
        let backend = LocalDiskBackend::new(f.ws.join(".anubis-cache")).unwrap();
        let cache = BuildCache::new(&f.ws, Box::new(backend));
        let args: Vec<Vec<String>> =
            args.iter().map(|a| a.iter().map(|s| s.replace("WS/", &format!("{}/", f.ws))).collect()).collect();
        let c = cmd(&f.ws, srcs, args.clone(), outputs);
        let invocations: Vec<Vec<String>> = args.iter().map(|a| resolve_args(a, &f.ws, "out/cmd")).collect();
        cache_key(&cache, &c, &f.tool, &invocations).unwrap()
    }

    #[test]
    fn cache_key_covers_tool_args_srcs_and_outputs() {
        let f = fixture("ws");
        let base = || key(&f, &["a.txt", "b.txt"], &[&["WS/a.txt", "$OUTDIR/a.c"], &["x"]], &["a.c"]);
        let k = base();
        assert_eq!(k, base(), "deterministic");

        // Arguments, and how they split into invocations.
        assert_ne!(k, key(&f, &["a.txt", "b.txt"], &[&["WS/a.txt", "$OUTDIR/a.c"], &["y"]], &["a.c"]));
        assert_ne!(k, key(&f, &["a.txt", "b.txt"], &[&["WS/a.txt"], &["$OUTDIR/a.c", "x"]], &["a.c"]));
        // srcs list and outputs.
        assert_ne!(k, key(&f, &["a.txt"], &[&["WS/a.txt", "$OUTDIR/a.c"], &["x"]], &["a.c"]));
        assert_ne!(k, key(&f, &["a.txt", "b.txt"], &[&["WS/a.txt", "$OUTDIR/a.c"], &["x"]], &["b.c"]));

        // Contents: a source, the exe, a runtime library, a resource. Each
        // key is built with a fresh cache (a new run).
        let mut seen = vec![k];
        for (path, bytes) in [
            (f.ws.join("b.txt"), "b2"),
            (f.tool.output_file.clone(), "tool v2"),
            (f.ws.join("lib/libdep.so"), "dep v2"),
            (f.tool.resources[0].0.clone(), "table v2"),
        ] {
            std::fs::write(&path, bytes).unwrap();
            let k2 = base();
            assert!(!seen.contains(&k2), "changing {path} must change the key");
            seen.push(k2);
        }
    }

    #[test]
    fn cache_key_does_not_depend_on_workspace_location() {
        let a = fixture("one");
        let b = fixture("two");
        let k = |f: &Fixture| key(f, &["a.txt"], &[&["WS/a.txt", "$OUTDIR/a.c"]], &["a.c"]);
        assert_eq!(k(&a), k(&b));
    }

    #[test]
    fn missing_src_is_an_error() {
        let f = fixture("ws");
        let backend = LocalDiskBackend::new(f.ws.join(".anubis-cache")).unwrap();
        let cache = BuildCache::new(&f.ws, Box::new(backend));
        let c = cmd(&f.ws, &["a.txt", "nope.txt"], vec![vec![]], &["a.c"]);
        let err = cache_key(&cache, &c, &f.tool, &[vec![]]).unwrap_err().to_string();
        assert!(err.contains("nope.txt"), "{err}");
    }
}
