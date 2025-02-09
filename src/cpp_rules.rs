#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::{Anubis, Rule, RuleTypeInfo};
use serde::Deserialize;
use std::path::PathBuf;

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

fn parse_cpp_binary(v: crate::papyrus::Value) -> anyhow::Result<Box<dyn Rule + 'static>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let cpp = CppBinary::deserialize(de).map_err(|e| anyhow::anyhow!("{}", e))?;
    Ok(Box::new(cpp))
}

pub fn register_rule_typeinfos(anubis: &mut Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: "cpp_binary".to_owned(),
        create_rule: parse_cpp_binary,
    })?;

    Ok(())
}
