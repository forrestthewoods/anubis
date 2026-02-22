#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

pub mod anubis;
pub mod error;
pub mod fs_tree_hasher;
pub mod install_toolchains;
pub mod job_system;
pub mod logging;
pub mod papyrus;
pub mod papyrus_serde;
pub mod progress;
pub mod rules;
pub mod toolchain;
pub mod toolchain_db;
pub mod util;

// Re-export items at the crate root to preserve internal cross-module imports.
// (When this was a binary crate, main.rs had wildcard use statements that made
// these available as `crate::ItemName` for all submodules.)
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
