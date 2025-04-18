#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Toolchain {
    cpp: CppToolchain,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CppToolchain {
    compiler: PathBuf,
    compiler_flags: Vec<String>,
    library_dirs: Vec<PathBuf>,
    libraries: Vec<PathBuf>,
    system_include_dirs: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Mode {
    pub name: String,
    pub vars: HashMap<String, String>,
}

impl crate::papyrus::PapyrusObjectType for Toolchain {
    fn name() -> &'static str {
        &"toolchain"
    }
}

impl crate::papyrus::PapyrusObjectType for Mode {
    fn name() -> &'static str {
        &"mode"
    }
}
