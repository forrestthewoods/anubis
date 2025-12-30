#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

// ----------------------------------------------------------------------------
// Structs
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Toolchain {
    pub cpp: CppToolchain,
    pub nasm: NasmToolchain,

    #[serde(skip_deserializing)]
    pub target: anubis::AnubisTarget,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Mode {
    pub name: String,
    pub vars: HashMap<String, String>,

    #[serde(skip_deserializing)]
    pub target: anubis::AnubisTarget,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CppToolchain {
    pub compiler: PathBuf,
    pub compiler_flags: Vec<String>,
    pub archiver: PathBuf,
    pub library_dirs: Vec<PathBuf>,
    pub libraries: Vec<PathBuf>,
    pub system_include_dirs: Vec<PathBuf>,
    pub defines: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct NasmToolchain {
    pub assember: PathBuf,
    pub output_format: String,
}

// ----------------------------------------------------------------------------
// Trait Implementations
// ----------------------------------------------------------------------------
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
