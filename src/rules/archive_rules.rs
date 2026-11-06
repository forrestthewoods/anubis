//! `archive_zip` and `archive_7z`: package files into an archive, e.g. for
//! distribution. One rule per format, since each format has its own settings.
//!
//! The rules need no mode, so `anubis build -t //x:y` without `-m` builds them.
//! Archives are reproducible: entries are sorted and carry no timestamps that
//! vary (zip: a fixed timestamp and mode 0644; 7z: none), so the same inputs
//! always give the same bytes.

use crate::anubis::{self, AnubisTarget, Rule, RuleTypeInfo, RuleTypename};
use crate::job_system::*;
use crate::rules::rule_utils::ensure_directory;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::io::Write;
use std::sync::Arc;

/// ```text
/// archive_zip(
///     name = "src",
///     srcs = glob(["*.c", "asm/*"]),
///     prefix = "nasm_c",   # optional directory inside the archive
///     level = 9,           # optional deflate level, 0-9 (default 6)
/// )
/// ```
/// Entries are named by their path relative to the `ANUBIS` file's directory.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveZip {
    pub name: String,
    pub srcs: Vec<Utf8PathBuf>,
    #[serde(default)]
    pub prefix: String,
    pub level: Option<u32>,

    #[serde(skip_deserializing)]
    target: AnubisTarget,
}

/// ```text
/// archive_7z(
///     name = "src",
///     srcs = glob(["*.c", "asm/*"]),
///     prefix = "nasm_c",   # optional directory inside the archive
///     level = 9,           # optional LZMA2 level, 0-9 (default 6)
/// )
/// ```
/// One solid LZMA2 block holds every file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive7z {
    pub name: String,
    pub srcs: Vec<Utf8PathBuf>,
    #[serde(default)]
    pub prefix: String,
    pub level: Option<u32>,

    #[serde(skip_deserializing)]
    target: AnubisTarget,
}

/// The archive file an `archive_*` rule wrote.
#[derive(Debug)]
pub struct ArchiveArtifact {
    pub output_file: Utf8PathBuf,
}
impl JobArtifact for ArchiveArtifact {}

/// What differs between the formats: the file extension and how to write the entries.
trait ArchiveFormat: Rule + Sized + 'static {
    const EXTENSION: &'static str;
    const VERB: &'static str;
    fn srcs(&self) -> &[Utf8PathBuf];
    fn prefix(&self) -> &str;
    fn level(&self) -> Option<u32>;
    /// Writes `entries` (archive name, source file), already sorted, to `out`.
    fn write(&self, entries: &[(String, &Utf8PathBuf)], out: std::fs::File) -> anyhow::Result<()>;
}

impl ArchiveFormat for ArchiveZip {
    const EXTENSION: &'static str = "zip";
    const VERB: &'static str = "Zipping";
    fn srcs(&self) -> &[Utf8PathBuf] {
        &self.srcs
    }
    fn prefix(&self) -> &str {
        &self.prefix
    }
    fn level(&self) -> Option<u32> {
        self.level
    }

    fn write(&self, entries: &[(String, &Utf8PathBuf)], out: std::fs::File) -> anyhow::Result<()> {
        let options = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(self.level.map(|l| l as i32))
            .last_modified_time(zip::DateTime::default())
            .unix_permissions(0o644);
        let mut zip = zip::ZipWriter::new(out);
        for (name, src) in entries {
            let data = std::fs::read(src).with_context(|| format!("reading {src}"))?;
            zip.start_file(name.as_str(), options)?;
            zip.write_all(&data)?;
        }
        zip.finish()?;
        Ok(())
    }
}

impl ArchiveFormat for Archive7z {
    const EXTENSION: &'static str = "7z";
    const VERB: &'static str = "Archiving";
    fn srcs(&self) -> &[Utf8PathBuf] {
        &self.srcs
    }
    fn prefix(&self) -> &str {
        &self.prefix
    }
    fn level(&self) -> Option<u32> {
        self.level
    }

    fn write(&self, entries: &[(String, &Utf8PathBuf)], out: std::fs::File) -> anyhow::Result<()> {
        use sevenz_rust2::{encoder_options::Lzma2Options, ArchiveEntry, ArchiveWriter, SourceReader};
        let mut writer = ArchiveWriter::new(out)?;
        writer.set_content_methods(vec![Lzma2Options::from_level(self.level.unwrap_or(6)).into()]);
        let mut files = Vec::with_capacity(entries.len());
        for (_, src) in entries {
            files.push(SourceReader::new(
                std::fs::File::open(src).with_context(|| format!("opening {src}"))?,
            ));
        }
        // `ArchiveEntry::new_file` sets no dates or attributes, unlike `from_path`.
        let archive_entries = entries.iter().map(|(name, _)| ArchiveEntry::new_file(name)).collect();
        writer.push_archive_entries(archive_entries, files)?;
        writer.finish()?;
        Ok(())
    }
}

macro_rules! archive_rule {
    ($ty:ty, $typename:literal, $parse:ident) => {
        impl Rule for $ty {
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
                    .downcast_arc::<$ty>()
                    .map_err(|_| anyhow_loc!("Failed to downcast rule to {}", stringify!($ty)))?;
                Ok(ctx.new_job(
                    format!("{} {}", $typename, self.target.target_path()),
                    JobDisplayInfo {
                        verb: <$ty as ArchiveFormat>::VERB,
                        short_name: self.name.clone(),
                        detail: self.target.target_path().to_owned(),
                    },
                    Box::new(move |job| build_archive(&*rule, &rule.target, &job.ctx)),
                ))
            }

            fn preload(&self, _ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
                Ok(vec![])
            }
        }

        impl crate::papyrus::PapyrusObjectType for $ty {
            fn name() -> &'static str {
                $typename
            }
        }

        fn $parse(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
            let de = crate::papyrus_serde::ValueDeserializer::new(v);
            let mut rule = <$ty>::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
            rule.target = t;
            Ok(Arc::new(rule))
        }
    };
}

archive_rule!(ArchiveZip, "archive_zip", parse_archive_zip);
archive_rule!(Archive7z, "archive_7z", parse_archive_7z);

fn build_archive<F: ArchiveFormat>(
    rule: &F,
    target: &AnubisTarget,
    ctx: &JobContext,
) -> anyhow::Result<JobOutcome> {
    let target_path = target.target_path();
    if let Some(level) = rule.level() {
        bail_loc_if!(level > 9, "[{}]: level must be 0-9, got {}", target_path, level);
    }
    let entries = archive_entries(
        rule.srcs(),
        rule.prefix(),
        &ctx.anubis.root.join(target.get_relative_dir()),
        target_path,
    )?;

    let out_dir = ctx.anubis.out_dir(&ctx.mode, target, F::EXTENSION);
    ensure_directory(out_dir.as_ref())?;
    let output_file = out_dir.join(format!("{}.{}", target.target_name(), F::EXTENSION));
    let temp_file = out_dir.join(format!("{}.{}.tmp", target.target_name(), F::EXTENSION));

    let out = std::fs::File::create(&temp_file).with_context(|| format!("creating {temp_file}"))?;
    rule.write(&entries, out).with_context(|| format!("writing {temp_file}"))?;

    crate::build_cache::remove_outputs([output_file.as_path()])?;
    std::fs::rename(&temp_file, &output_file)
        .with_context(|| format!("renaming {temp_file} to {output_file}"))?;
    if let Some(cache) = &ctx.anubis.build_cache {
        cache.invalidate(&output_file);
    }

    Ok(JobOutcome::Success(Arc::new(ArchiveArtifact { output_file })))
}

/// Names each source by its path relative to `package_dir`, under `prefix`, sorted.
fn archive_entries<'a>(
    srcs: &'a [Utf8PathBuf],
    prefix: &str,
    package_dir: &Utf8Path,
    target_path: &str,
) -> anyhow::Result<Vec<(String, &'a Utf8PathBuf)>> {
    let prefix = prefix.trim_matches('/');
    let mut entries = Vec::with_capacity(srcs.len());
    for src in srcs {
        bail_loc_if!(!src.is_file(), "[{}]: [{}] is not a file", target_path, src);
        let rel = src
            .strip_prefix(package_dir)
            .map_err(|_| anyhow_loc!("[{}] is not under [{}]", src, package_dir))?
            .as_str()
            .replace('\\', "/");
        let name = if prefix.is_empty() {
            rel
        } else {
            format!("{prefix}/{rel}")
        };
        entries.push((name, src));
    }
    entries.sort();
    if let Some(w) = entries.windows(2).find(|w| w[0].0 == w[1].0) {
        bail_loc!("[{}] lists [{}] more than once", target_path, w[0].0);
    }
    Ok(entries)
}

pub fn register_rule_typeinfos(anubis: &anubis::Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("archive_zip".to_owned()),
        parse_rule: parse_archive_zip,
    })?;
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("archive_7z".to_owned()),
        parse_rule: parse_archive_7z,
    })
}
