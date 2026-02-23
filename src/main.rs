use anubis::anubis::*;
use anubis::install_toolchains::*;
use anubis::logging;
use anubis::logging::*;
use anubis::papyrus;
use anubis::progress;
use anubis::rules::*;
use anubis::util::SlashFix;
use anubis::{anyhow_loc, bail_loc, function_name, timed_span};

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

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Build(BuildArgs),
    Dump(DumpArgs),
    Run(RunArgs),
    InstallToolchains(InstallToolchainsArgs),
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
    #[arg(short, long, required = true, visible_alias = "mode", num_args = 1..)]
    modes: Vec<String>,

    #[arg(short, long, required = true, visible_alias = "target", num_args = 1..)]
    targets: Vec<String>,
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

    // Find the project root
    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_owned();

    // Load root config and create anubis
    let anubis_config = load_anubis_config(&anubis_root_file)?;
    let anubis = Anubis::new(project_root, verbose_tools, &anubis_config)?;

    // Parse mode and target
    let mode_target = AnubisTarget::new(&args.mode)?;
    let target = AnubisTarget::new(&args.target)?;

    // Get mode
    let mode = anubis.get_mode(&mode_target)?;

    // Get resolved config for the target
    let config_relpath = target.get_config_relpath();
    let resolved_config = anubis.get_resolved_config(&config_relpath, &mode)?;

    // Find the specific target within the config
    let target_value = resolved_config.get_named_object(target.target_name())?;

    // Format and print the resolved target
    println!("# Resolved target: {} with mode: {}", args.target, args.mode);
    println!("# Mode variables:");
    for (k, v) in mode.vars.iter() {
        println!("#   {} = {}", k, v);
    }
    println!();
    println!("{}", papyrus::format_value(target_value, 0));

    Ok(())
}

fn build(
    args: &BuildArgs,
    workers: Option<usize>,
    verbose_tools: bool,
    display_mode: progress::DisplayMode,
    verbose_display: bool,
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

    // Find the project root by looking for .anubis_root file
    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_owned();
    tracing::debug!("Found project root: {:?}", project_root);

    // Load root config and create anubis with the discovered project root
    let anubis_root = load_anubis_config(&anubis_root_file)?;
    let anubis = Arc::new(Anubis::new(project_root.clone(), verbose_tools, &anubis_root)?);

    // Load disk cache for pre_hash_and_cache_dirs
    let hash_cache_path = project_root.join(".anubis-out").join("hash_cache.cache").slash_fix();
    anubis.fs_tree_hasher.load_cache(&hash_cache_path);

    // Hash pre_hash_and_cache_dirs, skipping any already loaded from disk cache
    let mut cache_dirty = false;
    for dir_name in &anubis_root.pre_hash_and_cache_dirs {
        let dir = project_root.join(dir_name).slash_fix();
        if anubis.fs_tree_hasher.has_dir_cached(&dir) {
            tracing::info!("{dir_name}/ loaded from disk cache");
        } else {
            let t0 = std::time::Instant::now();
            anubis.fs_tree_hasher.hash_dir(&dir)?;
            tracing::info!("{dir_name}/ hashed in {}", anubis::util::format_duration(t0.elapsed()));
            cache_dirty = true;
        }
    }

    // Save disk cache if anything was newly hashed
    if cache_dirty {
        anubis.fs_tree_hasher.save_cache(&hash_cache_path);
    }

    // Hash pre_hash_dirs (no disk cache, just pre-populate the in-memory hasher)
    for dir_name in &anubis_root.pre_hash_dirs {
        let dir = project_root.join(dir_name).slash_fix();
        let t0 = std::time::Instant::now();
        anubis.fs_tree_hasher.hash_dir(&dir)?;
        tracing::info!("{dir_name}/ hashed in {}", anubis::util::format_duration(t0.elapsed()));
    }

    // Expand any target patterns (e.g., "//samples/basic/..." -> all targets under samples/basic/)
    let expanded_targets = anubis.expand_targets(&args.targets)?;

    if expanded_targets.is_empty() {
        tracing::warn!("No targets to build");
        return Ok(());
    }

    tracing::info!(
        "Building {} target(s): {:?}",
        expanded_targets.len(),
        expanded_targets
    );

    // Parse target paths
    let modes: Vec<AnubisTarget> = args.modes.iter()
        .map(|m| AnubisTarget::new(m))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let toolchain = AnubisTarget::new("//toolchains:default")?;

    let anubis_targets: Vec<AnubisTarget> =
        expanded_targets.iter().map(|t| AnubisTarget::new(t)).collect::<anyhow::Result<Vec<_>>>()?;

    for target in &anubis_targets {
        tracing::info!("Building target: {}", target.target_path());
    }

    // Create progress display for live build output
    // Drop impl handles shutdown (prints summary, clears TUI) on both success and error paths.
    let num_workers = workers.unwrap_or_else(num_cpus::get_physical);
    let progress = progress::ProgressDisplay::new(display_mode, num_workers, verbose_display);

    // Build all targets together with a shared JobSystem
    // This ensures job caches remain valid (job IDs are per-JobSystem)
    let _build_span = timed_span!(tracing::Level::INFO, "build_execution");
    build_targets(anubis, &modes, &toolchain, &anubis_targets, num_workers, progress.sender())?;

    Ok(())
}

fn run(
    args: &RunArgs,
    workers: Option<usize>,
    verbose_tools: bool,
    display_mode: progress::DisplayMode,
    verbose_display: bool,
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

    // Find the project root by looking for .anubis_root file
    let cwd = std::env::current_dir()?;
    let anubis_root_file = find_anubis_root(&cwd)?;
    let project_root = anubis_root_file
        .parent()
        .ok_or_else(|| anyhow_loc!("Could not get parent directory of .anubis_root"))?
        .to_owned();
    tracing::debug!("Found project root: {:?}", project_root);

    // Load root config and create anubis with the discovered project root
    let anubis_config = load_anubis_config(&anubis_root_file)?;
    let anubis = Arc::new(Anubis::new(project_root.clone(), verbose_tools, &anubis_config)?);

    // Build the target
    let mode = AnubisTarget::new(&args.mode)?;
    let toolchain = AnubisTarget::new("//toolchains:default")?;
    let anubis_target = AnubisTarget::new(&args.target)?;

    tracing::info!("Building target: {}", anubis_target.target_path());

    // Build the target with a progress display.
    // progress is scoped so its Drop runs before we launch the executable.
    let num_workers = workers.unwrap_or_else(num_cpus::get_physical);
    let artifact = {
        let progress = progress::ProgressDisplay::new(display_mode, num_workers, verbose_display);
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

    // Get the executable path from the build artifact
    let exe_artifact = artifact.downcast_arc::<cc_rules::CompileExeArtifact>().map_err(|_| {
        anyhow_loc!(
            "Target '{}' is not an executable. The 'run' command only works with cpp_binary targets.",
            args.target
        )
    })?;

    // Normalize the path for cross-platform compatibility
    let exe_path = exe_artifact.output_file.clone().slash_fix();

    tracing::info!("Running executable: {:?}", exe_path);

    // Verify the executable exists
    if !exe_path.exists() {
        bail_loc!("Executable not found at {:?}. Build may have failed.", exe_path);
    }

    // Run the executable
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
    // Parse command-line arguments first to get the log level
    let args = Args::parse();

    // Initialize logging system with the specified log level
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

    // Determine if we should enable verbose output from external tools
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
        Commands::Build(b) => build(&b, args.workers, verbose_tools, display_mode, verbose_display),
        Commands::Dump(d) => dump(&d, verbose_tools),
        Commands::Run(r) => run(&r, args.workers, verbose_tools, display_mode, verbose_display),
        Commands::InstallToolchains(t) => install_toolchains(&t),
    };

    // Dump errors
    if let Err(e) = &result {
        tracing::error!("{}", e);
    }

    // Return result
    result
}
