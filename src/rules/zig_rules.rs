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
use camino::Utf8PathBuf;
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
            std::fs::write(&src_path, "int main() { return 0; }\n")
                .with_context(|| format!("Failed to write dummy source: {:?}", src_path))?;
            (src_path, "cc".to_owned())
        }
        cc_rules::CcLanguage::Cpp => {
            let src_path = temp_dir.join("dummy.cpp");
            std::fs::write(&src_path, "int main() { return 0; }\n")
                .with_context(|| format!("Failed to write dummy source: {:?}", src_path))?;
            (src_path, "c++".to_owned())
        }
    };

    let is_c = match zig_glibc.lang {
        cc_rules::CcLanguage::C => true,
        cc_rules::CcLanguage::Cpp => false
    };

    let dummy_bin_name = "dummy_exe";
    let mut args: Vec<String> = vec![
        "build-exe".into(),
        "--global-cache-dir".into(),
        build_dir.join("zig").to_string(),
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
    let output = run_command_verbose(toolchain.zig()?.compiler.as_ref(), &args, verbose)?;

    if output.status.success() {
        // zig emits all logs to stderr
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lines = stderr.lines().rev();

        let linker_cmd = stderr.lines().rev().find(|l| l.contains(dummy_bin_name)).ok_or_else(|| {
            anyhow_loc!(
                "Failed to find binname [{}] in in zig linker output: [{}]",
                dummy_bin_name,
                stderr
            )
        })?;
        tracing::debug!("Zig Glibc LinkerCmd: [{linker_cmd}");

        let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow_loc!("non-UTF-8 working directory {:?}", p))?;
        let linker_parts: Vec<&str> = linker_cmd.split_ascii_whitespace().collect();
        let link_args: Vec<Utf8PathBuf> = linker_parts
            .iter()
            .filter(|part| zig_glibc.expected_link_args.iter().any(|arg| part.rfind(arg).is_some()))
            .map(|part| {
                // zig prints paths relative to its working directory (ours)
                let p = Utf8PathBuf::from(*part);
                if p.is_relative() { cwd.join(p) } else { p }
            })
            .collect();
        tracing::debug!("Zig Glibc LinkArgs: {:?}", link_args);

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
