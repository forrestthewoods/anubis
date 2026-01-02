//! Build rules for Anubis.
//!
//! This module contains all the build rule implementations:
//! - `cc_rules`: C/C++ compilation rules. Use `cc_binary` and `cc_static_library`
//!   with an explicit `lang` field set to "c" or "cpp" to select the toolchain.
//! - `nasm_rules`: NASM assembly rules (nasm_objects)
//! - `rule_utils`: Shared utility functions for rule implementations

pub mod cc_rules;
pub mod nasm_rules;
pub mod rule_utils;

pub use cc_rules::*;
pub use nasm_rules::*;
