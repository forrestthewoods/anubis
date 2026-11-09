//! `anubis-cmake`: experimental. Writes a CMakeLists.txt that builds Anubis
//! C/C++ targets with the consumer's own compiler. See `export.rs`.

mod export;

use anubis::anubis::{find_anubis_root, load_anubis_config, Anubis};
use anubis::{anyhow_loc, bail_loc};
use camino::Utf8PathBuf;
use clap::Parser;

/// Write a CMakeLists.txt that builds Anubis C/C++ targets with your own compiler.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Modes to merge, e.g. one per platform and build type.
    #[arg(short, long, visible_alias = "mode", required = true, num_args = 1..)]
    modes: Vec<String>,

    /// Targets to export (their deps come along).
    #[arg(short, long, visible_alias = "target", required = true, num_args = 1..)]
    targets: Vec<String>,

    /// File to write, inside the project.
    #[arg(short, long, default_value = "CMakeLists.txt")]
    output: Utf8PathBuf,

    /// Prefix for CMake target names (`<ns>_<name>`, alias `<ns>::<name>`).
    #[arg(long, required = true)]
    namespace: String,

    /// Targets the consumer's system provides (e.g. vendored system
    /// headers): exported as empty INTERFACE libraries.
    #[arg(long, num_args = 1..)]
    system: Vec<String>,

    /// Change nothing; fail if the file is missing or out of date.
    #[arg(long)]
    check: bool,
}

fn main() {
    if let Err(e) = run(Args::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
        .map_err(|p| anyhow_loc!("non-UTF-8 cwd [{}]", p.display()))?;
    let root_file = find_anubis_root(cwd.as_std_path())?;
    let root = root_file.parent().ok_or_else(|| anyhow_loc!("no parent for [{}]", root_file))?.to_owned();
    let anubis = Anubis::new(root.clone(), false, &load_anubis_config(&root_file)?)?;

    let output = cwd.join(&args.output);
    let namespace = args.namespace.clone();

    // A command line that works from the project root.
    let rel_out = output.strip_prefix(&root).map(|p| p.to_string()).unwrap_or_else(|_| output.to_string());
    let mut command_line = format!(
        "anubis-cmake --namespace {namespace} -m {} -t {} -o {}",
        args.modes.join(" "),
        args.targets.join(" "),
        rel_out
    );
    if !args.system.is_empty() {
        command_line += &format!(" --system {}", args.system.join(" "));
    }

    let text = export::export_cmake(
        &anubis,
        &export::ExportOptions {
            modes: args.modes.clone(),
            targets: args.targets.clone(),
            system_targets: args.system.clone(),
            output: output.clone(),
            namespace,
            command_line,
        },
    )?;

    let current = std::fs::read_to_string(&output).ok();
    if args.check {
        if current.as_deref() != Some(text.as_str()) {
            bail_loc!("{} is out of date; rerun the command in its header", output);
        }
        return Ok(());
    }
    if current.as_deref() != Some(text.as_str()) {
        if let Some(dir) = output.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&output, text)?;
    }
    println!("wrote {output}");
    Ok(())
}
