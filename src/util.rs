//! Small shared helpers: path separator fixing ([`SlashFix`]), fallible path
//! macros (`parent_dir!`, `file_name!`), quick hashes and duration formatting.

use crate::{anyhow_loc, function_name};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;
use xxhash_rust::xxh3::Xxh3;

// ----------------------------------------------------------------------------
// Fallible path operations
// ----------------------------------------------------------------------------

/// `path.parent()` as a `Result` whose error names the path and the call site.
/// A macro, not a function, so `anyhow_loc!` reports the caller's line.
#[macro_export]
macro_rules! parent_dir {
    ($path:expr) => {{
        let p = &$path;
        p.parent().ok_or_else(|| $crate::anyhow_loc!("No parent dir for [{:?}]", p))
    }};
}

/// `path.file_name()` as a `Result`; see [`parent_dir!`].
#[macro_export]
macro_rules! file_name {
    ($path:expr) => {{
        let p = &$path;
        p.file_name().ok_or_else(|| $crate::anyhow_loc!("No file name for [{:?}]", p))
    }};
}

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
// SlashFix
// ----------------------------------------------------------------------------
pub trait SlashFix {
    fn slash_fix(self) -> Self;
}

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

impl SlashFix for Utf8PathBuf {
    fn slash_fix(self) -> Self {
        self.as_str().replace("\\", "/").into()
    }
}

// ----------------------------------------------------------------------------
// Superluminal
// ----------------------------------------------------------------------------
pub struct SuperluminalGuard;

impl Drop for SuperluminalGuard {
    fn drop(&mut self) {
        superluminal_perf::end_event();
    }
}

#[macro_export]
macro_rules! superluminal_span {
    ($name:expr) => {{
        superluminal_perf::begin_event($name);
        $crate::util::SuperluminalGuard
    }};
}

// ----------------------------------------------------------------------------
// Hashing
// ----------------------------------------------------------------------------
pub fn quick_hash<T: Hash>(t: &T) -> u64 {
    let mut h = Xxh3::new();
    t.hash(&mut h);
    h.finish()
}

pub fn hash_combine(seed: u64, hash: u64) -> u64 {
    seed ^ (hash.wrapping_add(0x9e3779b9).wrapping_add(seed << 6).wrapping_add(seed >> 2))
}

// ----------------------------------------------------------------------------
// Path
// ----------------------------------------------------------------------------
pub fn strip_prefix<'a>(path: &'a Utf8Path, prefix: &Utf8Path) -> anyhow::Result<&'a Utf8Path> {
    path.strip_prefix(prefix).with_context(|| anyhow::anyhow!("Failed to prefix_strip [{}] from [{}]", prefix, path))
}

/// Cheap string-only path normalization: lowercase + forward slashes on
/// Windows, forward slashes only elsewhere. No filesystem access.
pub fn normalize_path(path: &Utf8Path) -> Utf8PathBuf {
    #[cfg(windows)]
    {
        let p: Utf8PathBuf = path.as_str().to_lowercase().into();
        p.slash_fix()
    }
    #[cfg(not(windows))]
    {
        let p: Utf8PathBuf = path.into();
        p.slash_fix()
    }
}
