#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

use crate::papyrus::*;

#[derive(Clone, Debug, Deserialize)]
pub struct CppBinary {
    pub name: String,
    pub srcs: Vec<PathBuf>,
}

impl Rule for CppBinary {
    fn name(&self) -> String {
        self.name.clone()
    }
}

fn parse_cpp_binary(v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let cpp = CppBinary::deserialize(de).map_err(|e| anyhow::anyhow!("{}", e))?;
    Ok(Arc::new(cpp))
}

pub fn register_rule_typeinfos(anubis: &mut Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("cpp_binary".to_owned()),
        parse_rule: parse_cpp_binary,
    })?;

    Ok(())
}

impl crate::papyrus::PapyrusObjectType for CppBinary {
    fn name() -> &'static str {
        &"cpp_binary"
    }
}
