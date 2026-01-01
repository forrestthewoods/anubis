//! Build rules for Anubis.
//!
//! This module contains all the build rule implementations:
//! - `cc_rules`: Unified C/C++ compilation rules (cc_binary, cc_static_library)
//!   Language is detected from file extension: .c files use C toolchain,
//!   .cpp/.cc/.cxx files use C++ toolchain. Legacy rule names (cpp_binary,
//!   c_binary, etc.) are also supported for backward compatibility.
//! - `nasm_rules`: NASM assembly rules (nasm_objects)
//! - `rule_utils`: Shared utility functions for rule implementations

pub mod cc_rules;
pub mod nasm_rules;
pub mod rule_utils;

pub use cc_rules::*;
pub use nasm_rules::*;
