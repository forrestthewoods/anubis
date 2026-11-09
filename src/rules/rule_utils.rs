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
/// `cwd` is the working directory; compiles run in the workspace root, so a
/// relative path in their argv or `.d` means the same file wherever `anubis`
/// was started (the cache key and the `.d` check both assume that).
pub fn run_command_verbose(exe: &Path, args: &[String], verbose_tools: bool, cwd: Option<&Path>) -> anyhow::Result<Output> {
    let command_display = || format!("{} {}", exe.display(), args.join(" "));
    tracing::trace!("Executing command: {}", command_display());

    // One span per tool run, so `--profile` shows every subprocess.
    let tool = exe.file_stem().unwrap_or_default().to_string_lossy();
    let _span = tracing::info_span!("subprocess", tool = %tool).entered();
    let mut command = std::process::Command::new(exe);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
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
