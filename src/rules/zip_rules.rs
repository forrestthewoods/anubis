//! `zip_archive`: package files into a `.zip`, e.g. for distribution.
//!
//! The rule needs no mode, so `anubis build -t //x:y` without `-m` builds it.
//! Archives are reproducible: entries are sorted and carry a fixed timestamp
//! and permissions, so the same inputs always give the same bytes.

use crate::anubis::{self, AnubisTarget, Rule, RuleTypeInfo, RuleTypename};
use crate::job_system::*;
use crate::rules::rule_utils::ensure_directory;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};
use anyhow::Context;
use camino::Utf8PathBuf;
use serde::Deserialize;
use std::io::Write;
use std::sync::Arc;

/// ```text
/// zip_archive(
///     name = "src",
///     srcs = glob(["*.c", "asm/*"]),
///     prefix = "nasm_c",   # optional directory inside the archive
/// )
/// ```
/// Entries are named by their path relative to the `ANUBIS` file's directory.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZipArchive {
    pub name: String,
    pub srcs: Vec<Utf8PathBuf>,
    #[serde(default)]
    pub prefix: String,

    #[serde(skip_deserializing)]
    target: AnubisTarget,
}

#[derive(Debug)]
pub struct ZipArchiveArtifact {
    pub output_file: Utf8PathBuf,
}
impl JobArtifact for ZipArchiveArtifact {}

impl Rule for ZipArchive {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn needs_mode(&self) -> bool {
        false
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let rule = arc_self
            .downcast_arc::<ZipArchive>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule to ZipArchive"))?;
        Ok(ctx.new_job(
            format!("Zip {}", self.target.target_path()),
            JobDisplayInfo {
                verb: "Zipping",
                short_name: self.name.clone(),
                detail: self.target.target_path().to_owned(),
            },
            Box::new(move |job| build_zip_archive(&rule, &job.ctx)),
        ))
    }

    fn preload(&self, _ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        Ok(vec![])
    }
}

impl crate::papyrus::PapyrusObjectType for ZipArchive {
    fn name() -> &'static str {
        "zip_archive"
    }
}

fn parse_zip_archive(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut rule = ZipArchive::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    rule.target = t;
    Ok(Arc::new(rule))
}

fn build_zip_archive(rule: &ZipArchive, ctx: &JobContext) -> anyhow::Result<JobOutcome> {
    let package_dir = ctx.anubis.root.join(rule.target.get_relative_dir());
    let prefix = rule.prefix.trim_matches('/');

    let mut entries = Vec::with_capacity(rule.srcs.len());
    for src in &rule.srcs {
        let rel = src
            .strip_prefix(&package_dir)
            .map_err(|_| anyhow_loc!("[{}] is not under [{}]", src, package_dir))?
            .as_str()
            .replace('\\', "/");
        let name = if prefix.is_empty() { rel } else { format!("{prefix}/{rel}") };
        entries.push((name, src));
    }
    entries.sort();
    if let Some(w) = entries.windows(2).find(|w| w[0].0 == w[1].0) {
        bail_loc!("[{}] lists [{}] more than once", rule.target.target_path(), w[0].0);
    }

    let out_dir = ctx.anubis.out_dir(&ctx.mode, &rule.target, "zip");
    ensure_directory(out_dir.as_ref())?;
    let output_file = out_dir.join(format!("{}.zip", rule.name));
    let temp_file = out_dir.join(format!("{}.zip.tmp", rule.name));

    let options = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&temp_file).with_context(|| format!("creating {temp_file}"))?);
    for (name, src) in &entries {
        bail_loc_if!(!src.is_file(), "[{}]: [{}] is not a file", rule.target.target_path(), src);
        let data = std::fs::read(src).with_context(|| format!("reading {src}"))?;
        zip.start_file(name.as_str(), options)?;
        zip.write_all(&data)?;
    }
    zip.finish()?;

    crate::build_cache::remove_outputs([output_file.as_path()])?;
    std::fs::rename(&temp_file, &output_file).with_context(|| format!("renaming {temp_file} to {output_file}"))?;
    if let Some(cache) = &ctx.anubis.build_cache {
        cache.invalidate(&output_file);
    }

    Ok(JobOutcome::Success(Arc::new(ZipArchiveArtifact { output_file })))
}

pub fn register_rule_typeinfos(anubis: &anubis::Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("zip_archive".to_owned()),
        parse_rule: parse_zip_archive,
    })
}
