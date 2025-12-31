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
    // Format the command for logging
    let command_display = format!("{} {}", exe.display(), args.join(" "));

    tracing::trace!("Executing command: {command_display}",);

    std::process::Command::new(exe)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("Failed to execute command: {command_display}",))
}
