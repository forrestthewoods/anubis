//! The `anubis` command line: `build`, `run`, `targets`, `clean`, `dump` and
//! `install-toolchains`.

use anubis::anubis::*;
use anubis::install_toolchains::*;
use anubis::logging;
use anubis::logging::*;
use anubis::papyrus;
use anubis::papyrus_fmt;
use anubis::progress;
use anubis::rules::*;
use anubis::util::SlashFix;
use anubis::{anyhow_loc, bail_loc, timed_span};

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

// ----------------------------------------------------------------------------
// CLI args
// ----------------------------------------------------------------------------
#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    /// Set the log level (error, warn, info, debug, trace, fullverbose).
    /// 'fullverbose' enables trace logging AND verbose output from external tools (e.g., clang -v).
    #[arg(short = 'l', long, default_value = "info", global = true)]
    log_level: LogLevel,

    /// Number of parallel workers (defaults to number of physical CPU cores)
    #[arg(short, long, global = true)]
    workers: Option<usize>,

    /// Disable live progress display; use plain scrolling output instead
    #[arg(long, global = true)]
    no_tui: bool,

    /// Enable profiling and write trace to specified file (viewable in Firefox Profiler, chrome://tracing, or Perfetto)
    #[arg(short = 'p', long, global = true)]
    profile: Option<PathBuf>,

    /// After a build, print job time by kind and the slowest jobs
    #[arg(long, global = true)]
    stats: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Build(BuildArgs),
    Dump(DumpArgs),
    Run(RunArgs),
    /// List targets: `//dir` for one ANUBIS file, `//dir/...` recursively.
    Targets(TargetsArgs),
    /// Delete everything Anubis writes: .anubis-out, .anubis-temp and the build cache (.anubis-cache).
    Clean,
    /// Format ANUBIS files (all under the project root by default).
    Fmt(FmtArgs),
    InstallToolchains(InstallToolchainsArgs),
}

#[derive(Debug, Parser)]
struct FmtArgs {
    /// Files to format. Default: every ANUBIS file under the project root.
    files: Vec<PathBuf>,
    /// Change nothing; list unformatted files and fail if there are any.
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Parser)]
struct TargetsArgs {
    /// Directories (`//samples/basic/simple_cpp`) or patterns (`//samples/...`, `///...` for all).
    #[arg(required = true, num_args = 1..)]
    paths: Vec<String>,
}

#[derive(Debug, Parser)]
struct RunArgs {
    #[arg(short, long)]
    mode: String,

    #[arg(short, long)]
    target: String,

    /// Arguments to pass to the executable
    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Debug, Parser)]
struct BuildArgs {
    /// Mode(s) to build in. Without any, targets are built modeless, which
    /// only rules that don't need a mode support.
    #[arg(short, long, visible_alias = "mode", num_args = 1..)]
    modes: Vec<String>,

    #[arg(short, long, required = true, visible_alias = "target", num_args = 1..)]
    targets: Vec<String>,

    /// Disable the build cache (.anubis-cache): run every action.
    #[arg(long)]
    no_cache: bool,

    /// After a successful build, write each target's output files as JSON:
    /// `[{"mode", "target", "vars", "files"}]`. Files are what a package
    /// would carry: a shared library or extension plus the shared libraries
    /// it needs at run time, or an executable. Used by the hatch-anubis
    /// build hook (python/hatch-anubis).
    #[arg(long)]
    outputs_json: Option<std::path::PathBuf>,
}

#[derive(Debug, Parser)]
struct DumpArgs {
    /// Mode target (e.g., //mode:win_dev)
    #[arg(short, long)]
    mode: String,

    /// Target to dump (e.g., //toolchains:default)
    #[arg(short, long)]
    target: String,
}

// ----------------------------------------------------------------------------
// CLI Commands
// ----------------------------------------------------------------------------
fn dump(args: &DumpArgs, verbose_tools: bool) -> anyhow::Result<()> {
    tracing::info!("Dumping target: {} with mode: {}", args.target, args.mode);

    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_owned();

    let anubis_config = load_anubis_config(&anubis_root_file)?;
    let anubis = Anubis::new(project_root, verbose_tools, &anubis_config)?;

    let mode_target = AnubisTarget::new(&args.mode)?;
    let target = AnubisTarget::new(&args.target)?;

    let mode = anubis.get_mode(&mode_target)?;

    let config_relpath = target.get_config_relpath();
    let resolved_config = anubis.get_resolved_config(&config_relpath, &mode)?;

    let target_value = resolved_config.get_named_object(target.target_name())?;

    println!("# Resolved target: {} with mode: {}", args.target, args.mode);
    println!("# Mode variables:");
    for (k, v) in mode.vars.iter() {
        println!("#   {} = {}", k, v);
    }
    println!();
    println!("{}", papyrus::format_value(target_value, 0));

    Ok(())
}

fn fmt(args: &FmtArgs) -> anyhow::Result<()> {
    let mut files = args.files.clone();
    if files.is_empty() {
        let root = find_anubis_root(&std::env::current_dir()?)?;
        let root = root.parent().ok_or_else(|| anyhow_loc!("no parent for [{}]", root))?;
        let mut dirs = vec![root.as_std_path().to_owned()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if path.is_dir() && !name.starts_with('.') && name != "target" {
                    dirs.push(path);
                } else if name == "ANUBIS" {
                    files.push(path);
                }
            }
        }
        files.sort();
    }

    let mut unformatted = 0;
    let mut failed = 0;
    for file in &files {
        let src = std::fs::read_to_string(file).map_err(|e| anyhow_loc!("reading {}: {}", file.display(), e))?;
        match papyrus_fmt::format(&src) {
            Ok(out) if out == src => {}
            Ok(out) => {
                unformatted += 1;
                if args.check {
                    println!("{}", file.display());
                } else {
                    std::fs::write(file, out)?;
                    println!("formatted {}", file.display());
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: {:#}", file.display(), e);
            }
        }
    }
    if failed > 0 || (args.check && unformatted > 0) {
        bail_loc!("{} file(s) need formatting, {} couldn't be formatted", unformatted, failed);
    }
    Ok(())
}

fn clean() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_owned();

    for dir in [".anubis-out", ".anubis-temp", ".anubis-cache"] {
        let path = project_root.join(dir);
        // Outputs are read-only hard links to cache blobs. Deleting them is
        // fine (on Windows too); clearing the read-only bit first would also
        // clear it on the blob.
        match std::fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!("Deleted {}", path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail_loc!("Failed to delete {}: {}", path, e),
        }
    }
    Ok(())
}

fn targets(args: &TargetsArgs, verbose_tools: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_owned();
    let anubis_config = load_anubis_config(&anubis_root_file)?;
    let anubis = Anubis::new(project_root.clone(), verbose_tools, &anubis_config)?;

    let mut all = std::collections::BTreeSet::new();
    for path in &args.paths {
        if TargetPattern::parse(path).is_some() {
            all.extend(anubis.expand_targets(std::slice::from_ref(path))?.into_iter().map(|t| t.target_path().to_owned()));
        } else {
            let dir = path
                .strip_prefix("//")
                .ok_or_else(|| anyhow_loc!("Expected //dir or //dir/..., got [{}]", path))?
                .trim_end_matches(':')
                .trim_end_matches('/');
            all.extend(targets_in_dir(project_root.as_std_path(), dir, &anubis.rule_typeinfos)?);
        }
    }
    for t in all {
        println!("{t}");
    }
    Ok(())
}

fn build(
    args: &BuildArgs,
    workers: Option<usize>,
    verbose_tools: bool,
    display_mode: progress::DisplayMode,
    verbose_display: bool,
    stats: bool,
) -> anyhow::Result<()> {
    tracing::info!("Starting Anubis build command: [{:?}]", args);

    // Nuke environment variables to ensure clean build environment
    let keys: Vec<_> = std::env::vars_os().map(|(key, _)| key).collect();
    for key in keys {
        if let Some(key_str) = key.to_str() {
            // Skip RUST_ macros (such as RUST_BACKTRACE)
            if key_str.contains("RUST_") {
                continue;
            }

            std::env::remove_var(key_str);
        }
    }

    let cwd = std::env::current_dir()?;
    let outputs_json = args.outputs_json.as_ref().map(|p| cwd.join(p));
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_owned();
    tracing::debug!("Found project root: {:?}", project_root);

    let anubis_root = load_anubis_config(&anubis_root_file)?;
    let mut anubis = Anubis::new(project_root.clone(), verbose_tools, &anubis_root)?;
    if !args.no_cache {
        anubis.build_cache = Some(anubis::build_cache::BuildCache::local(&project_root)?);
    }
    anubis.hints = anubis::hints::Hints::load(project_root.join(".anubis-cache").join("hints"));
    let anubis = Arc::new(anubis);

    // Expand any target patterns (e.g., "//samples/basic/..." -> all targets under samples/basic/)
    let expanded_targets = anubis.expand_targets(&args.targets)?;

    if expanded_targets.is_empty() {
        tracing::warn!("No targets to build");
        return Ok(());
    }

    tracing::info!("Building {} target(s)", expanded_targets.len());

    let modes: Vec<AnubisTarget> = args.modes.iter()
        .map(|m| AnubisTarget::new(m))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let toolchain = AnubisTarget::new(DEFAULT_TOOLCHAIN)?;

    let anubis_targets = expanded_targets;

    for target in &anubis_targets {
        tracing::info!("Building target: {}", target.target_path());
    }

    // Create progress display for live build output
    // Drop impl handles shutdown (prints summary, clears TUI) on both success and error paths.
    let num_workers = workers.unwrap_or_else(num_cpus::get_physical);
    let progress = progress::ProgressDisplay::with_stats(display_mode, num_workers, verbose_display, stats);

    // Build all targets together with a shared JobSystem
    // This ensures job caches remain valid (job IDs are per-JobSystem)
    let _build_span = timed_span!(tracing::Level::INFO, "build_execution");
    let result = build_targets(anubis.clone(), &modes, &toolchain, &anubis_targets, num_workers, progress.sender());
    if let Some(cache) = &anubis.build_cache {
        let (rehashed, reused) = cache.stat_counts();
        tracing::info!(
            "build cache: {} hits, {} misses ({} files hashed, {} unchanged by stat)",
            cache.hits(), cache.misses(), rehashed, reused
        );
        cache.save();
    }
    if let Err(e) = anubis.hints.save() {
        tracing::warn!("could not save scheduling hints: {e:#}");
    }
    let artifacts = result?;

    if let Some(path) = outputs_json {
        write_outputs_json(&anubis, &path, &modes, &anubis_targets, &artifacts)?;
    }

    Ok(())
}

/// `anubis build --outputs-json`: artifacts come back mode-major, one per
/// (mode, target).
fn write_outputs_json(
    anubis: &Arc<Anubis>,
    path: &std::path::Path,
    modes: &[AnubisTarget],
    targets: &[AnubisTarget],
    artifacts: &[Arc<dyn anubis::job_system::JobArtifact>],
) -> anyhow::Result<()> {
    let mut entries = Vec::new();
    let mode_list: Vec<Option<&AnubisTarget>> =
        if modes.is_empty() { vec![None] } else { modes.iter().map(Some).collect() };
    let mut artifacts = artifacts.iter();
    for mode in &mode_list {
        let vars = match mode {
            Some(m) => anubis.get_mode(m)?.vars.clone(),
            None => Default::default(),
        };
        for target in targets {
            let artifact = artifacts.next().ok_or_else(|| anyhow_loc!("missing artifact for {}", target.target_path()))?;
            let files: Vec<String> = if let Some(out) = artifact.clone().downcast_arc::<cc_rules::CcBuildOutput>().ok() {
                out.runtime_files.iter().map(|f| f.to_string()).collect()
            } else if let Some(exe) = artifact.clone().downcast_arc::<cc_rules::CompileExeArtifact>().ok() {
                vec![exe.output_file.to_string()]
            } else {
                Vec::new()
            };
            entries.push(serde_json::json!({
                "mode": mode.map(|m| m.target_path()),
                "target": target.target_path(),
                "vars": vars,
                "files": files,
            }));
        }
    }
    let text = serde_json::to_string_pretty(&entries)?;
    anyhow::Context::with_context(std::fs::write(path, text), || format!("writing {}", path.display()))?;
    Ok(())
}

fn run(
    args: &RunArgs,
    workers: Option<usize>,
    verbose_tools: bool,
    display_mode: progress::DisplayMode,
    verbose_display: bool,
    stats: bool,
) -> anyhow::Result<()> {
    tracing::info!("Starting Anubis run command: [{:?}]", args);

    // Nuke environment variables to ensure clean build environment
    let keys: Vec<_> = std::env::vars_os().map(|(key, _)| key).collect();
    for key in keys {
        if let Some(key_str) = key.to_str() {
            // Skip RUST_ macros (such as RUST_BACKTRACE)
            if key_str.contains("RUST_") {
                continue;
            }

            std::env::remove_var(key_str);
        }
    }

    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_owned();
    tracing::debug!("Found project root: {:?}", project_root);

    let anubis_config = load_anubis_config(&anubis_root_file)?;
    let anubis = Arc::new(Anubis::new(project_root.clone(), verbose_tools, &anubis_config)?);

    let mode = AnubisTarget::new(&args.mode)?;
    let toolchain = AnubisTarget::new(DEFAULT_TOOLCHAIN)?;
    let anubis_target = AnubisTarget::new(&args.target)?;

    // Fail before building if the target can't be run.
    let rule = anubis.get_rule(&anubis_target, &*anubis.get_mode(&mode)?)?;
    if !rule.is_runnable() {
        bail_loc!("Target [{}] is not runnable. `anubis run` only runs cc_binary targets.", args.target);
    }

    tracing::info!("Building target: {}", anubis_target.target_path());

    // Build the target with a progress display.
    // progress is scoped so its Drop runs before we launch the executable.
    let num_workers = workers.unwrap_or_else(num_cpus::get_physical);
    let artifact = {
        let progress = progress::ProgressDisplay::with_stats(display_mode, num_workers, verbose_display, stats);
        let _build_span = timed_span!(tracing::Level::INFO, "build_execution");
        build_single_target(
            anubis.clone(),
            &mode,
            &toolchain,
            &anubis_target,
            num_workers,
            progress.sender(),
        )?
    };

    let exe_artifact = artifact.downcast_arc::<cc_rules::CompileExeArtifact>().map_err(|_| {
        anyhow_loc!(
            "Target [{}] is runnable but its build produced no executable.",
            args.target
        )
    })?;

    let exe_path = exe_artifact.output_file.clone().slash_fix();

    tracing::info!("Running executable: {:?}", exe_path);

    if !exe_path.exists() {
        bail_loc!("Executable not found at {:?}. Build may have failed.", exe_path);
    }

    let status = std::process::Command::new(&exe_path).args(&args.args).status()?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        tracing::warn!("Executable exited with code: {}", code);
        std::process::exit(code);
    }

    Ok(())
}

// ----------------------------------------------------------------------------
// Main
// ----------------------------------------------------------------------------
fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let log_config = LogConfig {
        level: args.log_level,
        format: LogFormat::Simple,
        output: LogOutput::Stdout,
        enable_timing: true,
        enable_spans: true,
    };

    // Hold the profile guard for the duration of the program (if profiling)
    let _profile_guard = if let Some(ref profile_path) = args.profile {
        Some(logging::init_logging_with_profile(&log_config, profile_path)?)
    } else {
        init_logging(&log_config)?;
        None
    };

    let verbose_tools = args.log_level.is_verbose_tools();

    // Detect TTY before environment variables are cleared (isatty uses syscall, not env)
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let display_mode = if !args.no_tui && is_tty {
        progress::DisplayMode::Live
    } else {
        progress::DisplayMode::Simple
    };
    let verbose_display = matches!(args.log_level, LogLevel::Debug | LogLevel::Trace | LogLevel::FullVerbose);

    let result = match args.command {
        Commands::Build(b) => build(&b, args.workers, verbose_tools, display_mode, verbose_display, args.stats),
        Commands::Dump(d) => dump(&d, verbose_tools),
        Commands::Run(r) => run(&r, args.workers, verbose_tools, display_mode, verbose_display, args.stats),
        Commands::Targets(t) => targets(&t, verbose_tools),
        Commands::Clean => clean(),
        Commands::Fmt(f) => fmt(&f),
        Commands::InstallToolchains(t) => install_toolchains(&t),
    };

    if let Err(e) = &result {
        tracing::error!("{}", e);
    }

    result
}
