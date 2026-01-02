//! Build rules for Anubis.
//!
//! This module contains all the build rule implementations:
//! - `cc_rules`: C/C++ compilation rules. Use `c_binary` and `c_static_library`
//!   for C code (uses C toolchain), and `cpp_binary` and `cpp_static_library`
//!   for C++ code (uses C++ toolchain). Internally these share the same
//!   `CcBinary` and `CcStaticLibrary` types with the language injected at parse time.
//! - `nasm_rules`: NASM assembly rules (nasm_objects)
//! - `rule_utils`: Shared utility functions for rule implementations

pub mod cc_rules;
pub mod nasm_rules;
pub mod rule_utils;

pub use cc_rules::*;
pub use nasm_rules::*;
