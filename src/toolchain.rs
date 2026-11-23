//! The `toolchain` and `mode` objects. A toolchain names the compilers,
//! archivers, flags and system paths for C, C++, NASM, Zig and Python; every part is
//! optional, and the accessors say which part is missing. A mode is a set of
//! variables (`target_platform`, `build_type`, ...) that `select` reads.

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
    #[serde(default)]
    pub python: Option<PyToolchain>,

    /// Mode target for building tools that run during the build (e.g., //mode:win_release).
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

impl Mode {
    /// The `target_platform` var; modes without one are treated as Windows.
    pub fn target_platform(&self) -> &str {
        self.vars.get("target_platform").map(|s| s.as_str()).unwrap_or("windows")
    }

    /// Object file extension for the target platform: `obj` on Windows,
    /// `o` everywhere else (Linux, macOS), as each platform's tools expect.
    pub fn object_extension(&self) -> &'static str {
        if self.target_platform() == "windows" { "obj" } else { "o" }
    }
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
    /// Windows links find libraries by name in any casing, as they do on
    /// Windows. Set on hosts whose filesystem is case-sensitive; needs a
    /// linker with lld-link's `/vfsoverlay`. See `rules/lib_overlay.rs`.
    pub case_insensitive_library_dirs: bool,
    pub libraries: Vec<Utf8PathBuf>,
    pub system_include_dirs: Vec<Utf8PathBuf>,
    /// Apple framework search dirs (`-iframework`), e.g. the SDK's
    /// `System/Library/Frameworks`. Like `system_include_dirs`, headers under
    /// them pass the declared-headers check.
    pub system_framework_dirs: Vec<Utf8PathBuf>,
    /// Single files the compiler reads on its own, outside any include dir,
    /// that pass the declared-headers check: clang 23 lists the macOS SDK's
    /// `SDKSettings.json` (read for `-isysroot`) in every `.d`.
    pub system_files: Vec<Utf8PathBuf>,
    /// Apple frameworks every link uses (`-framework X`).
    pub frameworks: Vec<String>,
    /// `dsymutil`, for Mach-O targets: when set, every link of an executable
    /// or shared library also writes `<output>.dSYM` (macOS keeps DWARF in the
    /// object files, so without a dSYM the binary is only debuggable while
    /// they exist). Empty means none.
    pub dsymutil: Utf8PathBuf,
    /// `llvm-objcopy`, which removes debug info from ELF files for a
    /// package that asks for it (`py_wheel`'s `strip_symbols`). Empty means
    /// none.
    pub objcopy: Utf8PathBuf,
    /// Extra compiler flags for Objective-C/C++ sources (`.m`, `.mm`),
    /// e.g. `-fobjc-arc`. Come before the target's own flags.
    pub objc_flags: Vec<String>,
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

/// Python: the host interpreter and uv that run Python, plus what
/// `py_extension` needs for the target platform.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct PyToolchain {
    /// Host interpreter (python-build-standalone from Natron).
    pub interpreter: Utf8PathBuf,
    /// Host uv binary (from Natron).
    pub uv: Utf8PathBuf,
    /// Directories `py_wheel` puts on `sys.path` to run hatchling: its
    /// unpacked wheel and those of its dependencies (from Natron).
    pub hatchling: Vec<Utf8PathBuf>,
    /// Target extension module suffix, e.g. `.cpython-312-x86_64-linux-gnu.so`
    /// or `.cp312-win_amd64.pyd` (CPython's `EXT_SUFFIX`).
    pub ext_suffix: String,
    /// Target wheel tag, e.g. `cp312-cp312-manylinux_2_28_x86_64`: the
    /// Python the headers are for, and on Linux the glibc the toolchain
    /// links against.
    pub wheel_tag: String,
    /// Library every extension depends on: Python headers, and on Windows
    /// the `python3XY.lib` import library.
    pub headers: Option<AnubisTarget>,
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

    pub fn python(&self) -> anyhow::Result<&PyToolchain> {
        self.python.as_ref().ok_or_else(|| self.missing("python"))
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
