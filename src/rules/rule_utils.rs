//! Utility functions shared across rule implementations.

use anyhow::Context;
use camino::Utf8PathBuf;
use std::path::Path;
use std::process::Output;

use crate::{anyhow_loc, function_name};

/// Ensures that the directory for a given file path exists, creating it if necessary.
pub fn ensure_directory_for_file(filepath: &Path) -> anyhow::Result<()> {
    let dir = crate::parent_dir!(filepath)?;
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Ensures that a directory exists, creating it if necessary.
pub fn ensure_directory(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to ensure directories for [{:?}]", dir))
}

/// Run `exe` with captured stdout/stderr. With `verbose_tools`, the output
/// is also logged at info level. Errors only if the process could not start.
pub fn run_command_verbose(exe: &Path, args: &[String], verbose_tools: bool) -> anyhow::Result<Output> {
    let command_display = format!("{} {}", exe.display(), args.join(" "));

    tracing::trace!("Executing command: {command_display}",);

    let output = std::process::Command::new(exe)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("Failed to execute command: {command_display}",))?;

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
