//! Utility functions shared across rule implementations.

use anyhow::Context;
use std::path::Path;

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
