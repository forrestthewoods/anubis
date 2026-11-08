//! The build orchestrator.
//!
//! [`Anubis`] owns everything shared across a build: the parsed `ANUBIS`
//! files, resolved modes, toolchains and rules (each computed once, see
//! [`OnceCache`]), the build cache and the scheduling hints. [`AnubisTarget`]
//! is a `//dir:name` label. A [`Rule`] turns a target into jobs for the job
//! system; `build_targets` preloads impure rules, then runs every
//! (mode, target) pair through one job system.

use crate::job_system;
use crate::job_system::*;
use crate::papyrus;
use crate::papyrus::resolve_value_with_dir;
use crate::papyrus::*;
use crate::rules;
use crate::rules::*;
use crate::toolchain;
use crate::toolchain::Mode;
use crate::toolchain::Toolchain;
use crate::util;
use crate::util::SlashFix;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};
use crate::{anyhow_with_context, bail_with_context, timed_span};
use anyhow::Result;
use by_address::ByAddress;
use camino::{Utf8Path, Utf8PathBuf};
use dashmap::DashMap;
use downcast_rs::{impl_downcast, DowncastSync};
use heck::ToLowerCamelCase;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::any;
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

// ----------------------------------------------------------------------------
// Caches and small helpers
// ----------------------------------------------------------------------------
/// A cache whose values are computed once per key. The map's shard lock is
/// held only to find the key's cell; threads asking for the same key while it
/// is being computed wait on that cell instead of computing it again.
pub type OnceCache<K, V> = DashMap<K, Arc<OnceLock<V>>>;

fn get_or_compute<K, T>(cache: &OnceCache<K, ArcResult<T>>, key: K, compute: impl FnOnce() -> ArcResult<T>) -> ArcResult<T>
where
    K: Eq + std::hash::Hash,
    T: ?Sized,
{
    let cell = cache.entry(key).or_default().clone();
    cell.get_or_init(compute).clone()
}

pub type ArcResult<T> = anyhow::Result<Arc<T>>;

pub type TransitiveDeps = HashMap<ByAddress<Arc<Mode>>, HashSet<ByAddress<Arc<dyn Rule>>>>;

trait ResultExt<T> {
    fn clone(self) -> anyhow::Result<T>
    where
        T: Clone;
}

impl<T: Clone> ResultExt<T> for &anyhow::Result<T> {
    fn clone(self) -> anyhow::Result<T> {
        self.as_ref().map(|v| v.clone()).map_err(|e| anyhow_loc!("{}", e))
    }
}

trait Arcify<T> {
    fn arcify(self) -> ArcResult<T>;
}

impl<T> Arcify<T> for anyhow::Result<T> {
    fn arcify(self) -> ArcResult<T> {
        self.map(|v| Arc::new(v))
    }
}

fn arcify<T>(v: anyhow::Result<T>) -> anyhow::Result<Arc<T>> {
    v.map(|v| Arc::new(v))
}

// ----------------------------------------------------------------------------
// Root configuration
// ----------------------------------------------------------------------------

/// Project-level configuration read from `.anubis_root`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct AnubisConfig {
}

impl PapyrusObjectType for AnubisConfig {
    fn name() -> &'static str {
        "anubis_root"
    }
}

/// Load `AnubisConfig` from the `.anubis_root` file.
pub fn load_anubis_config(anubis_root_file: &Utf8Path) -> anyhow::Result<AnubisConfig> {
    let value = papyrus::read_papyrus_file(anubis_root_file.as_ref())?;
    value.deserialize_single_object::<AnubisConfig>()
}

pub fn find_anubis_root(start_dir: &Path) -> anyhow::Result<Utf8PathBuf> {
    let mut current_dir = start_dir.to_owned();

    loop {
        let candidate = current_dir.join(".anubis_root");
        if candidate.exists() && candidate.is_file() {
            return Utf8PathBuf::try_from(candidate)
                .map_err(|e| anyhow_loc!("Non-UTF-8 path to .anubis_root: {:?}", e));
        }

        if !current_dir.pop() {
            bail_loc!(
                "Failed to find .anubis_root in any parent directory starting from [{:?}]",
                current_dir
            )
        }
    }
}

/// `build_platform` and `build_arch` for the machine running the build, in
/// Anubis's naming (`windows`/`linux`/`macos`, `x64`/`arm64`).
fn build_platform_vars() -> anyhow::Result<HashMap<String, String>> {
    let build_arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => bail_loc!("Unsupported build architecture {}", other),
    };
    Ok(HashMap::from([
        ("build_platform".to_owned(), std::env::consts::OS.to_owned()),
        ("build_arch".to_owned(), build_arch.to_owned()),
    ]))
}

// ----------------------------------------------------------------------------
// Anubis, rules and cache keys
// ----------------------------------------------------------------------------
#[derive(Debug)]
pub struct Anubis {
    pub root: Utf8PathBuf,

    /// When true, external tools (e.g., clang) will be invoked with verbose flags (e.g., -v)
    pub verbose_tools: bool,

    // capability cache
    pub rule_typeinfos: DashMap<RuleTypename, RuleTypeInfo>,

    // environment caches
    pub dir_exists_cache: DashMap<Utf8PathBuf, bool>,

    // papyrus caches
    pub raw_config_cache: OnceCache<AnubisConfigRelPath, ArcResult<papyrus::Value>>,
    pub resolved_config_cache: OnceCache<ResolvedConfigCacheKey, ArcResult<papyrus::Value>>,
    
    // data caches
    pub mode_cache: OnceCache<AnubisTarget, ArcResult<Mode>>,
    pub toolchain_cache: OnceCache<ToolchainCacheKey, ArcResult<Toolchain>>,
    pub rule_cache: OnceCache<RuleCacheKey, ArcResult<dyn Rule>>,
    pub impure_transitive_deps_cache: DashMap<ByAddress<Arc<dyn Rule>>, TransitiveDeps>,

    // runtime caches    
    /// A [`OnceCache`], not a plain map: `build_rule` recurses (impure
    /// deps) while creating a job, and holding a shard lock across that
    /// deadlocked when two keys shared a shard.
    pub job_cache: OnceCache<JobCacheKey, anyhow::Result<JobId>>,

    /// Content-addressed action cache. None disables caching (`--no-cache`).
    pub build_cache: Option<crate::build_cache::BuildCache>,

    /// Expected compile durations from past builds, for scheduling.
    pub hints: crate::hints::Hints,

    /// Mode used to resolve configs when building without `-m`: only the
    /// `build_*` variables are defined.
    pub modeless: Arc<Mode>,
}

/// Toolchain used when none is given (the CLI and tool builds).
pub const DEFAULT_TOOLCHAIN: &str = "//toolchains:default";

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct AnubisTarget {
    full_path: String,    // ex: //path/to/foo:bar
    separator_idx: usize, // index of ':'
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AnubisConfigRelPath(String); // ex: //path/to/foo/ANUBIS

#[derive(Debug)]
pub struct RuleTypeInfo {
    pub name: RuleTypename,
    pub parse_rule: fn(AnubisTarget, &papyrus::Value) -> ArcResult<dyn Rule>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RuleTypename(pub String);

pub trait Rule: std::fmt::Debug + DowncastSync + Send + Sync + 'static {
    fn name(&self) -> String;
    fn target(&self) -> AnubisTarget;
    fn is_pure(&self) -> bool { true }
    /// Whether `anubis run` can execute this target's output.
    fn is_runnable(&self) -> bool { false }

    /// Whether this rule needs a mode. Rules that don't can be built without
    /// `-m`, and pick a mode for each dep themselves (`JobContext::with_mode`).
    fn needs_mode(&self) -> bool { true }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job>;
    fn preload(&self, ctx: Arc<JobContext>) -> anyhow::Result<Vec<JobId>>;
}
impl_downcast!(sync Rule);

#[derive(Debug)]
struct RulePreloadArtifact {
    mode: Option<Arc<Mode>>,
    rule: Arc<dyn Rule>,
}
impl JobArtifact for RulePreloadArtifact{}


#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolchainCacheKey {
    pub mode: AnubisTarget,
    pub toolchain: AnubisTarget,
}

/// Cache key for resolved config caching.
/// Configs are resolved with mode-specific variables (via select() statements),
/// so the same config file can produce different results for different modes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ResolvedConfigCacheKey {
    pub config_path: AnubisConfigRelPath,
    pub mode: AnubisTarget,
}

/// Cache key for rule caching.
/// Rules are deserialized from mode-resolved config (via select() statements),
/// so the same target can produce different rules for different modes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RuleCacheKey {
    pub target: AnubisTarget,
    pub mode: AnubisTarget,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct JobCacheKey {
    pub mode: Option<AnubisTarget>,
    pub target: AnubisTarget,
    pub action: String,
}

// ----------------------------------------------------------------------------
// impl Anubis
// ----------------------------------------------------------------------------
impl Anubis {
    pub fn new(root: Utf8PathBuf, verbose_tools: bool, config: &AnubisConfig) -> anyhow::Result<Anubis> {
        let mut anubis = Anubis {
            root,
            verbose_tools,
            rule_typeinfos: Default::default(),
            dir_exists_cache: Default::default(),
            raw_config_cache: Default::default(),
            resolved_config_cache: Default::default(),
            mode_cache: Default::default(),
            toolchain_cache: Default::default(),
            rule_cache: Default::default(),
            impure_transitive_deps_cache: Default::default(),
            job_cache: Default::default(),
            build_cache: None,
            hints: crate::hints::Hints::empty(),
            modeless: Arc::new(Mode {
                name: "modeless".into(),
                vars: build_platform_vars()?,
                target: AnubisTarget::default(),
            }),
        };

        tracing::debug!("Registering language rule type infos");
        rules::cc_rules::register_rule_typeinfos(&anubis)?;
        rules::cmd_rules::register_rule_typeinfos(&anubis)?;
        rules::ifs_rules::register_rule_typeinfos(&anubis)?;
        rules::nasm_rules::register_rule_typeinfos(&anubis)?;
        rules::zig_rules::register_rule_typeinfos(&anubis)?;
        rules::archive_rules::register_rule_typeinfos(&anubis)?;

        Ok(anubis)
    }

    /// Path: {root}/.anubis-out/$ModeName_HASH/label/$TargetPath/$TargetName
    pub fn out_dir(&self, mode: &Option<Arc<Mode>>, target: &AnubisTarget, label: &str) -> Utf8PathBuf {
        let mode_str = mode.as_ref().map_or("modeless".into(), |m| m.target.target_name_with_hash());
        self.root
            .join(".anubis-out")
            .join(mode_str)
            .join(target.get_relative_dir())
            .join(target.target_name())
            .join(label)
    }

    /// Returns the temp directory for temporary files during build.
    /// Path: {root}/.anubis-temp
    pub fn temp_dir(&self) -> Utf8PathBuf {
        self.root.join(".anubis-temp")
    }

    pub fn register_rule_typeinfo(&self, ti: RuleTypeInfo) -> anyhow::Result<()> {
        let rtis = &self.rule_typeinfos;

        if rtis.contains_key(&ti.name) {
            bail_loc!("Already contained entry for {}", &ti.name.0);
        }

        rtis.insert(ti.name.clone(), ti);
        Ok(())
    }

    pub fn get_mode(&self, mode_target: &AnubisTarget) -> anyhow::Result<Arc<Mode>> {
        get_or_compute(&self.mode_cache, mode_target.clone(), || {
            let config = self.get_raw_config(&mode_target.get_config_relpath())?;
            let mut mode = config.deserialize_named_object::<Mode>(mode_target.target_name())?;
            mode.target = mode_target.clone();
            mode.vars.extend(build_platform_vars()?);
            Ok(Arc::new(mode))
        })
    }

    /// The mode to resolve configs with: `mode`, or [`Anubis::modeless`].
    pub fn mode_or_modeless<'a>(&'a self, mode: &'a Option<Arc<Mode>>) -> &'a Mode {
        mode.as_deref().unwrap_or(&self.modeless)
    }

    pub fn get_toolchain(
        &self,
        mode: &Mode,
        toolchain_target: &AnubisTarget,
    ) -> anyhow::Result<Arc<Toolchain>> {
        let key = ToolchainCacheKey {
            mode: mode.target.clone(),
            toolchain: toolchain_target.clone(),
        };
        get_or_compute(&self.toolchain_cache, key, || {
            let config = self.get_resolved_config(&toolchain_target.get_config_relpath(), mode)?;
            let mut toolchain = config.deserialize_named_object::<Toolchain>(toolchain_target.target_name())?;
            toolchain.target = toolchain_target.clone();
            Ok(Arc::new(toolchain))
        })
    }

    fn get_raw_config(&self, config_path: &AnubisConfigRelPath) -> ArcResult<papyrus::Value> {
        get_or_compute(&self.raw_config_cache, config_path.clone(), || {
            let filepath = config_path.get_abspath(&self.root);
            papyrus::read_papyrus_file(filepath.as_ref()).map(Arc::new)
        })
        .map_err(|e| anyhow_loc!("Failed to parse [{}]: {}", config_path.0, e))
    }

    pub fn get_resolved_config(
        &self,
        config_relpath: &AnubisConfigRelPath,
        mode: &Mode,
    ) -> ArcResult<papyrus::Value> {
        // Create cache key that includes both config path and mode
        let cache_key = ResolvedConfigCacheKey {
            config_path: config_relpath.clone(),
            mode: mode.target.clone(),
        };

        get_or_compute(&self.resolved_config_cache, cache_key, || {
            let raw_config = self.get_raw_config(config_relpath)?;
            self.resolve_config_value((*raw_config).clone(), config_relpath, mode).map(Arc::new)
        })
    }

    /// Resolve `value`, part of the config at `config_relpath`, for `mode`.
    fn resolve_config_value(
        &self,
        value: papyrus::Value,
        config_relpath: &AnubisConfigRelPath,
        mode: &Mode,
    ) -> anyhow::Result<papyrus::Value> {
        let config_abspath = config_relpath.get_abspath(&self.root);
        let config_dir = config_abspath.parent().unwrap();
        resolve_value_with_dir(value, config_dir.as_std_path(), &mode.vars, Some(&config_relpath.get_dir_relpath()))
            .map_err(|e| anyhow_loc!("Error resolving config [{:?}]: {}", config_relpath.0, e))
    }

    pub fn get_rule(&self, rule: &AnubisTarget, mode: &Mode) -> ArcResult<dyn Rule> {
        let cache_key = RuleCacheKey {
            target: rule.clone(),
            mode: mode.target.clone(),
        };

        get_or_compute(&self.rule_cache, cache_key, || {
            // Resolve only this rule, so a select elsewhere in the file that
            // this mode can't evaluate (e.g. modeless) doesn't fail it.
            let config_relpath = rule.get_config_relpath();
            let raw_config = self.get_raw_config(&config_relpath)?;
            let raw_rule = raw_config.get_named_object(rule.target_name())?.clone();
            let resolved = self.resolve_config_value(raw_rule, &config_relpath, mode)?;
            let papyrus = &resolved;
            let rule_typename = match papyrus {
                Value::Object(obj) => RuleTypename(obj.typename.clone()),
                _ => bail_loc!("Rule [{}] ", rule),
            };

            let rti = self
                .rule_typeinfos
                .get(&rule_typename)
                .ok_or_else(|| anyhow_loc!("No rule typeinfo entry for [{}]", rule_typename.0))?;
            (rti.parse_rule)(rule.clone(), papyrus)
        })
    }

    // Either returns an existing job with `key` or creates a new Job for key
    // It is an unchecked error if an existing Job would not match the result of `make_job`
    pub fn ensure_job(&self, ctx: &Arc<JobContext>, key: JobCacheKey, make_job: Box<dyn FnOnce(&Arc<JobContext>) -> Job>) -> anyhow::Result<JobId> {
        let cell = self.job_cache.entry(key).or_default().clone();
        cell.get_or_init(|| {
            let job = make_job(&*ctx);
            ctx.job_system.add_job(job)
        }).clone()
    }

    pub fn preload_rule(&self, target: AnubisTarget, ctx: &Arc<JobContext>) -> anyhow::Result<JobId> {
        let key = JobCacheKey {
            mode: ctx.mode.as_ref().map(|m| m.target.clone()),
            target: target.clone(),
            action: "preload".to_string()
        };

        let make_job_fn = |ctx: &Arc<JobContext>| -> Job {
            let target_path = target.target_path().to_owned();
            ctx.new_job(
                format!("Loading rule [{}]", target_path),
                JobDisplayInfo { verb: "Load Rule", short_name: target_path.clone(), detail: target_path },
                Box::new(move |job| {
                    let anubis = &job.ctx.anubis;
                    let modeless = job.ctx.mode.is_none();
                    let rule = anubis.get_rule(&target, anubis.mode_or_modeless(&job.ctx.mode)).map_err(|e| {
                        if modeless { anyhow_loc!("{:#}\n(building without a mode; pass one with -m)", e) } else { e }
                    })?;
                    bail_loc_if!(
                        modeless && rule.needs_mode(),
                        "[{}] needs a mode. Pass one with -m, e.g. -m //mode:linux_dev",
                        target.target_path()
                    );

                    let dep_ids = rule.preload(job.ctx.clone())?;

                    if dep_ids.len() > 0 {
                        let dep_ids2 = dep_ids.clone();
                        let continuation_job : Job = job.ctx.new_job(
                            "Gather transitive impure deps".into(), 
                            JobDisplayInfo { 
                                verb: "Gather", 
                                short_name: target.target_name().to_owned(),
                                detail: target.target_path().to_owned(),
                            },
                            Box::new(move |job: Job| -> anyhow::Result<JobOutcome> {
                                let rule_key = ByAddress(rule.clone());
                                let mut impure_deps : TransitiveDeps = Default::default();

                                let impure_deps_cache = &job.ctx.anubis.impure_transitive_deps_cache;
                                for dep_id in &dep_ids2 {
                                    let dep_result = job.ctx.job_system.get_result(*dep_id)?;
                                    if let Ok(r) = dep_result.cast::<RulePreloadArtifact>() {
                                        let dep_rule_key = ByAddress(r.rule.clone());
                                        
                                        // if dep is impure, add it
                                        if !r.rule.is_pure() {
                                            let dep_mode = r.mode.as_ref().ok_or_else(|| anyhow_loc!("No mode for impure transitive dep [{}]", r.rule.target()))?;                                           
                                            let mode_key = ByAddress(dep_mode.clone());
                                            impure_deps
                                                .entry(mode_key)
                                                .or_default()
                                                .insert(dep_rule_key.clone());
                                        }

                                        // get all impure deps of dep, add them
                                        if let Some(c) = impure_deps_cache.get(&dep_rule_key) {
                                            for (k,v) in c.iter() {
                                                impure_deps.entry(k.clone()).or_default().extend(v.iter().cloned());
                                            }
                                        }
                                    }
                                }

                                if !impure_deps.is_empty() {
                                    job.ctx.anubis.impure_transitive_deps_cache.insert(rule_key, impure_deps);
                                }
                                
                                Ok(JobOutcome::Success(Arc::new(RulePreloadArtifact{
                                    mode: job.ctx.mode.clone(),
                                    rule,
                                })))
                            }));
                        
                        Ok(JobOutcome::Deferred(JobDeferral {
                            blocked_by: dep_ids,
                            continuation_job
                        }))

                    } else {
                        Ok(JobOutcome::Success(Arc::new(RulePreloadArtifact{
                            mode: job.ctx.mode.clone(),
                            rule,
                        })))
                    }
                })
            )
        };

        self.ensure_job(ctx, key, Box::new(make_job_fn))
    }

    /// Job for building `target` in `ctx`'s mode, created once per (mode, target)
    /// after the impure deps it needs.
    pub fn build_rule(&self, target: &AnubisTarget, ctx: &Arc<JobContext>) -> anyhow::Result<JobId> {
        let mode = self.mode_or_modeless(&ctx.mode);

        let cache_key = JobCacheKey {
            mode: ctx.mode.as_ref().map(|m| m.target.clone()),
            target: target.clone(),
            action: "build_rule".into(),
        };

        let cell = self.job_cache.entry(cache_key).or_default().clone();
        cell.get_or_init(|| {
            let rule = self.get_rule(target, mode)?;

            // determine if there are any impure deps that must be built first
            let impure_deps = ctx.anubis.impure_transitive_deps_cache
                .get(&ByAddress(rule.clone()))
                .map(|d| d.clone());

            let mut impure_dep_ids = Vec::<JobId>::default();
            if let Some(impure_deps) = impure_deps {
                for (mode, deps) in impure_deps {
                    let dep_ctx = Arc::new(ctx.with_mode((*mode).clone())?);

                    for dep in deps {
                        let dep_id = self.build_rule(&dep.target(), &dep_ctx)?;
                        impure_dep_ids.push(dep_id);
                    }
                }
            }

            let job = rule.build(rule.clone(), ctx.clone())?;
            ctx.job_system.add_job_with_deps(job, &impure_dep_ids)
        }).clone()
    }
    
    /// Fail early, listing every missing directory, instead of letting the
    /// compiler report one cryptically. `dir_type` names them in the error.
    pub fn verify_directories<'a>(
        &self,
        directories: impl IntoIterator<Item = &'a Utf8PathBuf>,
        dir_type: &str,
    ) -> anyhow::Result<()> {
        let mut missing_dirs = Vec::new();

        for dir in directories {
            if let Some(exists) = self.dir_exists_cache.get(dir) {
                if !*exists {
                    missing_dirs.push(dir.clone());
                }
                continue;
            }

            let exists = dir.is_dir();
            self.dir_exists_cache.insert(dir.clone(), exists);

            if !exists {
                missing_dirs.push(dir.clone());
            }
        }

        if !missing_dirs.is_empty() {
            bail_loc!(
                "{} directories do not exist:\n  {}",
                dir_type,
                missing_dirs
                    .iter()
                    .cloned()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join("\n  ")
            );
        }

        Ok(())
    }

    /// Expand `//dir/...` patterns into every target under `dir`; other
    /// strings are parsed as single targets.
    pub fn expand_targets(&self, targets: &[String]) -> anyhow::Result<Vec<AnubisTarget>> {
        let mut result = Vec::new();

        for target in targets {
            if let Some(pattern) = TargetPattern::parse(target) {
                let expanded = expand_target_pattern(self.root.as_std_path(), &pattern, &self.rule_typeinfos)?;
                tracing::debug!("Expanded pattern '{}' to {} targets", target, expanded.len());
                for t in expanded {
                    result.push(AnubisTarget::new(&t)?);
                }
            } else {
                result.push(AnubisTarget::new(target)?);
            }
        }

        Ok(result)
    }
}

// ----------------------------------------------------------------------------
// AnubisTarget
// ----------------------------------------------------------------------------
impl AnubisTarget {
    pub fn new(input: &str) -> anyhow::Result<AnubisTarget> {
        let parts: Vec<_> = input.split(":").collect();

        if parts.len() != 2 {
            bail_loc!(
                "Split on ':' had [{}] parts but must be 2. input: [{}]  parts: [{:?}]",
                parts.len(),
                input,
                parts
            );
        }

        if parts[0].is_empty() {
            // If first part is empty this is a rel-path
            Ok(AnubisTarget {
                full_path: input.to_owned(),
                separator_idx: 0,
            })
        } else {
            // This is repo relative
            if !parts[0].starts_with("//") {
                bail_loc!("Input string expected to start with '//'. input: [{}]", input);
            }

            if parts[1].contains("/") {
                bail_loc!("Invalid input. No slashes allowed after ':'. input: [{}]", input);
            }

            Ok(AnubisTarget {
                full_path: input.to_owned().replace("\\", "/"),
                separator_idx: parts[0].len(),
            })
        }
    }

    /// Returns true if this is a relative target (e.g., `:targetname`)
    pub fn is_relative(&self) -> bool {
        self.separator_idx == 0
    }

    /// `:avdevice` in `samples/external/ffmpeg` -> `//samples/external/ffmpeg:avdevice`.
    /// Absolute targets are returned unchanged.
    pub fn resolve(&self, dir_relpath: &str) -> AnubisTarget {
        if !self.is_relative() {
            return self.clone();
        }

        let target_name = self.target_name();
        let full_path = format!("//{dir_relpath}:{target_name}");
        let separator_idx = dir_relpath.len() + 2; // +2 for "//"

        AnubisTarget {
            full_path,
            separator_idx,
        }
    }

    pub fn target_path(&self) -> &str {
        &self.full_path
    }

    // given //path/to/foo:bar returns bar
    pub fn target_name(&self) -> &str {
        &self.full_path[self.separator_idx + 1..]
    }

    pub fn quick_short_hash(&self) -> u64 {
        util::quick_hash(&self) & 0xFFFFFFFF
    }

    pub fn target_name_with_hash(&self) -> String {
        format!("{}_{:x}", self.target_name(), self.quick_short_hash())
    }

    // given "//path/to/foo:bar"
    // return "path/to/foo"
    pub fn get_relative_dir(&self) -> &str {
        &self.full_path[2..self.separator_idx]
    }

    pub fn get_config_relpath(&self) -> AnubisConfigRelPath {
        // returns: //path/to/foo/ANUBIS
        // convert '\\' to '/' so paths are same on Linux/Windows
        let mut result = self.full_path[..self.separator_idx].to_owned();
        result.push_str(&"/ANUBIS");
        AnubisConfigRelPath(result)
    }

    pub fn get_config_abspath(&self, root: &Utf8Path) -> Utf8PathBuf {
        // returns: c:/stuff/project/path/to/foo/ANUBIS
        // convert '\\' to '/' so paths are same on Linux/Windows
        self.get_config_absdir(root)
            .join("ANUBIS")
            .slash_fix()
    }

    pub fn get_config_absdir(&self, root: &Utf8Path) -> Utf8PathBuf {
        // returns: c:/stuff/project/path/to/foo/
        // convert '\\' to '/' so paths are same on Linux/Windows
        root.join(&self.full_path[2..self.separator_idx])
            .slash_fix()
    }
}

impl std::fmt::Display for AnubisTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.full_path)
    }
}

impl AnubisConfigRelPath {
    pub fn get_abspath(&self, root: &Utf8Path) -> Utf8PathBuf {
        root.join(&self.0[2..]).slash_fix()
    }

    /// Returns the directory path relative to the repository root.
    /// For example, "//path/to/dir/ANUBIS" returns "path/to/dir"
    pub fn get_dir_relpath(&self) -> String {
        // self.0 is like "//path/to/dir/ANUBIS"
        // We want to return "path/to/dir"
        let without_prefix = &self.0[2..]; // "path/to/dir/ANUBIS"
                                           // Remove the trailing "/ANUBIS"
        without_prefix.strip_suffix("/ANUBIS").unwrap_or(without_prefix).to_owned()
    }
}

impl Serialize for AnubisTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.full_path)
    }
}

impl<'de> Deserialize<'de> for AnubisTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Use deserialize_newtype_struct with "AnubisTarget" to signal to the Papyrus
        // deserializer that we only accept Value::Target, not Value::String.
        // This prevents typos like `deps = ["//lib:foo"]` from being accepted -
        // use `deps = [Target("//lib:foo")]` instead.
        struct AnubisTargetVisitor;

        impl<'de> serde::de::Visitor<'de> for AnubisTargetVisitor {
            type Value = AnubisTarget;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a target path using Target(\"...\") syntax")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                AnubisTarget::new(value).map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_newtype_struct("AnubisTarget", AnubisTargetVisitor)
    }
}

// ----------------------------------------------------------------------------
// Target patterns
// ----------------------------------------------------------------------------
/// Represents a target pattern that can match multiple targets.
/// For example, "//samples/basic/..." matches all targets under the samples/basic directory.
#[derive(Clone, Debug)]
pub struct TargetPattern {
    /// The directory path relative to project root (e.g., "samples/basic" for "//samples/basic/...")
    pub dir_relpath: String,
}

impl TargetPattern {
    /// `//dir/...`, or `///...` for the whole repo (`//...` is not a pattern).
    pub fn is_pattern(s: &str) -> bool {
        s.len() >= 6 && s.starts_with("//") && s.ends_with("/...")
    }

    /// `//samples/basic/...` -> `samples/basic`; `///...` -> the root (`""`).
    pub fn parse(s: &str) -> Option<TargetPattern> {
        let without_prefix = s.strip_prefix("//")?;
        let without_suffix = without_prefix.strip_suffix("/...")?;

        Some(TargetPattern {
            dir_relpath: without_suffix.to_owned(),
        })
    }
}

/// Every target in every ANUBIS file under the pattern's directory.
pub fn expand_target_pattern(
    project_root: &Path,
    pattern: &TargetPattern,
    rule_typeinfos: &DashMap<RuleTypename, RuleTypeInfo>,
) -> anyhow::Result<Vec<String>> {
    let mut targets = Vec::new();

    let search_dir = if pattern.dir_relpath.is_empty() {
        project_root.to_path_buf()
    } else {
        project_root.join(&pattern.dir_relpath)
    };

    if !search_dir.exists() {
        bail_loc!("Directory does not exist for pattern: //{}", pattern.dir_relpath);
    }

    if !search_dir.is_dir() {
        bail_loc!("Path is not a directory for pattern: //{}", pattern.dir_relpath);
    }

    // Get the set of known rule type names
    let known_rules: std::collections::HashSet<String> = {
        rule_typeinfos.iter().map(|kv| kv.key().0.clone()).collect()
    };

    // Recursively find all ANUBIS files
    find_anubis_files_recursive(&search_dir, project_root, &known_rules, &mut targets)?;

    targets.sort();
    Ok(targets)
}

/// Targets defined in one directory's ANUBIS file (not its subdirectories).
pub fn targets_in_dir(
    project_root: &Path,
    dir_relpath: &str,
    rule_typeinfos: &DashMap<RuleTypename, RuleTypeInfo>,
) -> anyhow::Result<Vec<String>> {
    let anubis_file = project_root.join(dir_relpath).join("ANUBIS");
    if !anubis_file.is_file() {
        bail_loc!("No ANUBIS file at //{}", dir_relpath);
    }
    let known_rules: std::collections::HashSet<String> =
        rule_typeinfos.iter().map(|kv| kv.key().0.clone()).collect();
    let config = papyrus::read_papyrus_file(&anubis_file)?;
    let mut targets = Vec::new();
    extract_targets_from_config(&config, dir_relpath, &known_rules, &mut targets)?;
    Ok(targets)
}

/// Recursively find all ANUBIS files and extract targets from them.
fn find_anubis_files_recursive(
    dir: &Path,
    project_root: &Path,
    known_rules: &std::collections::HashSet<String>,
    targets: &mut Vec<String>,
) -> anyhow::Result<()> {
    let anubis_file = dir.join("ANUBIS");

    if anubis_file.exists() && anubis_file.is_file() {
        // Parse the ANUBIS file and extract targets
        let config = papyrus::read_papyrus_file(&anubis_file)?;

        // Calculate the directory relative path for target path construction
        let dir_relpath = dir
            .strip_prefix(project_root)
            .map_err(|e| anyhow_loc!("Failed to strip prefix: {}", e))?
            .to_string_lossy()
            .replace('\\', "/");

        // Extract target names from the config
        extract_targets_from_config(&config, &dir_relpath, known_rules, targets)?;
    }

    // Recurse into subdirectories
    let entries =
        std::fs::read_dir(dir).map_err(|e| anyhow_loc!("Failed to read directory {:?}: {}", dir, e))?;

    for entry in entries {
        let entry = entry.map_err(|e| anyhow_loc!("Failed to read directory entry: {}", e))?;
        let path = entry.path();

        if path.is_dir() {
            // Skip hidden directories and common non-source directories
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with('.') && name_str != "node_modules" && name_str != "target" {
                find_anubis_files_recursive(&path, project_root, known_rules, targets)?;
            }
        }
    }

    Ok(())
}

/// Extract target names from a parsed ANUBIS config.
fn extract_targets_from_config(
    config: &papyrus::Value,
    dir_relpath: &str,
    known_rules: &std::collections::HashSet<String>,
    targets: &mut Vec<String>,
) -> anyhow::Result<()> {
    // The config should be an array of rule definitions
    let array = match config {
        papyrus::Value::Array(arr) => arr,
        _ => return Ok(()), // Not an array, skip
    };

    for value in array {
        if let papyrus::Value::Object(obj) = value {
            // Check if this is a known rule type
            if known_rules.contains(&obj.typename) {
                // Extract the "name" field
                if let Some(papyrus::Value::String(name)) = obj.fields.get("name") {
                    let target_path = format!("//{}:{}", dir_relpath, name);
                    targets.push(target_path);
                }
            }
        }
    }

    Ok(())
}

// ----------------------------------------------------------------------------
// Build entry points
// ----------------------------------------------------------------------------
pub fn build_single_target(
    anubis: Arc<Anubis>,
    mode_target: &AnubisTarget,
    toolchain_path: &AnubisTarget,
    target_path: &AnubisTarget,
    num_workers: usize,
    progress_tx: crossbeam::channel::Sender<crate::progress::ProgressEvent>,
) -> anyhow::Result<Arc<dyn JobArtifact>> {
    let mut artifacts = build_targets(
        anubis,
        &[mode_target.clone()],
        toolchain_path,
        &[target_path.clone()],
        num_workers,
        progress_tx,
    )?;

    // Defensive check: ensure exactly one artifact was returned
    bail_loc_if!(
        artifacts.len() != 1,
        "Expected exactly 1 artifact for single target build, got {}",
        artifacts.len()
    );

    // Pop the single artifact
    artifacts.pop().ok_or_else(|| anyhow_loc!("No artifact returned for target"))
}

/// Build multiple targets across multiple modes using a shared JobSystem.
///
/// This is more efficient than calling `build_single_target` in a loop because:
/// 1. Dependencies are only built once (shared via job caches)
/// 2. Job IDs remain valid across all targets (same JobSystem instance)
///
/// The job caches (`job_cache`, `rule_job_cache`) map target+substep to JobIds,
/// and JobIds are only valid within a single JobSystem. By using one JobSystem
/// for all targets, we ensure cached JobIds remain valid.
///
/// Multiple modes produce an MxN matrix of builds (every target × every mode),
/// all fully parallel in a single shared JobSystem. The `JobCacheKey` already
/// includes `mode`, so different modes won't collide.
///
/// No modes builds every target once without a mode (see [`Rule::needs_mode`]).
///
/// Returns a Vec of artifacts in mode-major order: for each mode, the artifacts
/// for all targets in input order.
pub fn build_targets(
    anubis: Arc<Anubis>,
    mode_targets: &[AnubisTarget],
    toolchain_path: &AnubisTarget,
    target_paths: &[AnubisTarget],
    num_workers: usize,
    progress_tx: crossbeam::channel::Sender<crate::progress::ProgressEvent>,
) -> anyhow::Result<Vec<Arc<dyn JobArtifact>>> {
    if target_paths.is_empty() {
        return Ok(Vec::new());
    }

    // Resolve all modes and their toolchains up front. No modes means one
    // modeless build, which has no toolchain.
    let mut contexts: Vec<(String, Option<Arc<Mode>>, Option<Arc<Toolchain>>)> = Vec::new();
    for mode_target in mode_targets {
        tracing::debug!(mode_target = %mode_target.target_path(), "Loading build mode");
        let mode = anubis.get_mode(mode_target)?;

        tracing::debug!(
            toolchain_path = %toolchain_path.target_path(),
            mode = %mode.name,
            "Loading toolchain configuration"
        );
        let toolchain = anubis.get_toolchain(&mode, toolchain_path)?;
        contexts.push((mode_target.target_path().to_owned(), Some(mode), Some(toolchain)));
    }
    if contexts.is_empty() {
        contexts.push(("modeless".to_owned(), None, None));
    }

    // Create a SINGLE job system shared across ALL modes and targets
    let job_system: Arc<JobSystem> = Arc::new(JobSystem::new());

    // Initialize TUI progress
    let _ = progress_tx.send(crate::progress::ProgressEvent::SetJobCounter {
        counter: job_system.next_id.clone(),
    });

    let contexts: Vec<(String, Arc<JobContext>)> = contexts
        .into_iter()
        .map(|(label, mode, toolchain)| {
            let ctx = JobContext { anubis: anubis.clone(), job_system: job_system.clone(), mode, toolchain };
            (label, Arc::new(ctx))
        })
        .collect();

    // Preload phase: create preload jobs for all mode×target combos
    for (_, ctx) in &contexts {
        for target_path in target_paths.iter().cloned() {
            anubis.preload_rule(target_path, ctx)?;
        }
    }

    // Run all preload jobs at once
    JobSystem::run_to_completion(job_system.clone(), num_workers, progress_tx.clone())?;

    // Build phase: create build jobs for all mode×target combos
    let mut all_build_job_ids: Vec<Vec<JobId>> = Vec::new();
    for (_, ctx) in &contexts {
        let build_job_ids: Vec<JobId> = target_paths.iter()
            .map(|target_path| anubis.build_rule(target_path, ctx))
            .collect::<Result<Vec<_>, _>>()?;
        all_build_job_ids.push(build_job_ids);
    }

    // Run all build jobs at once
    JobSystem::run_to_completion(job_system.clone(), num_workers, progress_tx)?;

    // Log completion and collect artifacts (mode-major order)
    let mut artifacts = Vec::with_capacity(contexts.len() * target_paths.len());
    for ((label, _), build_job_ids) in contexts.iter().zip(&all_build_job_ids) {
        for (target_path, job_id) in target_paths.iter().zip(build_job_ids.iter()) {
            tracing::info!("Build complete [{} {}]", label, target_path.target_path());
            let artifact = job_system.get_result(*job_id)?;
            artifacts.push(artifact);
        }
    }

    Ok(artifacts)
}
