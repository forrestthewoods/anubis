use crate::cpp_rules::*;
use crate::job_system::*;
use crate::papyrus;
use crate::papyrus::*;
use crate::toolchain;
use crate::toolchain::Mode;
use crate::toolchain::Toolchain;
use crate::util::SlashFix;
use crate::{anyhow_loc, bail_loc, function_name};
use crate::{anyhow_with_context, bail_with_context, timed_span};
use crate::{cpp_rules, job_system};
use anyhow::{anyhow, bail, Result};
use dashmap::DashMap;
use downcast_rs::{impl_downcast, DowncastSync};
use heck::ToLowerCamelCase;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::any;
use std::any::Any;
use std::collections::HashMap;
use std::path::Display;
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
    pub root: PathBuf,

    // caches
    pub raw_config_cache: SharedHashMap<AnubisConfigRelPath, ArcResult<papyrus::Value>>,
    pub resolved_config_cache: SharedHashMap<AnubisConfigRelPath, ArcResult<papyrus::Value>>,
    pub mode_cache: SharedHashMap<AnubisTarget, ArcResult<Mode>>,
    pub toolchain_cache: SharedHashMap<ToolchainCacheKey, ArcResult<Toolchain>>,
    pub rule_cache: SharedHashMap<AnubisTarget, ArcResult<dyn Rule>>,
    pub rule_typeinfos: SharedHashMap<RuleTypename, RuleTypeInfo>,
    pub job_cache: SharedHashMap<JobCacheKey, JobId>,
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct JobCacheKey {
    pub mode: AnubisTarget,
    pub target: AnubisTarget,
    pub substep: Option<String>,
}

// ----------------------------------------------------------------------------
// implementations
// ----------------------------------------------------------------------------
impl Anubis {
    pub fn new(root: PathBuf) -> Anubis {
        Anubis {
            root,
            ..Default::default()
        }
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

    pub fn target_path(&self) -> &str {
        &self.full_path
    }

    // given //path/to/foo:bar returns bar
    pub fn target_name(&self) -> &str {
        &self.full_path[self.separator_idx + 1..]
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

    pub fn get_config_abspath(&self, root: &Path) -> PathBuf {
        // returns: c:/stuff/project/path/to/foo/ANUBIS
        // convert '\\' to '/' so paths are same on Linux/Windows
        root.join(&self.full_path[2..self.separator_idx])
            .join(&"ANUBIS")
            .to_string_lossy()
            .replace("\\", "/")
            .into()
    }
}

impl std::fmt::Display for AnubisTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.full_path)
    }
}

impl AnubisConfigRelPath {
    pub fn get_abspath(&self, root: &Path) -> PathBuf {
        root.join(&self.0[2..]).slash_fix()
    }
}

impl RuleExt for Arc<dyn Rule> {
    fn create_build_job(self, ctx: Arc<JobContext>) -> Job {
        match self.build(self.clone(), ctx.clone()) {
            Ok(job) => job,
            Err(e) => ctx.new_job(
                format!("Rule error.\n    Rule: [{:?}]\n    Error: [{}]", self, e),
                Box::new(|_| JobFnResult::Error(anyhow!("Failed to create job."))),
            ),
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
        let full_path = String::deserialize(deserializer)?;
        AnubisTarget::new(&full_path).map_err(serde::de::Error::custom)
    }
}

// ----------------------------------------------------------------------------
// free functions
// ----------------------------------------------------------------------------
pub fn find_anubis_root(start_dir: &Path) -> anyhow::Result<PathBuf> {
    // Start at the current working directory.
    let mut current_dir = start_dir.to_owned();

    loop {
        // Construct the candidate path by joining the current directory with ".anubis_root".
        let candidate = current_dir.join(".anubis_root");
        if candidate.exists() && candidate.is_file() {
            return Ok(candidate);
        }

        // Try moving up to the parent directory.
        if !current_dir.pop() {
            bail!(
                "Failed to find .anubis_root in any parent directory starting from [{:?}]",
                current_dir
            )
        }
    }
}

pub fn build_target(anubis: &Anubis, target: &Path) -> anyhow::Result<()> {
    // Convert the target path to a string so we can split it.
    let target_str =
        target.to_str().ok_or_else(|| anyhow!("Invalid target path [{:?}] (non UTF-8)", target))?;

    // Split by ':' and ensure there are exactly two parts.
    let parts: Vec<&str> = target_str.split(':').collect();
    if parts.len() != 2 {
        bail!(
            "Expected target of the form <config_path>:<cpp_binary_name>, got: {}",
            target_str
        );
    }
    let config_path_str = parts[0];
    let binary_name = parts[1];

    let config_path = if config_path_str.starts_with("//") {
        anubis.root.join(&config_path_str[2..]).join("ANUBIS")
    } else {
        bail!(
            "Anubis build targets must start with '//'. Target: [{:?}]",
            target
        );
    };

    // Load the papyrus config from the file specified by the first part.
    let config = read_papyrus_file(&config_path)?;

    // Expect the config to be an array and filter for cpp_binary entries.
    let rules: Vec<CppBinary> = match config {
        Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| {
                if let Value::Object(ref obj) = v {
                    if obj.typename == "cpp_binary" {
                        let de = crate::papyrus_serde::ValueDeserializer::new(&v);
                        return Some(CppBinary::deserialize(de).map_err(|e| anyhow!("{}", e)));
                    }
                }
                None
            })
            .collect::<Result<Vec<CppBinary>, anyhow::Error>>()?,
        _ => bail!("Expected config root to be an array"),
    };

    // Find the CppBinary with a matching name.
    let matching_binary = rules
        .into_iter()
        .find(|r| r.name == binary_name)
        .ok_or_else(|| anyhow!("No cpp_binary with name '{}' found in config", binary_name))?;

    tracing::debug!(
        binary_name = binary_name,
        target = %matching_binary.target(),
        "Found matching binary in configuration"
    );

    Ok(())
}

trait ResultExt<T> {
    fn clone(self) -> anyhow::Result<T>
    where
        T: Clone;
}

impl<T: Clone> ResultExt<T> for &anyhow::Result<T> {
    fn clone(self) -> anyhow::Result<T> {
        self.as_ref().map(|v| v.clone()).map_err(|e| anyhow!("{}", e))
    }
}

fn read_lock<T>(lock: &Arc<RwLock<T>>) -> anyhow::Result<std::sync::RwLockReadGuard<'_, T>> {
    lock.read().map_err(|e| anyhow!("Lock poisoned: {}", e))
}

fn write_lock<T>(lock: &Arc<RwLock<T>>) -> anyhow::Result<std::sync::RwLockWriteGuard<'_, T>> {
    lock.write().map_err(|e| anyhow!("Lock poisoned: {}", e))
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
    fn get_mode(&self, mode_target: &AnubisTarget) -> anyhow::Result<Arc<Mode>> {
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
                let result = papyrus::read_papyrus_file(&filepath).map(|v| Arc::new(v));

                // acquire write lock and store
                write_lock(&self.raw_config_cache)?.insert(config_path.clone(), result.clone());
                result
            }
        }
    }

    fn get_resolved_config(
        &self,
        config_relpath: &AnubisConfigRelPath,
        mode: &Mode,
    ) -> ArcResult<papyrus::Value> {
        // check if resolved config already exists
        if let Some(config) = read_lock(&self.resolved_config_cache)?.get(config_relpath) {
            return config.clone();
        }

        // get raw config
        let raw_config = self.get_raw_config(config_relpath)?;
        let config_abspath = config_relpath.get_abspath(&self.root);
        let config_dir = config_abspath.parent().unwrap();
        let resolved_config = match resolve_value((*raw_config).clone(), &config_dir, &mode.vars) {
            Ok(v) => Ok::<papyrus::Value, anyhow::Error>(v),
            Err(e) => bail!(e.context(format!("Error resolving config [{:?}]", config_relpath.0))),
        }?;

        // Store the resolved config in cache
        let arc_resolved = Arc::new(resolved_config);
        write_lock(&self.resolved_config_cache)?.insert(config_relpath.clone(), Ok(arc_resolved.clone()));

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
} // impl anubis

pub fn build_single_target(
    anubis: Arc<Anubis>,
    mode_target: &AnubisTarget,
    toolchain_path: &AnubisTarget,
    target_path: &AnubisTarget,
) -> anyhow::Result<()> {
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

    // get rule
    tracing::debug!(
        target_path = %target_path.target_path(),
        mode = %mode.name,
        "Loading build rule"
    );
    let rule = anubis.get_rule(target_path, &*mode)?;

    // Create job system
    let job_system: Arc<JobSystem> = Arc::new(JobSystem::new());
    let job_context = Arc::new(JobContext {
        anubis,
        job_system: job_system.clone(),
        mode: Some(mode),
        toolchain: Some(toolchain),
    });

    // Create initial job for initial rule
    let init_job = rule.create_build_job(job_context);
    job_system.add_job(init_job)?;

    // Build single rule
    //let num_workers = 1;
    let num_workers = num_cpus::get_physical();
    JobSystem::run_to_completion(job_system.clone(), num_workers)?;
    tracing::info!("Build complete");

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    macro_rules! assert_ok {
        ($result:expr) => {
            assert!($result.is_ok(), "Expected Ok, got Err: {:#?}", $result);
        };
    }

    macro_rules! assert_err {
        ($result:expr) => {
            assert!($result.is_err(), "Expected Err, got Ok: {:#?}", $result);
        };
    }

    #[test]
    fn anubis_target_invalid() {
        assert_err!(AnubisTarget::new("foo"));
        assert_err!(AnubisTarget::new("foo:bar"));
        assert_err!(AnubisTarget::new("//foo:bar:baz"));
        assert_err!(AnubisTarget::new("//foo"));
        assert_err!(AnubisTarget::new("//ham/eggs"));
        assert_err!(AnubisTarget::new("//ham:eggs/bacon"));

        // should be valid, but is not yet implemented
        assert_err!(AnubisTarget::new(":eggs"));
    }

    #[test]
    fn anubis_target_valid() {
        assert_ok!(AnubisTarget::new("//foo:bar"));
        assert_ok!(AnubisTarget::new("//foo/bar:baz"));

        let t = AnubisTarget::new("//foo/bar:baz").unwrap();
        assert_eq!(t.get_relative_dir(), "foo/bar");
        assert_eq!(t.target_name(), "baz");
    }

    #[test]
    fn anubis_abspath() {
        let root = PathBuf::from_str("c:/stuff/proj_root").unwrap();

        assert_eq!(
            AnubisTarget::new("//hello:world").unwrap().get_config_abspath(&root).to_string_lossy(),
            "c:/stuff/proj_root/hello/ANUBIS"
        );
    }
}
