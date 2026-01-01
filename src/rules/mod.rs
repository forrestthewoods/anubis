//! Build rules for Anubis.
//!
//! This module contains all the build rule implementations:
//! - `cc_rules`: C/C++ compilation rules (c_binary, c_static_library, cpp_binary, cpp_static_library)
//! - `nasm_rules`: NASM assembly rules (nasm_objects)
//! - `rule_utils`: Shared utility functions for rule implementations

pub mod cc_rules;
pub mod nasm_rules;
pub mod rule_utils;

pub use cc_rules::*;
pub use nasm_rules::*;
