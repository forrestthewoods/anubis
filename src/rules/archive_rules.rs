//! `archive_zip` and `archive_7z`: package files into an archive, e.g. for
//! distribution. One rule per format, since each format has its own settings.
//!
//! The rules need no mode, so `anubis build -t //x:y` without `-m` builds them.
//! Archives are reproducible: entries are sorted and carry no timestamps that
//! vary (zip: a fixed timestamp and mode 0644; 7z: none), so the same inputs
//! always give the same bytes.

use crate::anubis::{self, AnubisTarget, Rule, RuleTypeInfo, RuleTypename};
use crate::build_cache::{ActionOutput, KeyBuilder};
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
        // Sources are read one after another into one solid block, so opening each lazily keeps one open
        // at a time instead of one per entry (#199: EMFILE past the open-file limit).
        let files = entries.iter().map(|(_, src)| SourceReader::new(LazyFile::new(src))).collect();
        // `ArchiveEntry::new_file` sets no dates or attributes, unlike `from_path`.
        let archive_entries = entries.iter().map(|(name, _)| ArchiveEntry::new_file(name)).collect();
        writer.push_archive_entries(archive_entries, files)?;
        writer.finish()?;
        Ok(())
    }
}

/// A source file that opens on its first read and closes at EOF.
struct LazyFile<'a> {
    path: &'a Utf8Path,
    state: LazyFileState,
}

enum LazyFileState {
    Unopened,
    Open(std::fs::File),
    Done,
}

impl<'a> LazyFile<'a> {
    fn new(path: &'a Utf8Path) -> Self {
        Self {
            path,
            state: LazyFileState::Unopened,
        }
    }
}

impl std::io::Read for LazyFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let LazyFileState::Unopened = self.state {
            let file = std::fs::File::open(self.path)
                .map_err(|e| std::io::Error::new(e.kind(), format!("opening {}: {e}", self.path)))?;
            #[cfg(test)]
            tests::OPEN_LAZY_FILES.with(|n| {
                n.set(n.get() + 1);
                tests::MAX_OPEN_LAZY_FILES.with(|m| m.set(m.get().max(n.get())));
            });
            self.state = LazyFileState::Open(file);
        }
        let LazyFileState::Open(file) = &mut self.state else {
            return Ok(0);
        };
        let n = file
            .read(buf)
            .map_err(|e| std::io::Error::new(e.kind(), format!("reading {}: {e}", self.path)))?;
        if n == 0 && !buf.is_empty() {
            self.state = LazyFileState::Done;
            #[cfg(test)]
            tests::OPEN_LAZY_FILES.with(|n| n.set(n.get() - 1));
        }
        Ok(n)
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

    let cache = ctx.anubis.build_cache.as_ref();
    let started = std::time::SystemTime::now();
    let srcs: Vec<Utf8PathBuf> = entries.iter().map(|(_, src)| (*src).clone()).collect();
    let mut key = None;
    if let Some(cache) = cache {
        // Bump the version whenever the bytes written for the same inputs change: a change to `write`,
        // or an update to a crate that writes them (zip, flate2, miniz_oxide, sevenz-rust2, lzma-rust2).
        // `archive_bytes_are_pinned` fails when that happens.
        let mut kb = KeyBuilder::new("archive-v1");
        // The output's name: the cache restores the file under the name it was stored with.
        kb.field(output_file.file_name().unwrap_or_default());
        kb.field(rule.level().map_or("-".to_owned(), |l| l.to_string()));
        for (name, _) in &entries {
            kb.field(name);
        }
        // Digest the sources now, not from earlier in the run, so an edit before `started` is in the key.
        for src in &srcs {
            cache.invalidate(src);
        }
        bail_loc_if!(!cache.key_add_files(&mut kb, &srcs)?, "[{}]: a source disappeared", target_path);
        let k = kb.finish();
        if cache.restore(&k, &out_dir)? {
            cache.record_hit();
            return Ok(JobOutcome::Success(Arc::new(ArchiveArtifact { output_file })));
        }
        cache.record_miss();
        key = Some(k);
    }

    crate::rules::rule_utils::count_in_process_action();
    let out = std::fs::File::create(&temp_file).with_context(|| format!("creating {temp_file}"))?;
    if let Err(e) = rule.write(&entries, out) {
        let _ = std::fs::remove_file(&temp_file);
        return Err(e.context(format!("writing {temp_file}")));
    }

    crate::build_cache::remove_outputs([output_file.as_path()])?;
    std::fs::rename(&temp_file, &output_file)
        .with_context(|| format!("renaming {temp_file} to {output_file}"))?;
    if let (Some(cache), Some(key)) = (cache, key) {
        // A source edited since its digest was taken may be in the archive: write it, but don't store it
        // under a key for other bytes (#207).
        if crate::build_cache::any_modified_since(&srcs, started) {
            tracing::debug!("not caching {output_file}: a source changed while it was written");
            cache.invalidate(&output_file);
            return Ok(JobOutcome::Success(Arc::new(ArchiveArtifact { output_file })));
        }
        cache.store(&key, &out_dir, &[ActionOutput { path: output_file.clone(), optional: false }])?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// `LazyFile`s open on this thread now, and the most at once.
        pub(super) static OPEN_LAZY_FILES: Cell<usize> = const { Cell::new(0) };
        pub(super) static MAX_OPEN_LAZY_FILES: Cell<usize> = const { Cell::new(0) };
    }

    fn files(dir: &Utf8Path, count: usize) -> Vec<Utf8PathBuf> {
        (0..count)
            .map(|i| {
                let path = dir.join(format!("f{i:04}.txt"));
                std::fs::write(&path, format!("file {i}")).unwrap();
                path
            })
            .collect()
    }

    /// #199: 7z used to open every source before writing, failing past the open-file limit
    /// (1024 on Linux, 256 on macOS).
    #[test]
    fn archive_7z_opens_one_source_at_a_time() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let srcs = files(dir, 2000);
        let entries: Vec<_> = srcs.iter().map(|p| (p.file_name().unwrap().to_owned(), p)).collect();
        let rule = Archive7z {
            name: "m".into(),
            srcs: vec![],
            prefix: String::new(),
            level: Some(0),
            target: AnubisTarget::default(),
        };
        let out = dir.join("m.7z");

        MAX_OPEN_LAZY_FILES.with(|m| m.set(0));
        rule.write(&entries, std::fs::File::create(&out).unwrap()).unwrap();
        assert_eq!(MAX_OPEN_LAZY_FILES.with(Cell::get), 1);
        assert_eq!(OPEN_LAZY_FILES.with(Cell::get), 0);

        let mut count = 0;
        let mut reader = sevenz_rust2::ArchiveReader::new(
            std::fs::File::open(&out).unwrap(),
            sevenz_rust2::Password::empty(),
        )
        .unwrap();
        reader
            .for_each_entries(|entry, data| {
                let mut text = String::new();
                data.read_to_string(&mut text)?;
                assert_eq!(entry.name(), format!("f{count:04}.txt"));
                assert_eq!(text, format!("file {count}"));
                count += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(count, 2000);
    }

    /// Fails while writing, after the temp file exists.
    #[derive(Debug, Deserialize)]
    struct ArchiveFails {
        name: String,
        srcs: Vec<Utf8PathBuf>,
        prefix: String,
        level: Option<u32>,
        #[serde(skip_deserializing)]
        target: AnubisTarget,
    }

    impl ArchiveFormat for ArchiveFails {
        const EXTENSION: &'static str = "fails";
        const VERB: &'static str = "Failing";
        fn srcs(&self) -> &[Utf8PathBuf] {
            &self.srcs
        }
        fn prefix(&self) -> &str {
            &self.prefix
        }
        fn level(&self) -> Option<u32> {
            self.level
        }
        fn write(&self, _: &[(String, &Utf8PathBuf)], mut out: std::fs::File) -> anyhow::Result<()> {
            out.write_all(b"partial")?;
            anyhow::bail!("disk full")
        }
    }

    #[allow(dead_code)]
    mod rule {
        use super::*;
        archive_rule!(ArchiveFails, "archive_fails", parse_archive_fails);
    }

    /// #244: a second build with the same inputs is a cache hit and leaves the archive alone; a changed
    /// source or level, or another target with the same inputs, is a miss. A result whose sources changed
    /// since the build started is not stored (#207).
    #[test]
    fn archive_is_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        std::fs::write(root.join(".anubis_root"), "anubis_root()\n").unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();
        let srcs = files(&root.join("pkg"), 3);
        let cache_dir = root.join(".anubis-cache");

        // A new `Anubis` per build, like separate `anubis build` runs: the digest memo is per run.
        let build = |target: &str, level: Option<u32>| {
            let target = AnubisTarget::new(target).unwrap();
            let mut anubis = anubis::Anubis::new(root.clone(), false, &anubis::AnubisConfig {}).unwrap();
            let backend = crate::build_cache::LocalDiskBackend::new(cache_dir.clone()).unwrap();
            anubis.build_cache = Some(crate::build_cache::BuildCache::new(&root, Box::new(backend)));
            let ctx = JobContext {
                anubis: Arc::new(anubis),
                job_system: Arc::new(JobSystem::new()),
                mode: None,
                toolchain: None,
            };
            let rule = ArchiveZip {
                name: "m".into(),
                srcs: srcs.clone(),
                prefix: String::new(),
                level,
                target: target.clone(),
            };
            let JobOutcome::Success(artifact) = build_archive(&rule, &target, &ctx).unwrap() else {
                panic!("expected success");
            };
            let output = artifact.downcast_arc::<ArchiveArtifact>().unwrap().output_file.clone();
            let cache = ctx.anubis.build_cache.as_ref().unwrap();
            (output, cache.hits(), cache.misses())
        };
        let state = |p: &Utf8Path| {
            let m = std::fs::metadata(p).unwrap();
            (m.len(), m.modified().unwrap())
        };

        // The sources were just written, so they may have changed while the archive was: not stored.
        let (zip, hits, misses) = build("//pkg:m", None);
        assert_eq!((hits, misses), (0, 1));
        std::thread::sleep(std::time::Duration::from_millis(50));
        let (_, hits, misses) = build("//pkg:m", None);
        assert_eq!((hits, misses), (0, 1), "a result built from fresh sources must not be stored");
        let first = state(&zip);
        std::thread::sleep(std::time::Duration::from_millis(20));

        let (_, hits, misses) = build("//pkg:m", None);
        assert_eq!((hits, misses), (1, 0));
        assert_eq!(state(&zip), first, "a hit must not rewrite the archive");

        let (_, hits, misses) = build("//pkg:m", Some(0));
        assert_eq!((hits, misses), (0, 1), "a new level is a miss");

        let (other, hits, misses) = build("//pkg:m2", Some(0));
        assert_eq!((hits, misses), (0, 1), "another target's archive has another name");
        assert!(other.ends_with("m2.zip") && other.is_file(), "{other}");

        std::fs::write(&srcs[1], "changed").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let (_, hits, misses) = build("//pkg:m", Some(0));
        assert_eq!((hits, misses), (0, 1), "a changed source is a miss");
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut archive.by_name("f0001.txt").unwrap(), &mut text).unwrap();
        assert_eq!(text, "changed");
    }

    /// The cache key's `archive-vN` stands for the writers' output. If this fails, the bytes for the same
    /// inputs changed (a change to `write` or a crate update): bump the version in `build_archive`, then
    /// update the digests here.
    #[test]
    fn archive_bytes_are_pinned() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let srcs = files(dir, 3);
        let entries: Vec<_> = srcs.iter().map(|p| (format!("p/{}", p.file_name().unwrap()), p)).collect();
        let digest = |rule: &dyn Fn(std::fs::File) -> anyhow::Result<()>, name: &str| {
            let out = dir.join(name);
            rule(std::fs::File::create(&out).unwrap()).unwrap();
            crate::build_cache::Digest::of_file(&out).unwrap().hex()
        };
        let zip = ArchiveZip {
            name: "m".into(),
            srcs: vec![],
            prefix: String::new(),
            level: Some(9),
            target: AnubisTarget::default(),
        };
        let sevenz = Archive7z {
            name: "m".into(),
            srcs: vec![],
            prefix: String::new(),
            level: Some(9),
            target: AnubisTarget::default(),
        };
        assert_eq!(
            digest(&|f| zip.write(&entries, f), "m.zip"),
            "85b63dc890bc91102a719617587acdf9a48a276cebe2a3872c6ea788db62e4c4"
        );
        assert_eq!(
            digest(&|f| sevenz.write(&entries, f), "m.7z"),
            "98f3cc3623f1030d6427b88537707e820ebd45eca0561a3b257fac6ff40b5f5a"
        );
    }

    #[test]
    fn failed_write_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        std::fs::write(root.join(".anubis_root"), "anubis_root()\n").unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();
        let srcs = files(&root.join("pkg"), 1);

        let anubis = Arc::new(anubis::Anubis::new(root.clone(), false, &anubis::AnubisConfig {}).unwrap());
        let ctx = JobContext {
            anubis: anubis.clone(),
            job_system: Arc::new(JobSystem::new()),
            mode: None,
            toolchain: None,
        };
        let target = AnubisTarget::new("//pkg:m").unwrap();
        let rule = ArchiveFails {
            name: "m".into(),
            srcs,
            prefix: String::new(),
            level: None,
            target: target.clone(),
        };

        let err = build_archive(&rule, &target, &ctx).err().expect("write should fail");
        assert!(format!("{err:#}").contains("disk full"), "{err:#}");
        let out_dir = anubis.out_dir(&None, &target, "fails");
        let left: Vec<_> =
            std::fs::read_dir(out_dir.as_std_path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }
}
