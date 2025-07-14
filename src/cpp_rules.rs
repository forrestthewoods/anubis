#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis::{self, AnubisTarget, JobCacheKey, RuleExt};
use crate::job_system::*;
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use serde::Deserialize;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;

use crate::papyrus::*;
use crate::toolchain::Toolchain;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};
use serde::{de, Deserializer};

// ----------------------------------------------------------------------------
// Declarations
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Deserialize)]
pub struct CppBinary {
    pub name: String,
    pub srcs: Vec<PathBuf>,

     #[serde(default)]
     pub deps: Vec<AnubisTarget>,

    #[serde(skip_deserializing)]
    target: anubis::AnubisTarget,
}

#[derive(Debug)]
struct LinkArgsResult {
    pub filepath: PathBuf,
}
impl JobResult for LinkArgsResult {}

#[derive(Debug)]
struct CompileExeResult {
    pub output_file: PathBuf,
}
impl JobResult for CompileExeResult {}

enum Substep {
    Id(JobId),
    Job(Job),
}

trait CppContextExt<'a> {
    fn get_toolchain(&'a self) -> anyhow::Result<&'a Toolchain>;
    fn get_toolchain_root(&self) -> anyhow::Result<PathBuf>;
    fn get_args(&self) -> anyhow::Result<Vec<String>>;
    fn get_compiler(&self) -> anyhow::Result<PathBuf>;
}

// ----------------------------------------------------------------------------
// Implementations
// ----------------------------------------------------------------------------
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
        for define in &toolchain.cpp.defines {
            args.push(format!("-D{}", define));
        }

        // Assorted
        args.push("-MD".into()); // generate .d dependencies file
        args.push("-H".into()); // show all includes

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

impl Rule for CppBinary {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        bail_loc_if!(ctx.mode.is_none(), "Can not create CppBinary job without a mode");

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
    let mode = job.ctx.mode.as_ref().unwrap(); // should have been validated previously

    // check cache
    // let job_key = JobCacheKey {
    //     mode: mode.target.clone(),
    //     target: cpp.target.clone(),
    //     substep: None
    // };

    // if let Ok(job_cache) = job.ctx.anubis.job_cache.read() {
    //     if let Some(job_id) = job_cache.get(&job_key) {
    //         if let Some(maybe_result) = job.ctx.job_system.try_get_result(*job_id) {
    //             match maybe_result {
    //                 Ok(result) => {
    //                     return JobFnResult::Success(result)
    //                 },
    //                 Err(e) => {
    //                     return JobFnResult::Error(e)
    //                 }
    //             }
    //         }
    //     }
    // } else {
    //     return JobFnResult::Error(anyhow_loc!("job_cache poisoned"));
    // }

    let mut deferral: JobDeferral = Default::default();
    let mut link_arg_jobs: Vec<JobId> = Default::default();

    // create child job to compile each dep
    for dep in &cpp.deps {
        let rule = job.ctx.anubis.get_rule(dep, &mode);

        // we need to ensure this rule gets built
        // which means either we get its existing job_id
        // or we make sure a new job gets built

        //job.ctx.job_system.

        match rule {
            Ok(rule) => {
                //let x = rule.create_build_job(job.ctx.clone());
                //let dep_job = rule.build(rule.clone(), job.ctx.clone());
            }
            Err(e) => return JobFnResult::Error(e),
        }
    }

    // create child job to compile each src
    for src in &cpp.srcs {
        let substep = build_cpp_file(src.clone(), &cpp, job.ctx.clone());
        match substep {
            Ok(Substep::Job(child_job)) => {
                // Store the new job
                deferral.graph_updates.push(JobGraphEdge {
                    blocked: job.id,
                    blocker: child_job.id,
                });
                link_arg_jobs.push(child_job.id);
                deferral.new_jobs.push(child_job);
            }
            Ok(Substep::Id(child_job_id)) => {
                // Create a dependency on an existing job
                deferral.graph_updates.push(JobGraphEdge {
                    blocked: job.id,
                    blocker: child_job_id,
                });
                link_arg_jobs.push(child_job_id);
            }
            Err(e) => {
                return JobFnResult::Error(anyhow::anyhow!("{}", e));
            }
        }
    }

    // create a job to link all objects from child job into result
    let link_job = move |job: Job| -> JobFnResult {
        // link all object files into an exe
        let link_result = link_exe(&link_arg_jobs, &cpp, job.ctx.clone());
        match link_result {
            Ok(result) => result,
            Err(e) => JobFnResult::Error(anyhow::anyhow!("{}", e)),
        }
    };

    // Update this job to perform link
    job.job_fn = Some(Box::new(link_job));
    deferral.new_jobs.push(job);

    // Defer!
    JobFnResult::Deferred(deferral)
}

fn build_cpp_file(src_path: PathBuf, cpp: &Arc<CppBinary>, ctx: Arc<JobContext>) -> anyhow::Result<Substep> {
    let src = src_path.to_string_lossy().to_string();

    // See if job for (mode, target, compile_$src) already exists
    let job_key = JobCacheKey {
        mode: ctx.mode.as_ref().unwrap().target.clone(),
        target: cpp.target.clone(),
        substep: Some(format!("compile_{}", &src)),
    };

    let mut job_cache = ctx.anubis.job_cache.write().map_err(|e| anyhow_loc!("Lock poisoned: {}", e))?;
    let entry = job_cache.entry(job_key);
    let mut new_job = false;
    let job_id = *entry.or_insert_with(|| {
        new_job = true;
        ctx.get_next_id()
    });
    drop(job_cache);

    if !new_job {
        return Ok(Substep::Id(job_id));
    }

    // Create a new job that builds the file
    let ctx2 = ctx.clone();
    let src2 = src.clone();
    let job_fn = move |job| {
        let result = || -> anyhow::Result<JobFnResult> {
            // Get initial args args
            let mut args = ctx2.get_args()?;

            // Add extra args
            args.push("-c".into()); // compile object file, do not link

            // Compute object output filepath
            let src_dir =
                src_path.parent().ok_or_else(|| anyhow_loc!("No parent dir for [{:?}]", src_path))?;
            let src_filename =
                src_path.file_name().ok_or_else(|| anyhow_loc!("No filename for [{:?}]", src_path))?;
            let reldir = pathdiff::diff_paths(&src_dir, &ctx2.anubis.root).ok_or_else(|| {
                anyhow_loc!(
                    "Could not relpath from [{:?}] to [{:?}]",
                    &ctx2.anubis.root,
                    &src_path
                )
            })?;
            let output_file = ctx2
                .anubis
                .root
                .join(".anubis-out")
                .join(&ctx2.mode.as_ref().unwrap().name)
                .join(reldir)
                .join("build")
                .join(src_filename)
                .with_extension("obj")
                .slash_fix();
            ensure_directory(&output_file)?;

            args.push("-o".into());
            args.push(output_file.to_string_lossy().into());
            args.push(src2.clone());
            args.push("-v".into());

            // run the command
            let compiler = ctx2.get_compiler()?;
            let output = std::process::Command::new(&compiler)
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output();

            match output {
                Ok(o) => {
                    if o.status.success() {
                        Ok(JobFnResult::Success(Arc::new(LinkArgsResult {
                            filepath: output_file,
                        })))
                    } else {
                        Ok(JobFnResult::Error(anyhow_loc!("Command completed with error status [{}].\n  Args: [{:#?}\n  stdout: {}\n  stderr: {}", o.status, args, String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
                    }
                }
                Err(e) => Ok(JobFnResult::Error(anyhow_loc!(
                    "Command failed unexpectedly\n  Proc: [{:?}]\n  Cmd: [{:#?}]\n  Err: [{}]",
                    &compiler,
                    &args,
                    e
                ))),
            }
        }();

        match result {
            Ok(r) => r,
            Err(e) => JobFnResult::Error(anyhow::anyhow!(
                "Failed to compile.\n    Src: [{:?}]    \n  Error: [{:?}]",
                src2,
                e
            )),
        }
    };

    Ok(Substep::Job(ctx.new_job_with_id(
        job_id,
        format!("Compile cpp file [{}]", src),
        Box::new(job_fn),
    )))
}

fn link_exe(
    link_arg_jobs: &[JobId],
    cpp: &Arc<CppBinary>,
    ctx: Arc<JobContext>,
) -> anyhow::Result<JobFnResult> {
    // Get all child jobs
    let mut link_args: Vec<Arc<LinkArgsResult>> = Default::default();
    for link_arg_job in link_arg_jobs {
        let job_result = ctx.job_system.expect_result::<LinkArgsResult>(*link_arg_job)?;
        link_args.push(job_result);
    }

    // Build link command
    let mut args = ctx.get_args()?;

    // Add all object files
    for link_arg in &link_args {
        args.push(link_arg.filepath.to_string_lossy().into());
    }

    args.push("C:/Users/lordc/AppData/Local/zig/o/03bca4392b84606eec3d46f80057cd4e/Scrt1.o".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/55dfa83a4f4b12116e23f4ec9777d4f8/crti.o".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/fad170fd298b8fd8bff1ba805a71756f/libc++abi.a".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/1bf1d8780ed85e68dd8d74d05e544265/libc++.a".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/85b568e3cd646bd03ffc524e8f933c62/libunwind.a".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libm.so.6".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libpthread.so.0".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libc.so.6".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libdl.so.2".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/librt.so.1".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libld.so.2".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libutil.so.1".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/a356bf1e709772429e2479b90bfabc00/libresolv.so.2".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/e244f0af77d6abfb14cbc7be4d094091/libc_nonshared.a".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/d88abd594b039257747920427b18cc0c/libcompiler_rt.a".into());
    args.push("C:/Users/lordc/AppData/Local/zig/o/026418d2b02a504673714dfd597c332d/crtn.o".into());

    // Compute output filepath
    let relpath = cpp.target.get_relative_dir();
    let output_file = ctx
        .anubis
        .root
        .join(".anubis-out")
        .join(&ctx.mode.as_ref().unwrap().name)
        .join(relpath)
        .join("bin")
        .join(&cpp.name)
        .with_extension("exe")
        .slash_fix();
    println!("Linking file {:?}", output_file);
    ensure_directory(&output_file)?;

    args.push("-o".into());
    args.push(output_file.to_string_lossy().into());

    // run the command
    let compiler = ctx.get_compiler()?;
    let output = std::process::Command::new(compiler)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    match output {
        Ok(o) => {
            if o.status.success() {
                Ok(JobFnResult::Success(Arc::new(CompileExeResult { output_file })))
            } else {
                Ok(JobFnResult::Error(anyhow_loc!(
                    "Command completed with error status [{}].\n  Args: [{:#?}\n  stdout: {}\n  stderr: {}",
                    o.status,
                    args,
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )))
            }
        }
        Err(e) => Ok(JobFnResult::Error(anyhow_loc!(
            "Command failed unexpectedly [{}]",
            e
        ))),
    }
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
