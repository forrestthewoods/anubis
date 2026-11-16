//! Zig-related build rules for extracting libc and runtime libraries.
//!
//! This module provides rules for extracting Zig's bundled libc and runtime
//! libraries for cross-compilation scenarios.

use crate::anubis::{self, AnubisTarget, JobCacheKey};
use crate::cc_rules;
use crate::job_system::*;
use crate::rules::rule_utils::{ensure_directory, run_command_verbose};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{anyhow_loc, bail_loc, function_name};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZigGlibc {
    pub name: String,
    pub target_triple: String,
    pub glibc_version: String,
    pub expected_link_args: Vec<String>,
    pub lang: cc_rules::CcLanguage,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

impl ZigGlibc {
    /// Short id of everything Zig's output depends on: the rule's resolved
    /// fields and the Zig binary, but not the mode. Names the shared job and
    /// its output dir, so modes that resolve the rule differently don't share.
    fn inputs_id(&self, zig: &camino::Utf8Path) -> String {
        let mut h = xxhash_rust::xxh3::Xxh3::new();
        for part in [zig.as_str(), &self.target_triple, &self.glibc_version, &format!("{:?}", self.lang)] {
            h.update(part.as_bytes());
            h.update(&[0]);
        }
        for a in &self.expected_link_args {
            h.update(a.as_bytes());
            h.update(&[0]);
        }
        format!("{:016x}", h.digest())
    }
}

// ----------------------------------------------------------------------------
// Trait Implementations
// ----------------------------------------------------------------------------
impl crate::papyrus::PapyrusObjectType for ZigGlibc {
    fn name() -> &'static str {
        "zig_glibc"
    }
}

impl anubis::Rule for ZigGlibc {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let zig_libc = arc_self
            .clone()
            .downcast_arc::<ZigGlibc>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to ZigGlibc", arc_self))?;

        // What Zig produces depends on the triple, glibc version, language,
        // link-arg filter and Zig binary, not on the mode. So every mode whose
        // resolved rule has the same inputs waits on one shared job, and the
        // libc is extracted once per build instead of once per Linux mode.
        // Modes that resolve different inputs get separate jobs and dirs.
        let zig = ctx
            .toolchain
            .as_ref()
            .ok_or_else(|| anyhow_loc!("No toolchain specified"))?
            .zig()?
            .compiler
            .clone();
        // Workspace-relative, so the id (and the cache key that holds the
        // output dir) is the same on every machine.
        let inputs_id = zig_libc.inputs_id(zig.strip_prefix(&ctx.anubis.root).unwrap_or(&zig));
        let key = JobCacheKey {
            mode: None,
            target: self.target.clone(),
            action: format!("zig_glibc {inputs_id}"),
        };
        let short = format!("ZigGlibc {} {:?}", self.target_triple, self.lang);
        let detail = format!("ZigGlibc {}", self.target);
        let display = JobDisplayInfo { verb: "Extracting", short_name: short, detail };
        let shared_display = display.clone();
        let shared_id = ctx.anubis.ensure_job(
            &ctx,
            key,
            Box::new(move |ctx: &Arc<JobContext>| {
                ctx.new_job(
                    format!("Build ZigGlibc: {}", zig_libc.target),
                    shared_display,
                    Box::new(move |job| build_zig_glibc(zig_libc.clone(), &inputs_id, job)),
                )
            }),
        )?;

        // This mode's job just forwards the shared job's result.
        let target = self.target.clone();
        Ok(ctx.new_job(
            format!("Wait for ZigGlibc: {}", self.target),
            JobDisplayInfo { verb: "Gather", ..display },
            Box::new(move |job| {
                let forward_display = JobDisplayInfo {
                    verb: "Gather",
                    short_name: target.target_name().to_owned(),
                    detail: target.target_path().to_owned(),
                };
                let forward = job.ctx.new_job(
                    format!("{} (result)", job.desc),
                    forward_display,
                    Box::new(move |job| Ok(JobOutcome::Success(job.ctx.job_system.get_result(shared_id)?))),
                );
                Ok(JobOutcome::Deferred(JobDeferral { blocked_by: vec![shared_id], continuation_job: forward }))
            }),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        Ok(vec![])
    }
}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------
fn parse_zig_glibc(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut zig_glibc = ZigGlibc::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    zig_glibc.target = t;
    Ok(Arc::new(zig_glibc))
}

/// The libc files a link needs. Objects (`crt1.o`, which holds `_start`) are
/// executable-only startup code; the libraries go into every linked output.
fn glibc_output(link_args: Vec<Utf8PathBuf>) -> JobOutcome {
    let (startup_objects, libraries) = link_args.into_iter().partition(|p| p.extension() == Some("o"));
    JobOutcome::Success(Arc::new(cc_rules::CcBuildOutput {
        transitive_libraries: libraries,
        startup_objects,
        ..Default::default()
    }))
}

fn build_zig_glibc(zig_glibc: Arc<ZigGlibc>, inputs_id: &str, job: Job) -> anyhow::Result<JobOutcome> {
    let toolchain = job.ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref();

    let build_dir = job
        .ctx
        .anubis
        .out_dir(&None, &zig_glibc.target, "zig_glibc")
        .join(inputs_id)
        .join(&zig_glibc.target_triple)
        .join(format!("{:?}", zig_glibc.lang));
    ensure_directory(build_dir.as_ref())?;

    let temp_dir = job
        .ctx
        .anubis
        .out_dir(&None, &zig_glibc.target, "zig_glibc")
        .join(inputs_id)
        .join(&zig_glibc.target_triple)
        .join("dummy")
        .join(format!("{:?}", zig_glibc.lang));
    ensure_directory(temp_dir.as_ref())?;

    // ex: x86_64-linux-gnu.2.28
    let full_target_triple = format!("{}.{}", &zig_glibc.target_triple, &zig_glibc.glibc_version);

    let (src_file, zig_cmd) = match zig_glibc.lang {
        cc_rules::CcLanguage::C => {
            let src_path = temp_dir.join("dummy.c");
            write_if_changed(&src_path, "int main() { return 0; }\n")?;
            (src_path, "cc".to_owned())
        }
        cc_rules::CcLanguage::Cpp => {
            let src_path = temp_dir.join("dummy.cpp");
            write_if_changed(&src_path, "int main() { return 0; }\n")?;
            (src_path, "c++".to_owned())
        }
    };

    let is_c = match zig_glibc.lang {
        cc_rules::CcLanguage::C => true,
        cc_rules::CcLanguage::Cpp => false
    };

    let dummy_bin_name = "dummy_exe";
    let zig_cache_dir = build_dir.join("zig");
    let mut args: Vec<String> = vec![
        "build-exe".into(),
        "--global-cache-dir".into(),
        zig_cache_dir.to_string(),
        "-target".into(),
        full_target_triple,
        "--verbose-link".into(),
    ];

    if is_c {
        args.push("-lc".into());
    } else {
        args.push("-cflags".into());
        args.push("-std=c++20".into());
        args.push("--".into());
        args.push("-lc++".into());
    }

    args.push(format!("-femit-bin={}", temp_dir.join(dummy_bin_name)));
    args.push(src_file.to_string());

    // Cache: zig + args + the link-arg filter. Zig's compiler_rt archives
    // are not byte-reproducible, so without this every downstream Linux
    // link would miss whenever .anubis-out is wiped.
    let zig_root = job.ctx.anubis.out_dir(&None, &zig_glibc.target, "zig_glibc").join(inputs_id);
    let zig = toolchain.zig()?.compiler.as_path();
    let cache = job.ctx.anubis.build_cache.as_ref();
    let cache_key = match cache {
        Some(cache) => {
            let mut kb = cache.key_for_command("zig_glibc", zig, &args, Some(&zig_root))?;
            // The output dir stays in the key: zig embeds it in the stub
            // .so files' debug info. It no longer depends on the mode.
            kb.field(cache.normalize_path(&zig_root));
            for a in &zig_glibc.expected_link_args {
                kb.field(a);
            }
            Some(kb.finish())
        }
        None => None,
    };
    if let (Some(cache), Some(key)) = (cache, cache_key.as_ref()) {
        if let Some(link_args) = cache.restore_outputs(key, &zig_root)? {
            cache.record_hit();
            return Ok(glibc_output(link_args));
        }
        cache.record_miss();
    }

    let verbose = job.ctx.anubis.verbose_tools;
    let output = run_command_verbose(toolchain.zig()?.compiler.as_ref(), &args, verbose, &job.ctx.anubis.root)?;

    if output.status.success() {
        // zig emits all logs to stderr
        let stderr = String::from_utf8_lossy(&output.stderr);

        let linker_cmd = stderr.lines().rev().find(|l| l.contains(dummy_bin_name)).ok_or_else(|| {
            anyhow_loc!(
                "Failed to find binname [{}] in in zig linker output: [{}]",
                dummy_bin_name,
                stderr
            )
        })?;
        tracing::debug!("Zig Glibc LinkerCmd: [{linker_cmd}");

        let link_args: Vec<Utf8PathBuf> =
            cache_paths_in_link_line(linker_cmd, &zig_cache_dir, &job.ctx.anubis.root)
                .into_iter()
                .filter(|p| zig_glibc.expected_link_args.iter().any(|arg| p.as_str().rfind(arg.as_str()).is_some()))
                .collect();
        tracing::debug!("Zig Glibc LinkArgs: {:?}", link_args);
        // A misread link line must fail here, not as a confusing link error later.
        if link_args.is_empty() {
            bail_loc!("Found none of {:?} under [{zig_cache_dir}] in Zig's link line: [{linker_cmd}]", zig_glibc.expected_link_args);
        }
        if let Some(missing) = link_args.iter().find(|p| !p.exists()) {
            bail_loc!("Zig's link line names [{missing}], which doesn't exist. Link line: [{linker_cmd}]");
        }

        if let (Some(cache), Some(key)) = (cache, cache_key.as_ref()) {
            if link_args.iter().all(|p| p.starts_with(&zig_root)) {
                let outputs: Vec<_> = link_args
                    .iter()
                    .map(|p| crate::build_cache::ActionOutput { path: p.clone(), optional: false })
                    .collect();
                cache.store(key, &zig_root, &outputs)?;
            } else {
                tracing::debug!("not caching zig_glibc: link args outside {zig_root}");
            }
        }

        Ok(glibc_output(link_args))
    } else {
        tracing::error!(
            exit_code = output.status.code(),
            stdout = %String::from_utf8_lossy(&output.stdout),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "Compilation failed"
        );

        bail_loc!(
            "Command completed with error status [{}].\n  Args: {}\n  stdout: {}\n  stderr: {}",
            output.status,
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

// ----------------------------------------------------------------------------
// Public Functions
// ----------------------------------------------------------------------------

/// Registers zig rule types with Anubis.
pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("zig_glibc".to_owned()),
        parse_rule: parse_zig_glibc,
    })?;

    Ok(())
}

/// The files under `cache_dir` named on Zig's `--verbose-link` line.
///
/// Zig prints the linker's argv joined with spaces and no quoting, so the line
/// can't be split on whitespace once the workspace path has a space in it
/// (#196). Every file we want is in Zig's global cache, which we chose: each
/// one is the cache dir (spaces and all) followed by Zig's own names inside
/// it (hashes and library names, never a space). So a path starts wherever
/// the cache dir starts an argument and runs to the next whitespace. The cache
/// dir is matched absolute or relative to the workspace root (Zig's working
/// directory), with either slash.
fn cache_paths_in_link_line(line: &str, cache_dir: &Utf8Path, root: &Utf8Path) -> Vec<Utf8PathBuf> {
    // Both are ASCII-for-ASCII rewrites, so byte offsets carry over to `line`.
    let norm = |s: &str| s.replace('\\', "/");
    let norm_line = norm(line);
    // (prefix, whether a match is relative to the workspace root)
    let mut prefixes = vec![(norm(cache_dir.as_str()), false)];
    if let Ok(rel) = cache_dir.strip_prefix(root) {
        prefixes.push((norm(rel.as_str()), true));
    }

    let mut found: Vec<(usize, Utf8PathBuf)> = Vec::new();
    for (prefix, relative) in &prefixes {
        let prefix = prefix.trim_end_matches('/');
        for (start, _) in norm_line.match_indices(prefix) {
            let at_arg_start = start == 0 || norm_line[..start].ends_with(|c: char| c.is_whitespace() || c == '=');
            let continues_dir = norm_line[start + prefix.len()..].starts_with('/');
            if !at_arg_start || !continues_dir || found.iter().any(|(s, _)| *s == start) {
                continue;
            }
            let rest = &line[start + prefix.len()..];
            let end = start + prefix.len() + rest.find(char::is_whitespace).unwrap_or(rest.len());
            let p = Utf8PathBuf::from(&line[start..end]);
            found.push((start, if *relative { root.join(p) } else { p }));
        }
    }
    found.sort_by_key(|(start, _)| *start);
    found.into_iter().map(|(_, p)| p).collect()
}

/// Write `text` to `path` unless it already holds it, so a build with
/// nothing to do writes nothing.
fn write_if_changed(path: &camino::Utf8Path, text: &str) -> anyhow::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    std::fs::write(path, text).with_context(|| format!("Failed to write dummy source: {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::cache_paths_in_link_line;
    use camino::{Utf8Path, Utf8PathBuf};

    #[test]
    fn link_line_paths_with_spaces() {
        let root = Utf8Path::new("/home/John Smith/my ws");
        let cache = root.join(".anubis-out/modeless/sp ace_libc_1234/zig_glibc/abcd/x86_64-linux-gnu/Cpp/zig");
        let line = format!(
            "ld.lld --error-limit=0 -o {root}/dummy/dummy_exe {cache}/o/aa/crt1.o -L {root}/zig/lib \
             {cache}/o/bb/libc++.a .anubis-out/modeless/sp ace_libc_1234/zig_glibc/abcd/x86_64-linux-gnu/Cpp/zig/o/cc/libc.so \
             --dynamic-linker={cache}/o/dd/ld.so -lm"
        );
        let got = cache_paths_in_link_line(&line, &cache, root);
        let want: Vec<Utf8PathBuf> = ["o/aa/crt1.o", "o/bb/libc++.a", "o/cc/libc.so", "o/dd/ld.so"]
            .iter()
            .map(|f| cache.join(f))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn link_line_windows_separators() {
        let root = Utf8Path::new("C:/Users/John Smith/ws");
        let cache = root.join(".anubis-out/modeless/libc_1234/zig_glibc/abcd/zig");
        let line = "ld.lld C:\\Users\\John Smith\\ws\\.anubis-out\\modeless\\libc_1234\\zig_glibc\\abcd\\zig\\o\\aa\\libc.so -o x";
        let got = cache_paths_in_link_line(line, &cache, root);
        assert_eq!(got, vec![Utf8PathBuf::from(
            "C:\\Users\\John Smith\\ws\\.anubis-out\\modeless\\libc_1234\\zig_glibc\\abcd\\zig\\o\\aa\\libc.so"
        )]);
    }
}
