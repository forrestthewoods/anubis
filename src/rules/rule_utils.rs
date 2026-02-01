//! Utility functions shared across rule implementations.

use anyhow::Context;
use std::path::Path;
use std::process::Output;

use crate::{anyhow_loc, function_name};

/// Ensures that the directory for a given file path exists, creating it if necessary.
pub fn ensure_directory_for_file(filepath: &Path) -> anyhow::Result<()> {
    let dir =
        filepath.parent().ok_or_else(|| anyhow_loc!("Could not get dir from filepath [{:?}]", filepath))?;
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Ensures that a directory exists, creating it if necessary.
pub fn ensure_directory(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to ensure directories for [{:?}]", dir))
}

/// Executes a command with the given executable and arguments.
///
/// This function provides a standardized way to run subprocesses with:
/// - Trace-level logging of the command being executed
/// - Piped stdout/stderr for capture
/// - Consistent error handling
///
/// # Arguments
/// * `exe` - Path to the executable to run
/// * `args` - Arguments to pass to the executable
///
/// # Returns
/// The command output on success, or an error if the command failed to execute.
pub fn run_command(exe: &Path, args: &[String]) -> anyhow::Result<Output> {
    run_command_verbose(exe, args, false)
}

/// Executes a command with the given executable and arguments, with optional verbose output.
///
/// This function provides a standardized way to run subprocesses with:
/// - Trace-level logging of the command being executed
/// - Piped stdout/stderr for capture
/// - Consistent error handling
/// - Optional info-level logging of stdout/stderr (when verbose_tools is true)
///
/// # Arguments
/// * `exe` - Path to the executable to run
/// * `args` - Arguments to pass to the executable
/// * `verbose_tools` - If true, logs stdout/stderr at info level after command completes
///
/// # Returns
/// The command output on success, or an error if the command failed to execute.
pub fn run_command_verbose(exe: &Path, args: &[String], verbose_tools: bool) -> anyhow::Result<Output> {
    // Format the command for logging
    let command_display = format!("{} {}", exe.display(), args.join(" "));

    tracing::trace!("Executing command: {command_display}",);

    let output = std::process::Command::new(exe)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("Failed to execute command: {command_display}",))?;

    // Log stdout/stderr at info level when verbose_tools is enabled
    if verbose_tools {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !stdout.is_empty() {
            tracing::info!(target: "command_output", "stdout:\n{}", stdout);
        }
        if !stderr.is_empty() {
            tracing::info!(target: "command_output", "stderr:\n{}", stderr);
        }
    }

    Ok(output)
}
