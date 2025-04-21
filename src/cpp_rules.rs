#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis::{self, AnubisTarget};
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;

use crate::papyrus::*;
use crate::{anyhow_loc, bail_loc, function_name, job_system::*};

// ----------------------------------------------------------------------------
// Declarations
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Deserialize)]
pub struct CppBinary {
    pub name: String,
    pub srcs: Vec<PathBuf>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

// ----------------------------------------------------------------------------
// Implementations
// ----------------------------------------------------------------------------
impl Rule for CppBinary {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn create_build_job_impl(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let cpp = arc_self
            .clone()
            .downcast_arc::<CppBinary>()
            .map_err(|_| anyhow::anyhow!("Failed to downcast rule [{:?}] to CppBinary", arc_self))?;

        Ok(ctx.new_job(
            format!("Build CppBinary Target {}", self.target.target_path()),
            Box::new(move |job| build_cpp_binary(cpp.clone(), job)),
        ))
    }
}

impl crate::papyrus::PapyrusObjectType for CppBinary {
    fn name() -> &'static str {
        &"cpp_binary"
    }
}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------
fn parse_cpp_binary(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut cpp = CppBinary::deserialize(de).map_err(|e| anyhow::anyhow!("{}", e))?;
    cpp.target = t;
    Ok(Arc::new(cpp))
}

fn build_cpp_binary(cpp: Arc<CppBinary>, mut job: Job) -> JobFnResult {
    let mut deferral: JobDeferral = Default::default();

    // create child job to compile each src
    for src in &cpp.srcs {
        let child_job = build_cpp_file(src.clone(), &cpp, job.ctx.clone());
        deferral.graph_updates.push(JobGraphEdge {
            blocked: job.id,
            blocker: child_job.id,
        });
        deferral.new_jobs.push(child_job);
    }

    // create child job to link

    // update and re-use this job
    job.job_fn = Some(Box::new(|job| JobFnResult::Error(anyhow_loc!("oh noooo"))));
    deferral.new_jobs.push(job);

    // Defer!
    JobFnResult::Deferred(deferral)
}

fn build_cpp_file(src: PathBuf, cpp: &Arc<CppBinary>, ctx: Arc<JobContext>) -> Job {
    let ctx2 = ctx.clone();
    let job_fn = move |job| {
        let result = || -> anyhow::Result<JobFnResult> {
            // Get toolchain
            let toolchain = &ctx2.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?;
            let root: String = toolchain
                .target
                .get_config_abspath(&ctx2.anubis.root)
                .parent()
                .ok_or_else(|| {
                    anyhow_loc!("Could not determine root for toolchain [{:?}]", toolchain.target)
                })?
                .to_string_lossy()
                .into_owned();

            // Create command
            let mut args: Vec<String> = Default::default();
            //args.push("-v".to_owned()); // verbose
            for flag in &toolchain.cpp.compiler_flags {
                args.push(flag.clone());
            }
            for inc_dir in &toolchain.cpp.system_include_dirs {
                args.push("-isystem".to_owned());
                args.push(format!("{}/{}", &root, inc_dir.to_string_lossy().into_owned()));
            }
            for lib_dir in &toolchain.cpp.library_dirs {
                args.push(format!("-L{}/{}", &root, lib_dir.to_string_lossy().into_owned()));
            }
            for lib in &toolchain.cpp.libraries {
                args.push(format!("-l{}", lib.to_string_lossy().into_owned()));
            }
            args.push("-o".into());
            args.push(".anubis-out/bin/program.exe".into());
            args.push(src.to_string_lossy().into_owned());

            // run the command
            let compiler = format!(
                "{}/{}",
                &root,
                toolchain.cpp.compiler.to_string_lossy().into_owned()
            );
            let output = std::process::Command::new(compiler)
                .args(&args) // optional
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output();

            match output {
                Ok(o) => {
                    if o.status.success() {
                        //Ok(JobFnResult::Success());
                        Ok(JobFnResult::Error(anyhow_loc!("it actually worked")))
                    } else {
                        Ok(JobFnResult::Error(anyhow_loc!("Command completed with error status [{}].\n  Args: [{:#?}\n  stdout: {}\n  stderr: {}", o.status, args, String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
                    }
                }
                Err(e) => Ok(JobFnResult::Error(anyhow_loc!(
                    "Command failed unexpectedly [{}]",
                    e
                ))),
            }
        }();

        match result {
            Ok(r) => r,
            Err(e) => JobFnResult::Error(anyhow_loc!(
                "Failed to compile.\n    Src: [{:?}]    \n  Error: [{:?}]",
                src,
                e
            )),
        }
    };

    ctx.new_job(
        format!("Build CppBinary Target {}", cpp.target.target_path()),
        Box::new(job_fn),
    )
}

// ----------------------------------------------------------------------------
// Public functions
// ----------------------------------------------------------------------------
pub fn register_rule_typeinfos(anubis: Arc<Anubis>) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("cpp_binary".to_owned()),
        parse_rule: parse_cpp_binary,
    })?;

    Ok(())
}
