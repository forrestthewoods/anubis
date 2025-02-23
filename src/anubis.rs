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

    // ANUBISpath -> Value
    // raw_papyrus: DashMap<String, Result<papyrus::Value>>
    
    // ModePath -> HashMap<ModeTarget, Result<papyrus::Value>>
    // mode_resolved_papyrus: DashMap<AnubisTarget, HashMap<AnubisPath, Result<papyrus::Value>>>

    // rules: DashMap<ModeTarget, HashMap<AnubisTarget, Result<Box<dyn Rule>>>

    // ModePath -> HashMap<TargetPath, BuildResult>
    // build_results: DashMap<AnubisTarget, HashMap<AnubisTarget, Box<dyn BuildResult>>>
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AnubisTarget {
    full_path: String,      // ex: //path/to/foo:bar
    separator_idx: usize,   // index of ':'    
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

impl AnubisTarget {
    fn new(input: &str) -> anyhow::Result<AnubisTarget> {
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

            if parts[1].contains("/") {
                bail_loc!("Invalid input. No slashes allowed after ':'. input: [{}]", input);
            }

            Ok(AnubisTarget{
                full_path: input.to_owned(),
                separator_idx: parts[0].len(),
            })
        } else if parts.len() == 1 {
            bail_loc!("relative paths not currently supported. input: [{}]", input);
        } else {
            bail_loc!("input must contain only a single colon. input: [{}]", input);
        }
    }

    pub fn dir_path(&self) -> &str {
        &self.full_path[2..self.separator_idx]
    }

    pub fn target_name(&self) -> &str {
        &self.full_path[self.separator_idx+1..]
    }

    pub fn get_config_path(&self, root: &Path) -> PathBuf {
        let mut result = PathBuf::new();
        for component in root.join(self.dir_path()).join("ANUBIS").components() {
            result.push(component);
        }
        result

        //Self::normalize_pathbuf(root.join(self.dir_path()).join(&"ANUBIS"))
        //root.join(self.dir_path()).join(&"ANUBIS").normalize().unwrap().into_path_buf()
    }

    fn normalize_pathbuf(path: PathBuf) -> PathBuf {
        let components: Vec<_> = path.components().collect();
        let mut normalized = PathBuf::with_capacity(path.as_os_str().len()); // Pre-allocate
        for (i, component) in components.iter().enumerate() {
            if i > 0 {
                normalized.push("/");
            }
            normalized.push(component.as_os_str());
        }
        normalized
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

pub fn build_single_target(anubis: &Anubis, mode_path: &str, target_path: &str) -> anyhow::Result<()> {
    // Parse inputs
    let mode_target = AnubisTarget::new(mode_path)?;
    let target = AnubisTarget::new(target_path)?;

    
    // Read modes config
    // let modes = read_papyrus_file(&mode_target.config_file_abspath)?;

    // // Deserialize object
    // let mode = modes.deserialize_named_object::<Mode>(&mode_target.target_name)?;
    // dbg!(mode);

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
    }

    #[test]
    fn anubis_abspath() {
        let root = PathBuf::from_str("c:/stuff/proj_root").unwrap();

        assert_eq!(AnubisTarget::new("//hello:world").unwrap().get_config_path(&root).to_string_lossy(), "c:/stuff/proj_root/hello/ANUBIS");
    }
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
