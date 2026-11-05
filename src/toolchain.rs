#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use crate::anubis;

use anubis::AnubisTarget;
use camino::Utf8PathBuf;
use serde::Deserialize;
use std::collections::HashMap;

// ----------------------------------------------------------------------------
// Structs
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Toolchain {
    pub name: String,
    // Each part is optional: a toolchain only needs the languages its targets use.
    // Use the accessors below, which say which part is missing.
    #[serde(default)]
    pub c: Option<CcToolchain>,
    #[serde(default)]
    pub cpp: Option<CcToolchain>,
    #[serde(default)]
    pub nasm: Option<NasmToolchain>,
    #[serde(default)]
    pub zig: Option<ZigToolchain>,

    /// Mode target for building host tools (e.g., //mode:win_release).
    /// This mode is used when building tools that run on the host platform,
    /// such as those used by `anubis_cmd` rules during cross-compilation.
    pub host_mode: AnubisTarget,

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
    pub compiler: Utf8PathBuf,
    pub compiler_flags: Vec<String>,
    pub linker: Utf8PathBuf,
    pub linker_flags: Vec<String>,
    pub archiver: Utf8PathBuf,
    pub library_dirs: Vec<Utf8PathBuf>,
    pub libraries: Vec<Utf8PathBuf>,
    pub system_include_dirs: Vec<Utf8PathBuf>,
    pub defines: Vec<String>,
    pub exe_deps: Vec<AnubisTarget>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct NasmToolchain {
    pub assembler: Utf8PathBuf,
    pub archiver: Utf8PathBuf,
    pub output_format: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ZigToolchain {
    pub compiler: Utf8PathBuf,
}

impl Toolchain {
    fn missing(&self, field: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "Toolchain [{}] has no `{}` configured. Add a `{}` field to the toolchain definition.",
            self.target.target_path(),
            field,
            field
        )
    }

    pub fn c(&self) -> anyhow::Result<&CcToolchain> {
        self.c.as_ref().ok_or_else(|| self.missing("c"))
    }

    pub fn cpp(&self) -> anyhow::Result<&CcToolchain> {
        self.cpp.as_ref().ok_or_else(|| self.missing("cpp"))
    }

    pub fn nasm(&self) -> anyhow::Result<&NasmToolchain> {
        self.nasm.as_ref().ok_or_else(|| self.missing("nasm"))
    }

    pub fn zig(&self) -> anyhow::Result<&ZigToolchain> {
        self.zig.as_ref().ok_or_else(|| self.missing("zig"))
    }
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
