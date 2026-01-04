//! Zig-related build rules for extracting libc and runtime libraries.
//!
//! This module provides rules for extracting Zig's bundled libc and runtime
//! libraries for cross-compilation scenarios.

use crate::anubis::{self, AnubisTarget};
use crate::job_system::*;
use crate::rules::rule_utils::ensure_directory;
use crate::util::SlashFix;
use crate::{anubis::RuleTypename, Anubis, Rule, RuleTypeInfo};
use crate::{anyhow_loc, bail_loc, function_name};
use anyhow::Context;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;



/// Represents the collection of libraries extracted from Zig for a specific target.
#[derive(Debug, Clone, Default)]
pub struct ZigLibcArtifacts {
    /// Object files that need to be linked (e.g., Scrt1.o, crti.o, crtn.o)
    pub startup_objects: Vec<PathBuf>,
    /// Static libraries (e.g., libc++.a, libunwind.a, libc_nonshared.a)
    pub static_libs: Vec<PathBuf>,
    /// Shared libraries (e.g., libc.so.6, libm.so.6)
    pub shared_libs: Vec<PathBuf>,
    /// The compiler runtime library (libcompiler_rt.a or similar)
    pub compiler_rt: Option<PathBuf>,
    /// Dynamic linker path (e.g., ld-linux-x86-64.so.2)
    pub dynamic_linker: Option<PathBuf>,
}

/// Configuration for extracting Zig libc artifacts.
#[derive(Debug, Clone)]
pub struct ZigLibcConfig {
    /// Path to the Zig executable
    pub zig_exe: PathBuf,
    /// Target triple (e.g., "x86_64-linux-gnu")
    pub target: String,
    /// Language to use for compilation ("c" or "c++")
    pub lang: String,
    /// Optional glibc version (e.g., "2.28")
    pub glibc_version: Option<String>,
    /// Directory where extracted libraries will be cached
    pub cache_dir: PathBuf,
}

/// Runs Zig with verbose output and parses the linker command to extract library paths.
///
/// This function:
/// 1. Creates a minimal source file appropriate for the language
/// 2. Runs Zig in verbose mode to capture the full linker command
/// 3. Parses the output to find all library paths
/// 4. Copies the libraries to the cache directory
pub fn extract_zig_libc(config: &ZigLibcConfig) -> anyhow::Result<ZigLibcArtifacts> {
    // Create temp directory for the dummy source file
    let temp_dir = config.cache_dir.join("temp");
    std::fs::create_dir_all(&temp_dir)
        .with_context(|| format!("Failed to create temp directory: {:?}", temp_dir))?;

    // Create a minimal source file
    let (src_file, zig_cmd) = if config.lang == "c++" {
        let src = temp_dir.join("dummy.cpp");
        std::fs::write(&src, "int main() { return 0; }\n")
            .with_context(|| format!("Failed to write dummy source: {:?}", src))?;
        (src, "c++")
    } else {
        let src = temp_dir.join("dummy.c");
        std::fs::write(&src, "int main() { return 0; }\n")
            .with_context(|| format!("Failed to write dummy source: {:?}", src))?;
        (src, "cc")
    };

    // Build the Zig command with verbose output
    let output_file = temp_dir.join("dummy.out");
    let mut args = vec![
        zig_cmd.to_string(),
        "-v".to_string(), // verbose mode shows linker command
        "-target".to_string(),
        config.target.clone(),
    ];

    // Add glibc version if specified
    if let Some(ref glibc_ver) = config.glibc_version {
        args.push(format!("-glibc={}", glibc_ver));
    }

    args.push("-o".to_string());
    args.push(output_file.to_string_lossy().to_string());
    args.push(src_file.to_string_lossy().to_string());

    tracing::debug!("Running Zig command: {:?} {:?}", config.zig_exe, args);

    // Run Zig and capture verbose output
    let output = std::process::Command::new(&config.zig_exe)
        .args(&args)
        .output()
        .with_context(|| format!("Failed to run Zig: {:?}", config.zig_exe))?;

    // Zig outputs verbose info to stderr
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    tracing::trace!("Zig stdout: {}", stdout);
    tracing::trace!("Zig stderr: {}", stderr);

    if !output.status.success() {
        bail_loc!(
            "Zig compilation failed with status {}.\nstdout: {}\nstderr: {}",
            output.status,
            stdout,
            stderr
        );
    }

    // Parse the verbose output to extract library paths
    let artifacts = parse_zig_verbose_output(&stderr, &config.cache_dir)?;

    // Clean up temp files
    let _ = std::fs::remove_file(&src_file);
    let _ = std::fs::remove_file(&output_file);

    Ok(artifacts)
}

/// Parses Zig's verbose output to extract library paths.
///
/// Zig's verbose mode outputs the full linker command which includes all the
/// object files and libraries it uses. We parse this to find what we need.
fn parse_zig_verbose_output(stderr: &str, cache_dir: &Path) -> anyhow::Result<ZigLibcArtifacts> {
    let mut artifacts = ZigLibcArtifacts::default();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    // Create output directories
    let libs_dir = cache_dir.join("libs");
    std::fs::create_dir_all(&libs_dir)
        .with_context(|| format!("Failed to create libs directory: {:?}", libs_dir))?;

    // Look for paths in the verbose output
    // Zig typically outputs paths that look like:
    // - C:/Users/.../AppData/Local/zig/o/<hash>/filename
    // - /home/user/.cache/zig/o/<hash>/filename
    for line in stderr.lines() {
        // Skip lines that don't look like they contain paths
        if !line.contains("zig") && !line.contains("/o/") && !line.contains("\\o\\") {
            continue;
        }

        // Parse the line for file paths
        for token in line.split_whitespace() {
            let path = PathBuf::from(token.trim_matches(|c| c == '"' || c == '\''));

            // Skip if not a valid path or already seen
            if !path.exists() || seen_paths.contains(&path) {
                continue;
            }

            // Check if this looks like a Zig cache path
            let path_str = path.to_string_lossy();
            if !path_str.contains("/o/") && !path_str.contains("\\o\\") {
                continue;
            }

            if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
                // Copy the file to our libs directory
                let dest_path = libs_dir.join(filename);
                if !dest_path.exists() {
                    std::fs::copy(&path, &dest_path)
                        .with_context(|| format!("Failed to copy {:?} to {:?}", path, dest_path))?;
                }

                // Categorize the file
                categorize_lib_file(&dest_path, filename, &mut artifacts);
                seen_paths.insert(path);
            }
        }
    }

    // If we didn't find libraries from parsing, try scanning Zig's cache directory
    if artifacts.startup_objects.is_empty() && artifacts.static_libs.is_empty() {
        tracing::debug!("Verbose parsing didn't find libraries, trying cache scan");
        scan_zig_cache_for_artifacts(stderr, &libs_dir, &mut artifacts)?;
    }

    Ok(artifacts)
}

/// Categorizes a library file based on its name.
pub fn categorize_lib_file(path: &Path, filename: &str, artifacts: &mut ZigLibcArtifacts) {
    let path = path.to_path_buf().slash_fix();

    // Startup objects (must be linked in specific order)
    if filename == "Scrt1.o" || filename == "crt1.o" {
        artifacts.startup_objects.insert(0, path);
    } else if filename == "crti.o" {
        // crti.o should come early
        let insert_pos = if artifacts.startup_objects.is_empty() {
            0
        } else {
            1.min(artifacts.startup_objects.len())
        };
        artifacts.startup_objects.insert(insert_pos, path);
    } else if filename == "crtn.o" {
        // crtn.o should come last among startup objects
        artifacts.startup_objects.push(path);
    }
    // Compiler runtime
    else if filename.contains("compiler_rt") || filename.contains("compiler-rt") {
        artifacts.compiler_rt = Some(path);
    }
    // Dynamic linker
    else if filename.starts_with("ld-linux") || filename == "ld.so.2" || filename.contains("ld-linux") {
        artifacts.dynamic_linker = Some(path);
    }
    // Static libraries
    else if filename.ends_with(".a") {
        artifacts.static_libs.push(path);
    }
    // Shared libraries
    else if filename.contains(".so") {
        artifacts.shared_libs.push(path);
    }
    // Object files that aren't startup objects
    else if filename.ends_with(".o") {
        artifacts.startup_objects.push(path);
    }
}

/// Scans Zig's cache directory structure to find artifacts.
///
/// Zig stores compiled artifacts in a cache directory like:
/// ~/.cache/zig/o/<hash>/filename or %LOCALAPPDATA%/zig/o/<hash>/filename
fn scan_zig_cache_for_artifacts(
    stderr: &str,
    libs_dir: &Path,
    artifacts: &mut ZigLibcArtifacts,
) -> anyhow::Result<()> {
    // Try to find cache paths from the stderr output
    let cache_paths: Vec<PathBuf> = stderr
        .lines()
        .flat_map(|line| line.split_whitespace())
        .filter_map(|token| {
            let path = PathBuf::from(token.trim_matches(|c| c == '"' || c == '\''));
            let path_str = path.to_string_lossy();
            if (path_str.contains("/zig/o/") || path_str.contains("\\zig\\o\\")) && path.exists() {
                // Get the parent directory (the hash directory)
                path.parent().map(|p| p.to_path_buf())
            } else {
                None
            }
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    // Scan each cache directory
    for cache_path in cache_paths {
        if !cache_path.exists() || !cache_path.is_dir() {
            continue;
        }

        for entry in std::fs::read_dir(&cache_path)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
                // Only process library files
                if !is_library_file(filename) {
                    continue;
                }

                // Copy to our libs directory
                let dest_path = libs_dir.join(filename);
                if !dest_path.exists() {
                    std::fs::copy(&path, &dest_path)
                        .with_context(|| format!("Failed to copy {:?} to {:?}", path, dest_path))?;
                }

                categorize_lib_file(&dest_path, filename, artifacts);
            }
        }
    }

    Ok(())
}

/// Checks if a filename looks like a library file we care about.
fn is_library_file(filename: &str) -> bool {
    // Object files
    if filename.ends_with(".o") {
        return true;
    }
    // Static libraries
    if filename.ends_with(".a") {
        return true;
    }
    // Shared libraries (various patterns)
    if filename.contains(".so") {
        return true;
    }
    // Dynamic linker
    if filename.starts_with("ld-") && filename.contains(".so") {
        return true;
    }
    false
}

/// Gets all library paths from the artifacts in the correct link order.
///
/// The order is important for static linking:
/// 1. Startup objects (Scrt1.o/crt1.o, crti.o)
/// 2. User code (handled by caller)
/// 3. C++ runtime libraries (libc++.a, libc++abi.a)
/// 4. libunwind
/// 5. libc and related
/// 6. compiler_rt
/// 7. crtn.o (if present)
pub fn get_link_libraries(artifacts: &ZigLibcArtifacts) -> Vec<PathBuf> {
    let mut libs = Vec::new();

    // Add startup objects (already ordered)
    for obj in &artifacts.startup_objects {
        // crtn.o should be last, so skip it here
        if obj.file_name().map(|f| f != "crtn.o").unwrap_or(true) {
            libs.push(obj.clone());
        }
    }

    // Add static libraries in a sensible order
    // First: C++ libraries
    for lib in &artifacts.static_libs {
        if let Some(name) = lib.file_name().and_then(|f| f.to_str()) {
            if name.contains("c++") || name.contains("cxx") {
                libs.push(lib.clone());
            }
        }
    }

    // Then: libunwind
    for lib in &artifacts.static_libs {
        if let Some(name) = lib.file_name().and_then(|f| f.to_str()) {
            if name.contains("unwind") {
                libs.push(lib.clone());
            }
        }
    }

    // Then: shared libraries (libc, libm, etc.)
    libs.extend(artifacts.shared_libs.iter().cloned());

    // Then: remaining static libraries (libc_nonshared, etc.)
    for lib in &artifacts.static_libs {
        if let Some(name) = lib.file_name().and_then(|f| f.to_str()) {
            if !name.contains("c++")
                && !name.contains("cxx")
                && !name.contains("unwind")
                && !name.contains("compiler_rt")
            {
                libs.push(lib.clone());
            }
        }
    }

    // Then: compiler runtime
    if let Some(ref rt) = artifacts.compiler_rt {
        libs.push(rt.clone());
    }

    // Finally: crtn.o if present
    for obj in &artifacts.startup_objects {
        if obj.file_name().map(|f| f == "crtn.o").unwrap_or(false) {
            libs.push(obj.clone());
        }
    }

    libs
}

// ----------------------------------------------------------------------------
// Public Structs
// ----------------------------------------------------------------------------

/// Rule for extracting Zig's libc and runtime libraries for a target platform.
///
/// Example usage in ANUBIS files:
/// ```papyrus
/// zig_libc(
///     name = "linux_libc",
///     target = "x86_64-linux-gnu",
///     lang = "c++",
///     glibc_version = "2.28",  # optional
///     zig_exe = RelPath("zig/0.15.2/bin/windows_x64/zig.exe"),
/// )
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZigLibc {
    /// Name of this rule target
    pub name: String,

    /// Target triple (e.g., "x86_64-linux-gnu", "aarch64-linux-gnu")
    pub target: String,

    /// Language: "c" or "c++" (determines which runtime libraries to extract)
    #[serde(default = "default_lang")]
    pub lang: String,

    /// Optional glibc version to target (e.g., "2.28")
    #[serde(default)]
    pub glibc_version: Option<String>,

    /// Path to the Zig executable
    pub zig_exe: PathBuf,

    #[serde(skip_deserializing)]
    anubis_target: anubis::AnubisTarget,
}

fn default_lang() -> String {
    "c".to_string()
}

/// Artifact produced by the zig_libc rule containing paths to extracted libraries.
#[derive(Debug)]
pub struct ZigLibcArtifact {
    /// All library paths in correct link order
    pub libraries: Vec<PathBuf>,
    /// The extracted artifacts structure
    pub artifacts: ZigLibcArtifacts,
}

// ----------------------------------------------------------------------------
// Trait Implementations
// ----------------------------------------------------------------------------

impl crate::papyrus::PapyrusObjectType for ZigLibc {
    fn name() -> &'static str {
        "zig_libc"
    }
}

impl anubis::Rule for ZigLibc {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn target(&self) -> AnubisTarget {
        self.anubis_target.clone()
    }

    fn build(&self, arc_self: Arc<dyn Rule>, ctx: Arc<JobContext>) -> anyhow::Result<Job> {
        let zig_libc = arc_self
            .clone()
            .downcast_arc::<ZigLibc>()
            .map_err(|_| anyhow_loc!("Failed to downcast rule [{:?}] to ZigLibc", arc_self))?;

        Ok(ctx.new_job(
            format!("Extract Zig libc for {}", self.target),
            Box::new(move |job| build_zig_libc(zig_libc.clone(), job)),
        ))
    }
}

impl JobArtifact for ZigLibcArtifact {}

// ----------------------------------------------------------------------------
// Private Functions
// ----------------------------------------------------------------------------

fn parse_zig_libc(t: AnubisTarget, v: &crate::papyrus::Value) -> anyhow::Result<Arc<dyn Rule>> {
    let de = crate::papyrus_serde::ValueDeserializer::new(v);
    let mut zig_libc = ZigLibc::deserialize(de).map_err(|e| anyhow_loc!("{}", e))?;
    zig_libc.anubis_target = t;
    Ok(Arc::new(zig_libc))
}

fn build_zig_libc(zig_libc: Arc<ZigLibc>, job: Job) -> anyhow::Result<JobOutcome> {
    let mode = job
        .ctx
        .mode
        .as_ref()
        .ok_or_else(|| anyhow_loc!("No mode specified"))?;

    // Create cache directory for extracted libraries
    // Structure: .anubis-build/{mode}/zig_libc/{target}/{lang}/
    let cache_dir = job
        .ctx
        .anubis
        .build_dir(&mode.name)
        .join("zig_libc")
        .join(&zig_libc.target)
        .join(&zig_libc.lang);

    ensure_directory(&cache_dir)?;

    // Check if we already have cached artifacts
    let libs_dir = cache_dir.join("libs");
    let marker_file = cache_dir.join(".extracted");

    if marker_file.exists() && libs_dir.exists() {
        tracing::info!("Using cached Zig libc artifacts from {:?}", libs_dir);
        let artifacts = load_cached_artifacts(&libs_dir)?;
        let libraries = get_link_libraries(&artifacts);

        return Ok(JobOutcome::Success(Arc::new(ZigLibcArtifact {
            libraries,
            artifacts,
        })));
    }

    // Extract Zig libc
    tracing::info!(
        "Extracting Zig libc for target {} (lang: {})",
        zig_libc.target,
        zig_libc.lang
    );

    let config = ZigLibcConfig {
        zig_exe: zig_libc.zig_exe.clone(),
        target: zig_libc.target.clone(),
        lang: zig_libc.lang.clone(),
        glibc_version: zig_libc.glibc_version.clone(),
        cache_dir: cache_dir.clone(),
    };

    let artifacts = extract_zig_libc(&config).with_context(|| {
        format!(
            "Failed to extract Zig libc for target {} with zig at {:?}",
            zig_libc.target, zig_libc.zig_exe
        )
    })?;

    // Get libraries in correct link order
    let libraries = get_link_libraries(&artifacts);

    // Log what we found
    tracing::debug!("Extracted {} startup objects", artifacts.startup_objects.len());
    tracing::debug!("Extracted {} static libs", artifacts.static_libs.len());
    tracing::debug!("Extracted {} shared libs", artifacts.shared_libs.len());
    if artifacts.compiler_rt.is_some() {
        tracing::debug!("Found compiler_rt");
    }

    // Create marker file to indicate successful extraction
    std::fs::write(&marker_file, "").ok();

    Ok(JobOutcome::Success(Arc::new(ZigLibcArtifact {
        libraries,
        artifacts,
    })))
}

/// Loads cached artifacts from the libs directory.
fn load_cached_artifacts(libs_dir: &std::path::Path) -> anyhow::Result<ZigLibcArtifacts> {
    let mut artifacts = ZigLibcArtifacts::default();

    if !libs_dir.exists() {
        return Ok(artifacts);
    }

    for entry in std::fs::read_dir(libs_dir)? {
        let entry = entry?;
        let path = entry.path().slash_fix();

        if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
            categorize_lib_file(&path, filename, &mut artifacts);
        }
    }

    Ok(artifacts)
}

// ----------------------------------------------------------------------------
// Public Functions
// ----------------------------------------------------------------------------

/// Registers zig rule types with Anubis.
pub fn register_rule_typeinfos(anubis: &Anubis) -> anyhow::Result<()> {
    anubis.register_rule_typeinfo(RuleTypeInfo {
        name: RuleTypename("zig_libc".to_owned()),
        parse_rule: parse_zig_libc,
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_categorize_lib_file_startup() {
        let mut artifacts = ZigLibcArtifacts::default();
        let path = PathBuf::from("/tmp/libs/Scrt1.o");

        categorize_lib_file(&path, "Scrt1.o", &mut artifacts);
        assert_eq!(artifacts.startup_objects.len(), 1);
    }

    #[test]
    fn test_categorize_lib_file_static() {
        let mut artifacts = ZigLibcArtifacts::default();
        let path = PathBuf::from("/tmp/libs/libc++.a");

        categorize_lib_file(&path, "libc++.a", &mut artifacts);
        assert_eq!(artifacts.static_libs.len(), 1);
    }

    #[test]
    fn test_categorize_lib_file_shared() {
        let mut artifacts = ZigLibcArtifacts::default();
        let path = PathBuf::from("/tmp/libs/libc.so.6");

        categorize_lib_file(&path, "libc.so.6", &mut artifacts);
        assert_eq!(artifacts.shared_libs.len(), 1);
    }

    #[test]
    fn test_is_library_file() {
        assert!(is_library_file("Scrt1.o"));
        assert!(is_library_file("libc++.a"));
        assert!(is_library_file("libc.so.6"));
        assert!(is_library_file("libm.so.6"));
        assert!(!is_library_file("main.cpp"));
        assert!(!is_library_file("README.md"));
    }
}
