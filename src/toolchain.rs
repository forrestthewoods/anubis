#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis;

use anubis::AnubisTarget;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

// ----------------------------------------------------------------------------
// Structs
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Toolchain {
    pub name: String,
    pub c: CcToolchain,
    pub cpp: CcToolchain,
    pub nasm: NasmToolchain,
    pub zig: ZigToolchain,

    /// Mode target for building build tools (e.g., //mode:win_release).
    /// This mode is used when building tools that run on the build platform,
    /// such as those used by `anubis_cmd` rules during cross-compilation.
    pub build_mode: AnubisTarget,

    #[serde(skip_deserializing)]
    pub target: AnubisTarget,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    pub name: String,
    pub vars: HashMap<String, String>,

    #[serde(skip_deserializing)]
    pub target: AnubisTarget,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct CcToolchain {
    pub compiler: PathBuf,
    pub compiler_flags: Vec<String>,
    pub linker: PathBuf,
    pub linker_flags: Vec<String>,
    pub archiver: PathBuf,
    pub library_dirs: Vec<PathBuf>,
    pub libraries: Vec<PathBuf>,
    pub system_include_dirs: Vec<PathBuf>,
    pub defines: Vec<String>,
    pub exe_deps: Vec<AnubisTarget>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct NasmToolchain {
    pub assembler: PathBuf,
    pub archiver: PathBuf,
    pub output_format: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ZigToolchain {
    pub compiler: PathBuf,
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
