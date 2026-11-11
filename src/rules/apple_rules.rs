//! `macos_app`: an application bundle around a `cc_binary` (macOS modes only).
//!
//! ```text
//! macos_app(
//!     name = "Hello",
//!     binary = Target(":main"),
//!     bundle_id = "com.example.hello",
//!     resources = glob([ "resources/**/*" ]),
//! )
//! ```
//!
//! writes `<name>.app` under the target's output dir:
//!
//! ```text
//! Hello.app/Contents/Info.plist        generated from the fields below
//! Hello.app/Contents/PkgInfo
//! Hello.app/Contents/MacOS/main        the executable, plus the dylibs it
//! Hello.app/Contents/MacOS/lib*.dylib  loads (@rpath = @loader_path)
//! Hello.app/Contents/Resources/...     `resources`, named relative to the ANUBIS file
//! ```
//!
//! Files are hard links to the build outputs (copies if that fails), so
//! nothing here runs a tool or needs caching. The executable keeps the ad-hoc
//! signature `ld64.lld` gives every arm64 binary, which is enough to run the
//! app on the machine that has it. Distributing it needs a Developer ID
//! signature and notarization, which this rule doesn't do yet.

use crate::anubis::{self, AnubisTarget};
use crate::job_system::*;
use crate::rules::cc_rules::CompileExeArtifact;
use crate::rules::rule_utils::ensure_directory;
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};
use anyhow::Context;
use camino::Utf8PathBuf;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacosApp {
    /// Bundle name: `<name>.app`, and `CFBundleName` unless `display_name` is set.
    pub name: String,
    /// The `cc_binary` to wrap.
    pub binary: AnubisTarget,
    /// `CFBundleIdentifier`, e.g. `com.example.hello`.
    pub bundle_id: String,
    /// `CFBundleDisplayName`. Defaults to `name`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// `CFBundleShortVersionString` and `CFBundleVersion`.
    #[serde(default = "default_version")]
    pub version: String,
    /// `LSMinimumSystemVersion`. Should match the toolchain's deployment target.
    #[serde(default = "default_minimum_os")]
    pub minimum_os: String,
    /// Files copied into `Contents/Resources`, keeping their path relative to
    /// the ANUBIS file's directory.
    #[serde(default)]
    pub resources: Vec<Utf8PathBuf>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

fn default_version() -> String {
    "1.0".to_owned()
}

fn default_minimum_os() -> String {
    "13.3".to_owned()
}

/// What a `macos_app` produces.
#[derive(Debug)]
pub struct MacosAppArtifact {
    pub bundle: Utf8PathBuf,
    pub executable: Utf8PathBuf,
}

impl JobArtifact for MacosAppArtifact {}

impl crate::papyrus::PapyrusObjectType for MacosApp {
    fn name() -> &'static str {
        "macos_app"
    }
}

impl anubis::Rule for MacosApp {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        bail_loc_if!(ctx.mode.is_none(), "macos_app {} needs a mode", self.target);
        let rule = arc_self
            .clone()
            .downcast_arc::<MacosApp>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to MacosApp", arc_self))?;
        Ok(ctx.new_job(
            format!("Build MacosApp: {}", self.target),
            JobDisplayInfo {
                verb: "Bundling",
                short_name: format!("{}.app", self.name),
                detail: format!("MacosApp {}", self.target),
            },
            Box::new(move |job| build_macos_app(rule.clone(), job)),
        ))
    }

    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>> {
        Ok(vec![ctx.anubis.preload_rule(self.binary.clone(), &ctx)?])
    }
}

fn parse_macos_app(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut rule = MacosApp::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    rule.target = t;
    Ok(Arc::new(rule))
}

fn build_macos_app(rule: Arc<MacosApp>, mut job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job.ctx.mode.as_ref().ok_or_else(|| anyhow_loc!("macos_app needs a mode"))?;
    if mode.vars.get("target_platform").map(|s| s.as_str()) != Some("macos") {
        bail_loc!("macos_app {} only builds in macOS modes, not {}", rule.target, mode.name);
    }
    let bin_job = job.ctx.anubis.build_rule(&rule.binary, &job.ctx)?;
    let rule2 = rule.clone();
    job.job_fn = Some(Box::new(move |job: Job| bundle(rule2, bin_job, job)));
    Ok(JobOutcome::Deferred(JobDeferral { blocked_by: vec![bin_job], continuation_job: job }))
}

fn bundle(rule: Arc<MacosApp>, bin_job: JobId, job: Job) -> anyhow::Result<JobOutcome> {
    let ctx = &job.ctx;
    let result = ctx.job_system.get_result(bin_job)?;
    let exe = result
        .downcast_arc::<CompileExeArtifact>()
        .map_err(|_| anyhow_loc!("macos_app {}: binary {} is not a cc_binary", rule.target, rule.binary))?;
    let exe_name = crate::file_name!(exe.output_file)?.to_owned();

    let app = ctx.anubis.out_dir(&ctx.mode, &rule.target, "app").join(format!("{}.app", rule.name)).slash_fix();
    let contents = app.join("Contents");
    let macos_dir = contents.join("MacOS");
    let resources_dir = contents.join("Resources");
    ensure_directory(macos_dir.as_ref())?;

    // Rebuilt from scratch each time, so removed resources and dylibs don't
    // linger. Only links and two small files, so it's cheap.
    // (source, destination, may hard-link)
    let mut wanted: Vec<(Utf8PathBuf, Utf8PathBuf, bool)> = Vec::new();
    wanted.push((exe.output_file.clone(), macos_dir.join(&exe_name), true));
    for f in &exe.runtime_files {
        wanted.push((f.clone(), macos_dir.join(crate::file_name!(f)?), true));
    }
    let package_dir = rule.target.get_config_absdir(&ctx.anubis.root);
    for r in &rule.resources {
        let rel = r.strip_prefix(&package_dir).map_err(|_| {
            anyhow_loc!("macos_app {}: resource {} is outside {}", rule.target, r, package_dir)
        })?;
        if !r.is_file() {
            bail_loc!("macos_app {}: resource {} is not a file", rule.target, r);
        }
        // Copied: a hard link would let an edit inside the bundle change
        // the source file.
        wanted.push((r.clone(), resources_dir.join(rel), false));
    }

    // Its binaries are read-only hard links to cache blobs; deleting them
    // is fine (see `anubis clean`).
    if app.exists() {
        std::fs::remove_dir_all(&app).with_context(|| format!("removing {app}"))?;
    }
    ensure_directory(macos_dir.as_ref())?;
    let mut written: Vec<Utf8PathBuf> = vec![contents.join("Info.plist"), contents.join("PkgInfo")];
    for (src, dst, link) in &wanted {
        ensure_directory(crate::parent_dir!(dst)?.as_ref())?;
        if !*link || std::fs::hard_link(src, dst).is_err() {
            std::fs::copy(src, dst).with_context(|| format!("copying {src} to {dst}"))?;
        }
        written.push(dst.clone());
    }
    std::fs::write(&written[0], info_plist(&rule, &exe_name))?;
    std::fs::write(&written[1], "APPL????")?;
    if let Some(cache) = &ctx.anubis.build_cache {
        for f in &written {
            cache.invalidate(f);
        }
    }

    Ok(JobOutcome::Success(Arc::new(MacosAppArtifact { executable: macos_dir.join(&exe_name), bundle: app })))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn info_plist(rule: &MacosApp, exe_name: &str) -> String {
    let display = rule.display_name.as_deref().unwrap_or(&rule.name);
    let entries = [
        ("CFBundleDevelopmentRegion", "en"),
        ("CFBundleDisplayName", display),
        ("CFBundleExecutable", exe_name),
        ("CFBundleIdentifier", &rule.bundle_id),
        ("CFBundleInfoDictionaryVersion", "6.0"),
        ("CFBundleName", &rule.name),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleShortVersionString", &rule.version),
        ("CFBundleVersion", &rule.version),
        ("LSMinimumSystemVersion", &rule.minimum_os),
    ];
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for (k, v) in entries {
        s += &format!("\t<key>{k}</key>\n\t<string>{}</string>\n", xml_escape(v));
    }
    s += "\t<key>NSHighResolutionCapable</key>\n\t<true/>\n</dict>\n</plist>\n";
    s
}

pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("macos_app".to_owned()),
        parse_rule: parse_macos_app,
    })?;
    Ok(())
}
