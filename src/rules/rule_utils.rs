//! Utility functions shared across rule implementations.

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use std::path::Path;
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{anyhow_loc, function_name};

/// Ensures that the directory for a given file path exists, creating it if necessary.
pub fn ensure_directory_for_file(filepath: &Path) -> anyhow::Result<()> {
    let dir = crate::parent_dir!(filepath)?;
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// `path` relative to the workspace root (every tool's working directory),
/// or as is if it isn't under the root.
pub fn ws_relative<'a>(path: &'a Utf8Path, root: &Utf8Path) -> &'a Utf8Path {
    path.strip_prefix(root).unwrap_or(path)
}

/// Ensures that a directory exists, creating it if necessary.
pub fn ensure_directory(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to ensure directories for [{:?}]", dir))
}

/// How much work this process did, for the end-of-build summary: tool
/// processes started ([`run_command_verbose`]) and actions run inside Anubis
/// (e.g. writing a zip). A build with nothing to do must leave both at 0;
/// cache misses can't show that, since a rule that never asks the cache
/// never counts one. `tests/noop_build.rs` checks it.
pub static TOOLS_RUN: AtomicU64 = AtomicU64::new(0);
pub static IN_PROCESS_ACTIONS_RUN: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Whether this thread has counted work since [`take_worked`]. A job
    /// runs on one worker thread, so the worker learns from it whether the
    /// job did work or only found its results in the cache.
    static WORKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn count_work(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
    WORKED.with(|w| w.set(true));
}

/// Whether this thread counted a tool or in-process action since the last
/// call.
pub fn take_worked() -> bool {
    WORKED.with(|w| w.replace(false))
}

/// Count one action that makes new bytes inside Anubis instead of running a tool.
pub fn count_in_process_action() {
    count_work(&IN_PROCESS_ACTIONS_RUN);
}

/// Run `exe` with captured stdout/stderr. With `verbose_tools`, the output
/// is also logged at info level. Errors only if the process could not start.
/// Every tool runs in the workspace root (`cwd`), so a relative path in its
/// argv or output (e.g. a compile's `.d`) means the same file wherever
/// `anubis` was started; cache keys and the `.d` check assume that.
pub fn run_command_verbose(exe: &Path, args: &[String], verbose_tools: bool, cwd: &camino::Utf8Path) -> anyhow::Result<Output> {
    let command_display = || format!("{} {}", exe.display(), args.join(" "));
    tracing::trace!("Executing command: {}", command_display());

    // One span per tool run, so `--profile` shows every subprocess.
    let tool = exe.file_stem().unwrap_or_default().to_string_lossy();
    let _span = tracing::info_span!("subprocess", tool = %tool).entered();
    count_work(&TOOLS_RUN);
    // Tools get an empty environment, so a build can't depend on the
    // user's (INCLUDE, LIB, CPATH, ...).
    let mut command = std::process::Command::new(exe);
    // argv[0] relative to the workspace root: lld-link writes it into PDBs
    // (#272). Windows has no separate argv[0].
    #[cfg(unix)]
    if let Ok(rel) = exe.strip_prefix(cwd) {
        std::os::unix::process::CommandExt::arg0(&mut command, rel);
    }
    let output = command
        .env_clear()
        .current_dir(cwd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("Failed to execute command: {}", command_display()))?;

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

/// llvm-ar reads response files with the host's rules unless told otherwise
/// (Windows rules on Windows, GNU elsewhere). Archive response files are
/// written with `write_response_file(.., false)`, so pin GNU quoting and the
/// same file means the same thing on every host.
pub const ARCHIVER_RSP_QUOTING: &str = "--rsp-quoting=posix";

/// Write `args` to a response file (`@path`), one per line, so long command
/// lines fit Windows' 32K limit. `windows_quoting` follows the tool's parser:
/// lld-link uses Windows rules (backslashes are literal), GNU tools treat a
/// backslash as an escape.
pub fn write_response_file(path: &camino::Utf8Path, args: &[String], windows_quoting: bool) -> anyhow::Result<()> {
    let mut text = String::new();
    for arg in args {
        let needs_quotes = arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || c == '"');
        let escaped = if windows_quoting { arg.replace('"', "\\\"") } else { arg.replace('\\', "\\\\").replace('"', "\\\"") };
        if needs_quotes {
            text.push('"');
            text.push_str(&escaped);
            if windows_quoting {
                // Backslashes before the closing quote would escape it.
                let trailing = escaped.len() - escaped.trim_end_matches('\\').len();
                text.push_str(&"\\".repeat(trailing));
            }
            text.push('"');
        } else {
            text.push_str(&escaped);
        }
        text.push('\n');
    }
    std::fs::write(path, text).with_context(|| format!("Failed to write response file [{path}]"))
}


#[cfg(test)]
mod tests {
    use super::write_response_file;

    #[test]
    fn response_file_quoting() {
        let dir = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::try_from(dir.path().join("a.rsp")).unwrap();
        let args = ["/OUT:x.exe", "C:\\My Dir\\", "a\"b", "c:\\lib"].map(String::from);

        write_response_file(&path, &args, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "/OUT:x.exe\n\"C:\\My Dir\\\\\"\n\"a\\\"b\"\nc:\\lib\n");

        write_response_file(&path, &args, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "/OUT:x.exe\n\"C:\\\\My Dir\\\\\"\n\"a\\\"b\"\nc:\\\\lib\n");
    }
}
