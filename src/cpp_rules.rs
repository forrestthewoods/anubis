#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis::{self, AnubisTarget, HackResult};
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

use crate::job_system::*;
use crate::papyrus::*;

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

fn build_cpp_binary(cpp: Arc<CppBinary>, job: Job) -> JobFnResult {
    let mut deferral: JobDeferral = Default::default();

    // create child job to compile each src
    //for src in self

    // create child job to link
    // create job that re-uses job

    JobFnResult::Error(anyhow::anyhow!("oh no"))
}

fn build_cpp_file(ctx: &Arc<JobContext>) -> JobFnResult {
    JobFnResult::Error(anyhow::anyhow!("oh no"))
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
