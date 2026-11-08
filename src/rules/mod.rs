//! Build rules for Anubis.
//!
//! This module contains all the build rule implementations:
//! - `cc_rules`: C/C++ compilation rules. Use `cc_binary` and `cc_static_library`
//!   with an explicit `lang` field set to "c" or "cpp" to select the toolchain.
//! - `cc_includes`: "must not exist" headers for cached compiles (shadowing, `__has_include`)
//! - `cmd_rules`: Command rules for running tools (anubis_cmd)
//! - `nasm_rules`: NASM assembly rules (nasm_objects)
//! - `zig_rules`: Zig libc extraction rules for cross-compilation
//! - `archive_rules`: `archive_zip` and `archive_7z`, packaging files for distribution (need no mode)
//! - `rule_utils`: Shared utility functions for rule implementations

pub mod cc_includes;
pub mod cc_rules;
pub mod cmd_rules;
pub mod ifs_rules;
pub mod nasm_rules;
pub mod rule_utils;
pub mod zig_rules;
pub mod archive_rules;
