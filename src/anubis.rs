use crate::cpp_rules;
use crate::cpp_rules::*;
use crate::papyrus;
use crate::papyrus::*;
use crate::toolchain::Mode;
use crate::{bail_loc, function_name};
use anyhow::{anyhow, bail};
use dashmap::DashMap;
use normpath::PathExt;
use serde::Deserialize;
use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ----------------------------------------------------------------------------
// declarations
// ----------------------------------------------------------------------------
#[derive(Debug, Default)]
pub struct Anubis {
    pub root: PathBuf,
    pub rule_typeinfos: DashMap<String, RuleTypeInfo>,
    pub rules: DashMap<String, Box<dyn Any>>,

    // raw_configs: DashMap<String, Option<papyrus::Value>>
    // mode_resolved_configs: DashMap<AnubisTarget, HashMap<AnubisPath, Option<Mode>>>
    // rules: DashMap<
    // build_results: DashMap<(AnubisTarget, AnubisTarget), Box<dyn BuildResult>>
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AnubisTarget {
    config_relpath: String,       // ex: //path/to/foo
    target_name: String,          // ex: bar
    config_file_abspath: PathBuf, // ex: c:/blah/reporoot/path/to/foo/ANUBIS
}

#[derive(Debug)]
pub struct RuleTypeInfo {
    pub name: String,
    pub parse_rule: fn(papyrus::Value) -> anyhow::Result<Box<dyn Rule>>,
}

pub trait Rule: 'static {
    fn name(&self) -> String;
}

pub struct BuildResult {
    pub output_files: HashMap<String, Vec<PathBuf>>, // category -> files
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
        if self.rule_typeinfos.contains_key(&ti.name) {
            bail!(
                "Anubis::register_rule_typeinfo already contained entry for {}",
                &ti.name
            );
        }

        self.rule_typeinfos.insert(ti.name.clone(), ti);
        Ok(())
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

    println!("Found matching binary: {:#?}", matching_binary);

    Ok(())
}

impl AnubisTarget {
    fn from_str(input: &str, repo_root: &Path, cwd: &Path) -> anyhow::Result<AnubisTarget> {
        // Split on ':'
        let parts: Vec<_> = input.split(":").collect();

        // Expect 1 or 2 parts
        if parts.len() == 0 || parts.len() > 2 {
            bail_loc!(
                "Split on ':' had [{}] parts when must be 1 or 2. input: [{}]",
                parts.len(),
                input
            );
        }

        if parts.len() == 2 {
            // This is repo relative
            if !parts[0].starts_with("//") {
                bail_loc!("Input string expected to start with '//'. input: [{}]", input);
            }

            let repo_fullpath = input.to_owned();
            let config_file_abspath =
                repo_root.join(&parts[0][2..]).join("ANUBIS").normalize()?.into_path_buf();
            let target_name = parts[1].to_owned();

            return Ok(AnubisTarget {
                config_relpath: parts[0].to_owned(),
                target_name,
                config_file_abspath,
            });
        } else {
            bail_loc!("relative paths not currently supported");
        }
    }
}

pub fn build_single_target(anubis: &Anubis, mode_path: &str, target_path: &str) -> anyhow::Result<()> {
    // Parse inputs
    let mode_target = AnubisTarget::from_str(mode_path, &anubis.root, &anubis.root)?;
    let target = AnubisTarget::from_str(target_path, &anubis.root, &anubis.root)?;

    // Read modes config
    let modes = read_papyrus_file(&mode_target.config_file_abspath)?;

    // Deserialize object
    let mode = modes.deserialize_named_object::<Mode>(&mode_target.target_name)?;
    dbg!(mode);

    Ok(())
}

// build_targets(targets: Vec<(Mode, Vec<Target>)>
// read mode

// build_single_target(mode_path, target_path)
// parse mode
// read file to string
// read to papyrus::value
// deserialize
// store in anubis.modes(string, mode)
// parse target
// read file to string
// read to papyrus::value
// store in anubis.raw_rules(target_path)
// resolve w/ mode
// store in anubis.rules(mode, target_path)
// build(mode, target)
// build_target
