//! Anubis, a build system for C and C++ projects. `main.rs` is the CLI on
//! top of this library. Start with [`anubis`] (the orchestrator), then
//! [`rules`], [`job_system`] and [`build_cache`].

#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

pub mod anubis;
pub mod build_cache;
pub mod error;
pub mod export_cmake;
pub mod hints;
pub mod install_toolchains;
pub mod job_system;
pub mod logging;
pub mod papyrus;
pub mod papyrus_fmt;
pub mod papyrus_serde;
pub mod progress;
pub mod rules;
pub mod toolchain;
pub mod util;

// Re-export items at the crate root to preserve internal cross-module imports.
pub use anubis::{Anubis, Rule, RuleTypeInfo};
pub use papyrus::{Identifier, UnresolvedInfo, Value};
pub use rules::cc_rules;

#[cfg(test)]
mod anubis_tests;
#[cfg(test)]
mod job_system_tests;
#[cfg(test)]
mod papyrus_tests;
#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod util_tests;
#[cfg(test)]
mod tests;
