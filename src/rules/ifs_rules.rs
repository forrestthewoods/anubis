//! `ifs_stub_library`: a link-only stand-in for a system shared library.
//!
//! Linux has no import libraries. To link against `libX11.so.6` the linker
//! only reads the library's SONAME and its dynamic symbol table, so a stub
//! `.so` that carries those (and no code) links exactly like the real one;
//! the real library is loaded at run time. LLVM's `llvm-ifs` builds such a
//! stub from a small text file (`.ifs`) that lists the SONAME and symbols:
//!
//! ```text
//! llvm-ifs --input-format=ELF --output-ifs=libX11.ifs /usr/lib/x86_64-linux-gnu/libX11.so.6
//! ```
//!
//! The `.ifs` is checked in; this rule turns it into the stub at build time
//! with the toolchain's own `llvm-ifs`, so it works the same on any host.
//!
//! ```text
//! ifs_stub_library(
//!     name = "x11",
//!     ifs = RelPath("libX11.ifs"),
//! )
//! ```
//!
//! Depend on it from a `cc_static_library` or `cc_binary` like any library.
//! `.ifs` files don't record glibc-style symbol versions, so this is for
//! unversioned libraries (X11, xcb, GL); glibc itself comes from Zig.

use crate::anubis::{self, AnubisTarget};
use crate::job_system::*;
use crate::rules::cc_rules::CcObjectArtifact;
use crate::rules::rule_utils::{ensure_directory, run_command_verbose};
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{anyhow_loc, bail_loc, function_name};
use anyhow::Context;
use camino::Utf8PathBuf;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IfsStubLibrary {
    pub name: String,
    /// The interface stub description (`llvm-ifs --output-ifs`).
    pub ifs: Utf8PathBuf,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

impl crate::papyrus::PapyrusObjectType for IfsStubLibrary {
    fn name() -> &'static str {
        "ifs_stub_library"
    }
}

impl anubis::Rule for IfsStubLibrary {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let rule = arc_self
            .clone()
            .downcast_arc::<IfsStubLibrary>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to IfsStubLibrary", arc_self))?;
        Ok(ctx.new_job(
            format!("Build IfsStubLibrary: {}", self.target),
            JobDisplayInfo {
                verb: "Stubbing",
                short_name: self.ifs.file_name().unwrap_or("ifs").to_owned(),
                detail: format!("IfsStubLibrary {}", self.target),
            },
            Box::new(move |job| build_ifs_stub(rule.clone(), job)),
        ))
    }

    fn preload(&self, _ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        Ok(vec![])
    }
}

fn parse_ifs_stub_library(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut rule = IfsStubLibrary::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    rule.target = t;
    Ok(Arc::new(rule))
}

/// `llvm-ifs` next to the toolchain's clang.
fn llvm_ifs_path(ctx: &JobContext) -> anyhow::Result<Utf8PathBuf> {
    let toolchain = ctx.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?;
    let compiler = &toolchain.cpp()?.compiler;
    let bin = compiler.parent().ok_or_else(|| anyhow_loc!("compiler path has no parent: {}", compiler))?;
    let exe = if cfg!(windows) { "llvm-ifs.exe" } else { "llvm-ifs" };
    let path = bin.join(exe);
    if !path.exists() {
        bail_loc!("llvm-ifs not found at {} (expected next to the C++ compiler)", path);
    }
    Ok(path)
}

fn build_ifs_stub(rule: Arc<IfsStubLibrary>, job: Job) -> anyhow::Result<JobOutcome> {
    let ctx = &job.ctx;
    if !rule.ifs.exists() {
        bail_loc!("ifs_stub_library {}: ifs file not found: {}", rule.target, rule.ifs);
    }
    let llvm_ifs = llvm_ifs_path(ctx)?;

    let out_dir = ctx.anubis.out_dir(&ctx.mode, &rule.target, "ifs_stub");
    ensure_directory(out_dir.as_ref())?;
    let stem = rule.ifs.file_stem().unwrap_or(&rule.name);
    let output = out_dir.join(format!("{stem}.so")).slash_fix();
    let tmp = out_dir.join(format!("{stem}.so.tmp")).slash_fix();

    let args = vec![format!("--output-elf={tmp}"), rule.ifs.clone().slash_fix().to_string()];
    let result = run_command_verbose(llvm_ifs.as_ref(), &args, ctx.anubis.verbose_tools)?;
    if !result.status.success() {
        bail_loc!(
            "llvm-ifs failed for {} [{}]\n  stdout: {}\n  stderr: {}",
            rule.ifs,
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    // Only replace the stub when it changed, so links that use it stay cached.
    let new_bytes = std::fs::read(&tmp).with_context(|| format!("reading {tmp}"))?;
    if std::fs::read(&output).ok().as_deref() != Some(new_bytes.as_slice()) {
        std::fs::rename(&tmp, &output).with_context(|| format!("writing {output}"))?;
        if let Some(cache) = ctx.anubis.build_cache.as_ref() {
            cache.invalidate(&output);
        }
    } else {
        let _ = std::fs::remove_file(&tmp);
    }

    Ok(JobOutcome::Success(Arc::new(CcObjectArtifact { object_path: output })))
}

pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("ifs_stub_library".to_owned()),
        parse_rule: parse_ifs_stub_library,
    })?;
    Ok(())
}
