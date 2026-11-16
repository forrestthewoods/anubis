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
/// or as is if it isn't under the root. Tools record some paths exactly as
/// given (CodeView's object name, NASM's source file symbol), so passing
/// these keeps the workspace's location out of their outputs (#272).
pub fn ws_relative<'a>(path: &'a Utf8Path, root: &Utf8Path) -> &'a Utf8Path {
    path.strip_prefix(root).unwrap_or(path)
}

/// `arg` with a leading workspace root taken off a path in it: a bare path
/// (`<root>/a.obj`) or an option's value after its first `:`
/// (`/LIBPATH:<root>/lib`). Anything else is returned unchanged. Matches
/// either slash, and ignores case on Windows.
pub fn ws_relative_arg(arg: &str, root: &Utf8Path) -> String {
    let prefix = format!("{}/", root.as_str().replace('\\', "/").trim_end_matches('/'));
    let strip = |s: &str| -> Option<String> {
        let head = s.get(..prefix.len())?.replace('\\', "/");
        let same = if cfg!(windows) { head.eq_ignore_ascii_case(&prefix) } else { head == prefix };
        same.then(|| s[prefix.len()..].to_owned())
    };
    if let Some(rest) = strip(arg) {
        return rest;
    }
    if arg.starts_with(['/', '-']) {
        if let Some((opt, value)) = arg.split_once(':') {
            if let Some(rest) = strip(value) {
                return format!("{opt}:{rest}");
            }
        }
    }
    arg.to_owned()
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

/// Count one action that makes new bytes inside Anubis instead of running a tool.
pub fn count_in_process_action() {
    IN_PROCESS_ACTIONS_RUN.fetch_add(1, Ordering::Relaxed);
}

/// Run `exe` with captured stdout/stderr. With `verbose_tools`, the output
/// is also logged at info level. Errors only if the process could not start.
/// Every tool runs in the workspace root (`cwd`), so a relative path in its
/// argv or output (e.g. a compile's `.d`) means the same file wherever
/// `anubis` was started; cache keys and the `.d` check assume that.
pub fn run_command_verbose(exe: &Path, args: &[String], verbose_tools: bool, cwd: &camino::Utf8Path) -> anyhow::Result<Output> {
    run_command(exe, None, args, verbose_tools, cwd)
}

/// [`run_command_verbose`], with the tool's `argv[0]` spelled relative to
/// `cwd` (the workspace root) when `exe` is under it. lld-link writes its
/// `argv[0]` into every PDB (#272). Unix only: Windows has no separate
/// `argv[0]`, so there the PDB still names the linker's absolute path.
pub fn run_command_relative_argv0(
    exe: &Path,
    args: &[String],
    verbose_tools: bool,
    cwd: &camino::Utf8Path,
) -> anyhow::Result<Output> {
    let arg0 = exe.strip_prefix(cwd).ok().and_then(|p| p.to_str()).filter(|p| !p.is_empty());
    run_command(exe, arg0, args, verbose_tools, cwd)
}

fn run_command(
    exe: &Path,
    #[cfg_attr(not(unix), allow(unused_variables))] arg0: Option<&str>,
    args: &[String],
    verbose_tools: bool,
    cwd: &camino::Utf8Path,
) -> anyhow::Result<Output> {
    let command_display = || format!("{} {}", exe.display(), args.join(" "));
    tracing::trace!("Executing command: {}", command_display());

    // One span per tool run, so `--profile` shows every subprocess.
    let tool = exe.file_stem().unwrap_or_default().to_string_lossy();
    let _span = tracing::info_span!("subprocess", tool = %tool).entered();
    TOOLS_RUN.fetch_add(1, Ordering::Relaxed);
    // Tools get an empty environment, so a build can't depend on the
    // user's (INCLUDE, LIB, CPATH, ...).
    let mut command = std::process::Command::new(exe);
    #[cfg(unix)]
    if let Some(arg0) = arg0 {
        std::os::unix::process::CommandExt::arg0(&mut command, arg0);
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
    use super::{write_response_file, ws_relative, ws_relative_arg};
    use camino::Utf8Path;

    #[test]
    fn workspace_relative_args() {
        let root = Utf8Path::new("/w/ws");
        assert_eq!(ws_relative_arg("/w/ws/a/b.obj", root), "a/b.obj");
        assert_eq!(ws_relative_arg("/LIBPATH:/w/ws/lib", root), "/LIBPATH:lib");
        assert_eq!(ws_relative_arg("/OUT:/w/ws/.anubis-out/x/main.exe", root), "/OUT:.anubis-out/x/main.exe");
        assert_eq!(ws_relative_arg("/vfsoverlay:/w/ws/o.yaml", root), "/vfsoverlay:o.yaml");
        // Not under the root, or the root only as a prefix of a name.
        assert_eq!(ws_relative_arg("/w/wsx/a.obj", root), "/w/wsx/a.obj");
        assert_eq!(ws_relative_arg("/LIBPATH:/elsewhere/lib", root), "/LIBPATH:/elsewhere/lib");
        assert_eq!(ws_relative_arg("/x/w/ws/a.obj", root), "/x/w/ws/a.obj");
        assert_eq!(ws_relative_arg("oldnames.lib", root), "oldnames.lib");
        assert_eq!(ws_relative_arg("/DEBUG", root), "/DEBUG");
        // Either slash; Windows roots.
        let win = Utf8Path::new("C:\\w\\ws");
        assert_eq!(ws_relative_arg("C:/w/ws/a.obj", win), "a.obj");
        assert_eq!(ws_relative_arg("C:\\w\\ws\\a.obj", win), "a.obj");
        assert_eq!(ws_relative_arg("/LIBPATH:C:/w/ws/lib", win), "/LIBPATH:lib");
        assert_eq!(ws_relative_arg("c:/w/ws/a.obj", win), if cfg!(windows) { "a.obj" } else { "c:/w/ws/a.obj" });

        assert_eq!(ws_relative(Utf8Path::new("/w/ws/.anubis-out/a.o"), root), ".anubis-out/a.o");
        assert_eq!(ws_relative(Utf8Path::new("/elsewhere/a.o"), root), "/elsewhere/a.o");
    }

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
