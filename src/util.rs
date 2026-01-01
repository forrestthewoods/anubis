use std::path::{Path, PathBuf};
use std::time::Duration;

// ----------------------------------------------------------------------------
// Duration Formatting
// ----------------------------------------------------------------------------

/// Formats a duration in a human-readable way.
/// - < 1 second: displays as milliseconds (e.g., "450ms")
/// - < 60 seconds: displays as seconds with 1 decimal place (e.g., "12.3s")
/// - >= 60 seconds: displays as minutes and seconds (e.g., "2m 30s")
pub fn format_duration(duration: Duration) -> String {
    let total_ms = duration.as_millis();
    let total_secs = duration.as_secs_f64();

    if total_ms < 1000 {
        // Less than 1 second - show milliseconds
        format!("{}ms", total_ms)
    } else if total_secs < 60.0 {
        // Less than 1 minute - show seconds with 1 decimal
        format!("{:.1}s", total_secs)
    } else {
        // 1 minute or more - show minutes and seconds
        let minutes = (total_secs / 60.0).floor() as u64;
        let remaining_secs = (total_secs % 60.0).round() as u64;
        format!("{}m {}s", minutes, remaining_secs)
    }
}

// ----------------------------------------------------------------------------
// Declarations
// ----------------------------------------------------------------------------
pub trait SlashFix {
    fn slash_fix(self) -> Self;
}

// ----------------------------------------------------------------------------
// Global Anubis Paths
// ----------------------------------------------------------------------------

/// Returns the user-level Anubis home directory.
/// - Windows: `%USERPROFILE%/.anubis`
/// - Linux/macOS: `~/.anubis`
pub fn get_anubis_home() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("C:\\Users\\Default"))
            .join(".anubis")
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join(".anubis")
    }
}

/// Returns the global toolchains directory (`~/.anubis/toolchains`).
pub fn get_global_toolchains_dir() -> PathBuf {
    get_anubis_home().join("toolchains")
}

/// Returns the global toolchain database path (`~/.anubis/toolchains.db`).
pub fn get_global_db_path() -> PathBuf {
    get_anubis_home().join("toolchains.db")
}

/// Returns the global temp directory for downloads (`~/.anubis/temp`).
pub fn get_global_temp_dir() -> PathBuf {
    get_anubis_home().join("temp")
}

// ----------------------------------------------------------------------------
// Symlink Utilities
// ----------------------------------------------------------------------------

/// Creates a directory symlink from `link_path` pointing to `target`.
/// On Windows, requires Developer Mode or Administrator privileges.
#[cfg(windows)]
pub fn create_directory_symlink(target: &Path, link_path: &Path) -> anyhow::Result<()> {
    use std::fs;
    use std::os::windows::fs::symlink_dir;

    // Remove existing symlink or directory if present
    if link_path.exists() || link_path.symlink_metadata().is_ok() {
        if link_path.symlink_metadata()?.file_type().is_symlink() {
            fs::remove_dir(link_path)?;
        } else if link_path.is_dir() {
            tracing::warn!(
                "Removing existing directory at {} to create symlink",
                link_path.display()
            );
            fs::remove_dir_all(link_path)?;
        }
    }

    // Create parent directory if needed
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)?;
    }

    symlink_dir(target, link_path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to create symlink from {} to {}\n\n\
             On Windows, creating symlinks requires either:\n\
             1. Developer Mode enabled (Settings > Update & Security > For Developers)\n\
             2. Running as Administrator\n\n\
             Please enable Developer Mode and try again.\n\n\
             Error: {}",
            link_path.display(),
            target.display(),
            e
        )
    })
}

/// Creates a directory symlink from `link_path` pointing to `target`.
#[cfg(not(windows))]
pub fn create_directory_symlink(target: &Path, link_path: &Path) -> anyhow::Result<()> {
    use std::fs;
    use std::os::unix::fs::symlink;

    // Remove existing symlink or directory if present
    if link_path.exists() || link_path.symlink_metadata().is_ok() {
        if link_path.symlink_metadata()?.file_type().is_symlink() {
            fs::remove_file(link_path)?;
        } else if link_path.is_dir() {
            tracing::warn!(
                "Removing existing directory at {} to create symlink",
                link_path.display()
            );
            fs::remove_dir_all(link_path)?;
        }
    }

    // Create parent directory if needed
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)?;
    }

    symlink(target, link_path)?;
    Ok(())
}

/// Checks if the given path is a symlink.
pub fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Reads the target of a symlink, if the path is a symlink.
pub fn read_symlink_target(path: &Path) -> Option<PathBuf> {
    std::fs::read_link(path).ok()
}

// ----------------------------------------------------------------------------
// Implementations
// ----------------------------------------------------------------------------
impl SlashFix for std::path::PathBuf {
    fn slash_fix(self) -> Self {
        self.to_string_lossy().to_string().replace("\\", "/").into()
    }
}

impl SlashFix for String {
    fn slash_fix(self) -> Self {
        self.replace("\\", "/")
    }
}
