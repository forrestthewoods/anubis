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
use crate::toolchain::Toolchain;
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

#[derive(Debug)]
struct CompileObjectResult {
    pub output_file: PathBuf,
}
impl JobResult for CompileObjectResult {}

#[derive(Debug)]
struct CompileExeResult {
    pub output_file: PathBuf,
}
impl JobResult for CompileExeResult {}

trait CppContextExt<'a> {
    fn get_toolchain(&'a self) -> anyhow::Result<&'a Toolchain>;
    fn get_toolchain_root(&self) -> anyhow::Result<PathBuf>;
    fn get_args(&self) -> anyhow::Result<Vec<String>>;
    fn get_compiler(&self) -> anyhow::Result<PathBuf>;
}

impl<'a> CppContextExt<'a> for Arc<JobContext> {
    fn get_toolchain(&'a self) -> anyhow::Result<&'a Toolchain> {
        Ok(self.toolchain.as_ref().ok_or_else(|| anyhow_loc!("No toolchain specified"))?.as_ref())
    }

    fn get_toolchain_root(&self) -> anyhow::Result<PathBuf> {
        let toolchain = self.get_toolchain()?;
        Ok(toolchain
            .target
            .get_config_abspath(&self.anubis.root)
            .parent()
            .ok_or_else(|| anyhow_loc!("Could not determine root for toolchain [{:?}]", toolchain.target))?
            .to_string_lossy()
            .into_owned()
            .into())
    }

    fn get_args(&self) -> anyhow::Result<Vec<String>> {
        let toolchain = self.get_toolchain()?;
        let root = self.get_toolchain_root()?.to_string_lossy().into_owned();

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
        args.push("-MD".into());
        //args.push("-v".into());
        //args.push("-o".into());
        //args.push(".anubis-out/build/program.exe".into());
        //args.push(src.to_string_lossy().into_owned());

        Ok(args)
    }

    fn get_compiler(&self) -> anyhow::Result<PathBuf> {
        let toolchain = self.get_toolchain()?;
        let root = self.get_toolchain_root()?;
        Ok(format!(
            "{}/{}",
            root.to_string_lossy(),
            toolchain.cpp.compiler.to_string_lossy()
        )
        .into())
    }
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
            // Get args
            let mut args = ctx2.get_args()?;

            // Compute output
            //let src_filename = src.file_name().ok_or_else(|| anyhow_loc!("Could not get filename from [{:?}]", src))?;
            let output_file = ctx2.anubis.root.join(".anubis-out/build").join("program2.exe");
            ensure_directory(&output_file)?;
            args.push("-o".into());
            args.push(output_file.to_string_lossy().into());
            args.push(src.to_string_lossy().into());

            // run the command
            let compiler = ctx2.get_compiler()?;
            let output = std::process::Command::new(compiler)
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output();

            match output {
                Ok(o) => {
                    if o.status.success() {
                        let result = JobFnResult::Success(Arc::new(CompileExeResult { output_file }));

                        //Ok(JobFnResult::Success());
                        //println!("stdout: {}\nstderr:{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
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

fn get_toolchain_root(anubis: &Arc<Anubis>, toolchain: &Arc<Toolchain>) -> anyhow::Result<String> {
    anyhow::bail!("oh no");
}

fn get_compiler(anubis: &Arc<Anubis>, toolchain: &Arc<Toolchain>) -> anyhow::Result<PathBuf> {
    anyhow::bail!("oh no");
}

fn ensure_directory(path: &Path) -> anyhow::Result<()> {
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().ok_or_else(|| anyhow_loc!("Could not get dir from path [{:?}]", path))?
    };
    let _ = std::fs::create_dir_all(dir)?;
    Ok(())
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
