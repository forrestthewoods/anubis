//! Build rules for Anubis.
//!
//! This module contains all the build rule implementations:
//! - `cc_rules`: C/C++ compilation rules. Use `cc_binary` and `cc_static_library`
//!   with an explicit `lang` field set to "c" or "cpp" to select the toolchain.
//! - `cmd_rules`: Command rules for running tools (anubis_cmd)
//! - `nasm_rules`: NASM assembly rules (nasm_objects)
//! - `zig_rules`: Zig libc extraction rules for cross-compilation
//! - `rule_utils`: Shared utility functions for rule implementations

pub mod cc_rules;
pub mod cmd_rules;
pub mod nasm_rules;
pub mod rule_utils;
pub mod zig_rules;

pub use cc_rules::*;
pub use cmd_rules::*;
pub use nasm_rules::*;
pub use zig_rules::*;
