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
use camino::{Utf8Path, Utf8PathBuf};
use dashmap::DashMap;
use downcast_rs::{impl_downcast, DowncastSync};
use heck::ToLowerCamelCase;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::any;
use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;

// utility for an Arc<Mutex<HashMap<K,V>>>
pub type SharedHashMap<K, V> = Arc<RwLock<HashMap<K, V>>>;

// utility for an Result<Arc<T>>
pub type ArcResult<T> = anyhow::Result<Arc<T>>;

// ----------------------------------------------------------------------------
// declarations
// ----------------------------------------------------------------------------
#[derive(Debug, Default)]
pub struct Anubis {
    pub root: Utf8PathBuf,

    /// When true, external tools (e.g., clang) will be invoked with verbose flags (e.g., -v)
    pub verbose_tools: bool,

    // capability cache
    pub rule_typeinfos: SharedHashMap<RuleTypename, RuleTypeInfo>,

    // environment caches
    pub dir_exists_cache: DashMap<Utf8PathBuf, bool>,

    // papyrus caches
    pub raw_config_cache: SharedHashMap<AnubisConfigRelPath, ArcResult<papyrus::Value>>,
    pub resolved_config_cache: SharedHashMap<ResolvedConfigCacheKey, ArcResult<papyrus::Value>>,
    
    // data caches
    pub mode_cache: SharedHashMap<AnubisTarget, ArcResult<Mode>>,
    pub toolchain_cache: SharedHashMap<ToolchainCacheKey, ArcResult<Toolchain>>,
    pub rule_cache: SharedHashMap<AnubisTarget, ArcResult<dyn Rule>>,

    // job execution caches    
    pub job_cache: DashMap<JobCacheKey, JobId>,
}

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
    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job>;
}
impl_downcast!(sync Rule);

pub trait RuleExt {
    fn create_build_job(self, ctx: Arc<JobContext>) -> Job;
}

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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct JobCacheKey {
    pub mode: Option<AnubisTarget>,
    pub target: AnubisTarget,
    pub action: String,
}

/// Cache key for rule-level job caching (mode + target granularity).
/// This prevents duplicate jobs when the same target is built as a dependency
/// by multiple other targets.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RuleJobCacheKey {
    pub mode: AnubisTarget,
    pub target: AnubisTarget,
}

// ----------------------------------------------------------------------------
// implementations
// ----------------------------------------------------------------------------
impl Anubis {
    pub fn new(root: Utf8PathBuf, verbose_tools: bool) -> anyhow::Result<Anubis> {
        let mut anubis = Anubis {
            root,
            verbose_tools,
            ..Default::default()
        };

        // Initialize anubis with language rules
        tracing::debug!("Registering language rule type infos");
        rules::cc_rules::register_rule_typeinfos(&anubis)?;
        rules::cmd_rules::register_rule_typeinfos(&anubis)?;
        rules::nasm_rules::register_rule_typeinfos(&anubis)?;
        rules::zig_rules::register_rule_typeinfos(&anubis)?;

        Ok(anubis)
    }

    /// Returns the build directory for intermediate build artifacts (object files, etc.)
    /// Path: {root}/.anubis-build/{mode_name}
    pub fn build_dir(&self, mode_name: &str) -> Utf8PathBuf {
        self.root.join(".anubis-build").join(mode_name)
    }

    /// Returns the bin directory for final build outputs (executables, etc.)
    /// Path: {root}/.anubis-bin/{mode_name}
    pub fn bin_dir(&self, mode_name: &str) -> Utf8PathBuf {
        self.root.join(".anubis-bin").join(mode_name)
    }

    /// Returns the temp directory for temporary files during build.
    /// Path: {root}/.anubis-temp
    pub fn temp_dir(&self) -> Utf8PathBuf {
        self.root.join(".anubis-temp")
    }

    pub fn register_rule_typeinfo(&self, ti: RuleTypeInfo) -> anyhow::Result<()> {
        // Acquire write lock
        let mut rtis = write_lock(&self.rule_typeinfos)?;

        // Ensure entry doesn't already exist for this rule type
        if rtis.contains_key(&ti.name) {
            bail_loc!("Already contained entry for {}", &ti.name.0);
        }

        // Store type info
        rtis.insert(ti.name.clone(), ti);
        Ok(())
    }
}

impl AnubisTarget {
    pub fn new(input: &str) -> anyhow::Result<AnubisTarget> {
        // Split on ':'
        let parts: Vec<_> = input.split(":").collect();

        // Expect 2 parts
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

    /// Resolves a relative target to an absolute target given the directory path
    /// relative to the repository root (e.g., "path/to/dir").
    ///
    /// For example, if `dir_relpath` is "samples/external/ffmpeg" and this target is ":avdevice",
    /// the result will be "//samples/external/ffmpeg:avdevice".
    ///
    /// If the target is already absolute, returns a clone of self.
    pub fn resolve(&self, dir_relpath: &str) -> AnubisTarget {
        if !self.is_relative() {
            return self.clone();
        }

        // Build absolute path: //dir_relpath:target_name
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
        root.join(&self.full_path[2..self.separator_idx])
            .join("ANUBIS")
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

impl RuleExt for Arc<dyn Rule> {
    fn create_build_job(self, ctx: Arc<JobContext>) -> Job {
        match self.build(self.clone(), ctx.clone()) {
            Ok(job) => job,
            Err(e) => {
                let desc = format!("Rule error.\n    Rule: [{:?}]\n    Error: [{}]", self, e);
                let display = JobDisplayInfo::from_desc(&desc);
                ctx.new_job(desc, display, Box::new(|_| bail_loc!("Failed to create job.")))
            }
        }
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
// free functions
// ----------------------------------------------------------------------------
pub fn find_anubis_root(start_dir: &Path) -> anyhow::Result<Utf8PathBuf> {
    // Start at the current working directory.
    let mut current_dir = start_dir.to_owned();

    loop {
        // Construct the candidate path by joining the current directory with ".anubis_root".
        let candidate = current_dir.join(".anubis_root");
        if candidate.exists() && candidate.is_file() {
            return Utf8PathBuf::try_from(candidate)
                .map_err(|e| anyhow_loc!("Non-UTF-8 path to .anubis_root: {:?}", e));
        }

        // Try moving up to the parent directory.
        if !current_dir.pop() {
            bail_loc!(
                "Failed to find .anubis_root in any parent directory starting from [{:?}]",
                current_dir
            )
        }
    }
}

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

fn read_lock<T>(lock: &Arc<RwLock<T>>) -> anyhow::Result<std::sync::RwLockReadGuard<'_, T>> {
    lock.read().map_err(|e| anyhow_loc!("Lock poisoned: {}", e))
}

fn write_lock<T>(lock: &Arc<RwLock<T>>) -> anyhow::Result<std::sync::RwLockWriteGuard<'_, T>> {
    lock.write().map_err(|e| anyhow_loc!("Lock poisoned: {}", e))
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

impl Anubis {
    pub fn get_mode(&self, mode_target: &AnubisTarget) -> anyhow::Result<Arc<Mode>> {
        // check if mode already exists
        if let Some(mode) = read_lock(&self.mode_cache)?.get(mode_target) {
            return mode.clone();
        }

        // get raw config
        let config_path = mode_target.get_config_relpath();
        let config = self.get_raw_config(&config_path)?;

        // deserialize mode
        let mut mode: anyhow::Result<Mode> =
            config.deserialize_named_object::<Mode>(mode_target.target_name());

        // inject target into mode
        if let Ok(m) = &mut mode {
            m.target = mode_target.clone();
        }

        // inject host platform
        if let Ok(m) = &mut mode {
            // ex: windows, linux, macos
            m.vars.insert("host_platform".into(), std::env::consts::OS.into());

            // we use our own architecture naming scheme
            let host_arch = match std::env::consts::ARCH {
                "x86_64" => "x64",
                "aarch64" => "arm64",
                default => bail_loc!("Unsupported host architecture {}", std::env::consts::ARCH),
            };
            m.vars.insert("host_arch".into(), host_arch.into());
        }

        // Arcify and store mode
        let mode: ArcResult<Mode> = mode.arcify();
        write_lock(&self.mode_cache)?.insert(mode_target.clone(), mode.clone());
        mode
    }

    pub fn get_toolchain(
        &self,
        mode: Arc<Mode>,
        toolchain_target: &AnubisTarget,
    ) -> anyhow::Result<Arc<Toolchain>> {
        // Check if toolchain already exists
        let key = ToolchainCacheKey {
            mode: mode.target.clone(),
            toolchain: toolchain_target.clone(),
        };
        if let Some(toolchain) = read_lock(&self.toolchain_cache)?.get(&key) {
            return toolchain.clone();
        }

        let mut toolchain = (|| {
            // get config
            let config = self.get_resolved_config(&toolchain_target.get_config_relpath(), &*mode)?;

            // deserialize toolchain
            config.deserialize_named_object::<Toolchain>(toolchain_target.target_name())
        })();

        // inject target into toolchain
        if let Ok(t) = &mut toolchain {
            t.target = toolchain_target.clone();
        }
        let toolchain = toolchain.arcify();

        // store Toolchain
        write_lock(&self.toolchain_cache)?.insert(key, toolchain.clone());
        toolchain
    }

    fn get_raw_config(&self, config_path: &AnubisConfigRelPath) -> ArcResult<papyrus::Value> {
        let paps = read_lock(&self.raw_config_cache)?;
        let maybe_papyrus = paps.get(config_path);
        match maybe_papyrus {
            Some(Ok(papyrus)) => Ok(papyrus.clone()),
            Some(Err(e)) => {
                bail_loc!(
                    "Can't get mode [{}] because papyrus parse failed with [{}]",
                    config_path.0,
                    e
                )
            }
            None => {
                // drop read lock
                drop(paps);

                // parse papyrus file
                let filepath = config_path.get_abspath(&self.root);
                let result = papyrus::read_papyrus_file(filepath.as_ref()).map(|v| Arc::new(v));

                // acquire write lock and store
                write_lock(&self.raw_config_cache)?.insert(config_path.clone(), result.clone());
                result
            }
        }
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

        // check if resolved config already exists
        if let Some(config) = read_lock(&self.resolved_config_cache)?.get(&cache_key) {
            return config.clone();
        }

        // get raw config
        let raw_config = self.get_raw_config(config_relpath)?;
        let config_abspath = config_relpath.get_abspath(&self.root);
        let config_dir = config_abspath.parent().unwrap();

        // Extract the directory relative path for resolving relative targets
        // config_relpath is like "//path/to/dir/ANUBIS", we want "path/to/dir"
        let dir_relpath = config_relpath.get_dir_relpath();

        let resolved_config = match resolve_value_with_dir(
            (*raw_config).clone(),
            config_dir.as_std_path(),
            &mode.vars,
            Some(&dir_relpath),
        ) {
            Ok(v) => Ok::<papyrus::Value, anyhow::Error>(v),
            Err(e) => {
                let e_str = e.to_string();
                bail_loc!("Error resolving config [{:?}]: {}", config_relpath.0, e_str)
            }
        }?;

        // Store the resolved config in cache
        let arc_resolved = Arc::new(resolved_config);
        write_lock(&self.resolved_config_cache)?.insert(cache_key, Ok(arc_resolved.clone()));

        Ok(arc_resolved)
    }

    pub fn get_rule(&self, rule: &AnubisTarget, mode: &Mode) -> ArcResult<dyn Rule> {
        // check cache
        if let Some(rule) = read_lock(&self.rule_cache)?.get(rule) {
            return rule.clone();
        }

        let new_rule = (|| {
            // get resolved config
            let config = self.get_resolved_config(&rule.get_config_relpath(), mode)?;

            // get rule object
            let papyrus = config.get_named_object(rule.target_name())?;
            let rule_typename = match papyrus {
                Value::Object(obj) => RuleTypename(obj.typename.clone()),
                _ => bail_loc!("Rule [{}] ", rule),
            };

            // deserialize rule
            let rtis = read_lock(&self.rule_typeinfos)?;
            let rti = rtis
                .get(&rule_typename)
                .ok_or_else(|| anyhow_loc!("No rule typeinfo entry for [{}]", rule_typename.0))?;
            (rti.parse_rule)(rule.clone(), papyrus)
        })();

        // store rule in cache
        write_lock(&self.rule_cache)?.insert(rule.clone(), new_rule.clone());

        new_rule
    }

    /// Build a rule target, using the rule-level job cache to prevent duplicate jobs.
    ///
    /// This method checks if a job for the given (mode, target) combination already exists.
    /// If it does, it returns the existing JobId. Otherwise, it creates a new job,
    /// adds it to the job system, caches it, and returns the new JobId.
    pub fn build_rule(&self, target: &AnubisTarget, ctx: &Arc<JobContext>) -> anyhow::Result<JobId> {
        let mode = ctx.mode.as_ref().ok_or_else(|| anyhow_loc!("Cannot build rule without a mode"))?;

        // Create job key
        let cache_key = JobCacheKey {
            mode: Some(mode.target.clone()),
            target: target.clone(),
            action: "build_rule".into(),
        };

        // Use DashMap's entry API to atomically check and insert
        use dashmap::mapref::entry::Entry;
        match self.job_cache.entry(cache_key) {
            Entry::Occupied(entry) => {
                tracing::trace!("Job Cache Hit: key: [{:?}] id [{}]", entry.key(), entry.get());
                Ok(*entry.get())
            }
            Entry::Vacant(entry) => {
                let rule = self.get_rule(target, mode)?;
                let job = rule.build(rule.clone(), ctx.clone())?;
                let job_id = job.id;

                entry.insert(job_id);
                ctx.job_system.add_job(job)?;

                Ok(job_id)
            }
        }
    }
    /// Verify that all directories exist, using a cache to avoid redundant filesystem checks.
    ///
    /// This method validates directories early in the build process to provide clear
    /// error messages when directories don't exist, rather than cryptic compiler errors.
    ///
    /// The `dir_type` parameter is used in error messages to describe what kind of
    /// directories are being validated (e.g., "include", "library", "system include").
    ///
    /// Returns Ok(()) if all directories exist, or an error listing the missing directories.
    pub fn verify_directories<'a>(
        &self,
        directories: impl IntoIterator<Item = &'a Utf8PathBuf>,
        dir_type: &str,
    ) -> anyhow::Result<()> {
        let mut missing_dirs = Vec::new();

        for dir in directories {
            // Check cache first
            if let Some(exists) = self.dir_exists_cache.get(dir) {
                if !*exists {
                    missing_dirs.push(dir.clone());
                }
                continue;
            }

            // Check filesystem and cache the result
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
} // impl anubis

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
        mode_target,
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

/// Build multiple targets using a shared JobSystem.
///
/// This is more efficient than calling `build_single_target` in a loop because:
/// 1. Dependencies are only built once (shared via job caches)
/// 2. Job IDs remain valid across all targets (same JobSystem instance)
///
/// The job caches (`job_cache`, `rule_job_cache`) map target+substep to JobIds,
/// and JobIds are only valid within a single JobSystem. By using one JobSystem
/// for all targets, we ensure cached JobIds remain valid.
///
/// Returns a Vec of artifacts in the same order as the input targets.
pub fn build_targets(
    anubis: Arc<Anubis>,
    mode_target: &AnubisTarget,
    toolchain_path: &AnubisTarget,
    target_paths: &[AnubisTarget],
    num_workers: usize,
    progress_tx: crossbeam::channel::Sender<crate::progress::ProgressEvent>,
) -> anyhow::Result<Vec<Arc<dyn JobArtifact>>> {
    if target_paths.is_empty() {
        return Ok(Vec::new());
    }

    // Get mode
    tracing::debug!(mode_target = %mode_target.target_path(), "Loading build mode");
    let mode = anubis.get_mode(mode_target)?;

    // Get toolchain for mode
    tracing::debug!(
        toolchain_path = %toolchain_path.target_path(),
        mode = %mode.name,
        "Loading toolchain configuration"
    );
    let toolchain = anubis.get_toolchain(mode.clone(), toolchain_path)?;

    // Create a SINGLE job system shared across ALL targets
    let job_system: Arc<JobSystem> = Arc::new(JobSystem::new());
    let job_context = Arc::new(JobContext {
        anubis,
        job_system: job_system.clone(),
        mode: Some(mode.clone()),
        toolchain: Some(toolchain),
    });

    // Add initial jobs for ALL targets using build_rule to populate the cache
    // This ensures that if target A and target B both appear in the list,
    // and A depends on B, we don't create duplicate jobs for B.
    // Collect job IDs to retrieve artifacts later.
    let mut job_ids = Vec::with_capacity(target_paths.len());
    for target_path in target_paths {
        tracing::debug!(
            target_path = %target_path.target_path(),
            mode = %mode.name,
            "Loading build rule"
        );
        let job_id = job_context.anubis.build_rule(target_path, &job_context)?;
        job_ids.push(job_id);
    }

    // Build ALL targets together

    // Give the progress display a live counter so it can poll the total job count each tick.
    // This is necessary because deferred jobs (e.g., CcBinary) create child compile/link jobs
    // dynamically, so the total grows well beyond the initially seeded count.
    let _ = progress_tx.send(crate::progress::ProgressEvent::SetJobCounter {
        counter: job_system.next_id.clone(),
    });

    JobSystem::run_to_completion(job_system.clone(), num_workers, progress_tx)?;

    // Log completion and collect artifacts for all targets
    let mut artifacts = Vec::with_capacity(target_paths.len());
    for (target_path, job_id) in target_paths.iter().zip(job_ids.iter()) {
        tracing::info!(
            "Build complete [{} {}]",
            mode_target.target_path(),
            target_path.target_path()
        );
        let artifact = job_system.get_result(*job_id)?;
        artifacts.push(artifact);
    }

    Ok(artifacts)
}

/// Represents a target pattern that can match multiple targets.
/// For example, "//samples/basic/..." matches all targets under the samples/basic directory.
#[derive(Clone, Debug)]
pub struct TargetPattern {
    /// The directory path relative to project root (e.g., "samples/basic" for "//samples/basic/...")
    pub dir_relpath: String,
}

impl TargetPattern {
    /// Check if a string is a target pattern (ends with "/...")
    /// Valid patterns: "//samples/basic/...", "///..." (root)
    /// Invalid: "//..." (would require unsafe arithmetic)
    pub fn is_pattern(s: &str) -> bool {
        // Pattern must be "//path/..." where path can be empty (for root: "///...")
        // Minimum valid pattern is "///..." (6 chars): // + / + ...
        s.len() >= 6 && s.starts_with("//") && s.ends_with("/...")
    }

    /// Parse a target pattern string.
    /// Returns None if the string is not a valid pattern.
    ///
    /// Examples:
    /// - "//samples/basic/..." -> Some(TargetPattern { dir_relpath: "samples/basic" })
    /// - "///..." -> Some(TargetPattern { dir_relpath: "" }) (root pattern)
    /// - "//..." -> None (invalid - use "///..." for root)
    pub fn parse(s: &str) -> Option<TargetPattern> {
        // Use safe string operations to avoid panics on edge cases
        let without_prefix = s.strip_prefix("//")?;
        let without_suffix = without_prefix.strip_suffix("/...")?;

        Some(TargetPattern {
            dir_relpath: without_suffix.to_owned(),
        })
    }
}

/// Expand a target pattern into a list of concrete target paths.
///
/// For example, "//samples/basic/..." expands to all targets in all ANUBIS files
/// under the samples/basic directory and its subdirectories.
pub fn expand_target_pattern(
    project_root: &Path,
    pattern: &TargetPattern,
    rule_typeinfos: &SharedHashMap<RuleTypename, RuleTypeInfo>,
) -> anyhow::Result<Vec<String>> {
    let mut targets = Vec::new();

    // Determine the base directory to search
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
        let rtis = read_lock(rule_typeinfos)?;
        rtis.keys().map(|k| k.0.clone()).collect()
    };

    // Recursively find all ANUBIS files
    find_anubis_files_recursive(&search_dir, project_root, &known_rules, &mut targets)?;

    targets.sort();
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
