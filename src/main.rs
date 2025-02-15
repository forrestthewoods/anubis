#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

mod cpp_rules;
mod papyrus;
mod papyrus_serde;
mod toolchain;

use anyhow::{anyhow, bail};
use cpp_rules::*;
use dashmap::DashMap;
use logos::Logos;
use papyrus::*;
use serde::Deserialize;
use std::any;
use std::any::Any;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use toolchain::*;

#[derive(Clone, Debug, Default, Deserialize)]
struct AnubisRoot {
    output_dir: PathBuf,
}

fn read_papyrus(path: &Path) -> anyhow::Result<papyrus::Value> {
    if !std::fs::exists(path)? {
        bail!("read_papyrus failed because file didn't exist: [{:?}]", path);
    }

    let src = fs::read_to_string(path)?;

    let mut lexer = Token::lexer(&src);
    let result = parse_config(&mut lexer);

    match result {
        Ok(value) => {
            //println!("{:#?}", config);

            let resolve_root = PathBuf::from_str("c:/source_control/anubis/examples/simple_cpp")?;
            let resolve_vars: HashMap<String, String> = [("platform", "windows"), ("arch", "x64")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();

            let value = resolve_value(value, &resolve_root, &resolve_vars)?;

            Ok(value)
        }
        Err(e) => {
            use ariadne::{ColorGenerator, Label, Report, ReportKind, Source};

            let mut colors = ColorGenerator::new();
            let a = colors.next();

            // ariadne sucks and has utterly inscrutable trait errors
            let p = path.to_string_lossy().to_string();
            let pp = p.as_str();

            let mut buf: Vec<u8> = Default::default();
            Report::build(ReportKind::Error, pp, 12)
                .with_message("Invalid ANUBIS".to_string())
                .with_label(Label::new((pp, e.span)).with_message(e.error).with_color(a))
                .finish()
                .write_for_stdout((pp, Source::from(src)), &mut buf)
                .unwrap();

            let err_msg = String::from_utf8(buf)?;
            bail!(err_msg)
        }
    }
}

#[derive(Debug, Default)]
struct Anubis {
    root: PathBuf,
    rule_typeinfos: dashmap::DashMap<String, RuleTypeInfo>,
    rules: dashmap::DashMap<String, Box<dyn Any>>,
}

impl Anubis {
    pub fn new(root: PathBuf) -> Anubis {
        Anubis {
            root,
            ..Default::default()
        }
    }
}

impl Anubis {
    fn register_rule_typeinfo(&self, ti: RuleTypeInfo) -> anyhow::Result<()> {
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

#[derive(Debug)]
struct RuleTypeInfo {
    pub name: String,
    pub create_rule: fn(papyrus::Value) -> anyhow::Result<Box<dyn Rule + 'static>>,
}

trait Rule {
    fn name(&self) -> String;
}

fn build(anubis: &Anubis, target: &Path) -> anyhow::Result<()> {
    // Convert the target path to a string so we can split it.
    let target_str = target
        .to_str()
        .ok_or_else(|| anyhow!("Invalid target path [{:?}] (non UTF-8)", target))?;

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
        bail!("Anubis build targets must start with '//'. Target: [{:?}]", target);
    };

    // Load the papyrus config from the file specified by the first part.
    let config = read_papyrus(&config_path)?;

    // Expect the config to be an array and filter for cpp_binary entries.
    let rules: Vec<CppBinary> = match config {
        Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| {
                if let Value::Object(ref obj) = v {
                    if obj.typename == "cpp_binary" {
                        let de = crate::papyrus_serde::ValueDeserializer::new(v);
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

fn find_anubis_root(start_dir: &Path) -> anyhow::Result<PathBuf> {
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
            bail!("Failed to find .anubis_root in any parent directory starting from [{:?}]", current_dir)
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Create anubis
    let cwd = std::env::current_dir()?;
    let mut anubis = Anubis::new(cwd.to_owned());

    // Initialize anubis with language rules
    // Could someday be via dynamic libs
    cpp_rules::register_rule_typeinfos(&mut anubis)?;

    // Build a target!
    build(&anubis, &Path::new("//examples/hello_world:hello_world"))
}

// create a build rule

struct BuildResult {
    output_files : HashMap<String, Vec<PathBuf>>, // category -> files
}
impl JobResult for BuildResult{}

struct Job {
    job_id: i64,
    job_fn : Box<dyn Fn() -> anyhow::Result<Box<dyn JobResult>>>,
}

enum JobStatus {
    Blocked,
    Queued, 
    Running,
    Succeeded,
    Failed,
}

struct JobSystem {
    blocked_jobs : DashMap<i64, Job>,
    job_results : DashMap<i64, anyhow::Result<Box<dyn JobResult>>>,
}

struct JobWorker {
    sender : crossbeam::channel::Sender<Job>,
    receiver: crossbeam::channel::Receiver<Job>,
}


trait JobResult{}

// build a target
// create a job system
// create a job cache
// create a build rule job
// look-up function to build rule
    // creates list sub-jobs
    // creates new job with dependency on subjobs
        // this subjob writes its output to the original job

// need to create a hash for a job
    // job hash:
        // rule: target + vars?
        // compile_obj: file + vars?
// job can be queued, processing, completed, failed, depfailed
