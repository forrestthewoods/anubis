use crate::papyrus::{self, PapyrusObjectType};
use crate::{anyhow_loc, bail_loc, function_name};
use clap::Parser;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use tar::Archive;
use xz2::read::XzDecoder;
use zip::ZipArchive;

use crate::toolchain_db::{GlobalToolchainDb, ProjectToolchainDb};
use crate::util::{
    create_directory_symlink, get_global_db_path, get_global_temp_dir, get_global_toolchains_dir, is_symlink,
    read_symlink_target,
};

// ----------------------------------------------------------------------------
// Toolchain Installation Configuration
// ----------------------------------------------------------------------------

/// Configuration for the install_toolchains rule in toolchains/ANUBIS.
/// This allows per-project specification of exact toolchain versions.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct InstallToolchains {
    pub llvm: Option<LlvmConfig>,
    pub zig: Option<ZigConfig>,
    pub nasm: Option<NasmConfig>,
    pub msvc: Option<MsvcConfig>,
}

impl PapyrusObjectType for InstallToolchains {
    fn name() -> &'static str {
        "install_toolchains"
    }
}

/// Configuration for LLVM/Clang toolchain
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct LlvmConfig {
    /// LLVM version (e.g., "21.1.6")
    pub version: String,
    /// Platform identifier for download (e.g., "x86_64-pc-windows-msvc")
    pub platform: String,
}

impl PapyrusObjectType for LlvmConfig {
    fn name() -> &'static str {
        "LlvmConfig"
    }
}

/// Configuration for Zig toolchain
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ZigConfig {
    /// Zig version (e.g., "0.15.2")
    pub version: String,
    /// Platform identifier for download (e.g., "x86_64-windows")
    pub platform: String,
}

impl PapyrusObjectType for ZigConfig {
    fn name() -> &'static str {
        "ZigConfig"
    }
}

/// Configuration for NASM assembler
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct NasmConfig {
    /// NASM version (e.g., "3.01")
    pub version: String,
    /// Platform identifier (e.g., "win64")
    pub platform: String,
}

impl PapyrusObjectType for NasmConfig {
    fn name() -> &'static str {
        "NasmConfig"
    }
}

/// Configuration for MSVC and Windows SDK
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct MsvcConfig {
    /// Visual Studio channel version for manifest download (e.g., "18" for vs/18/stable)
    pub vs_channel: String,
    /// Optional: specific MSVC version to install (e.g., "14.50.35717")
    /// If not specified, the latest version will be used
    pub msvc_version: Option<String>,
    /// Optional: specific Windows SDK version to install (e.g., "26100")
    /// If not specified, the latest version will be used
    pub sdk_version: Option<String>,
}

impl PapyrusObjectType for MsvcConfig {
    fn name() -> &'static str {
        "MsvcConfig"
    }
}

// Default values for toolchain versions
mod defaults {
    pub const ZIG_VERSION: &str = "0.15.2";
    pub const ZIG_PLATFORM: &str = "x86_64-windows";

    pub const LLVM_VERSION: &str = "21.1.6";
    pub const LLVM_PLATFORM: &str = "x86_64-pc-windows-msvc";

    pub const NASM_VERSION: &str = "3.01";
    pub const NASM_PLATFORM: &str = "win64";

    pub const MSVC_VS_CHANNEL: &str = "18";
}

/// Load the install_toolchains configuration from toolchains/ANUBIS if it exists.
fn load_install_toolchains_config(project_root: &Path) -> anyhow::Result<InstallToolchains> {
    let config_path = project_root.join("toolchains").join("ANUBIS");

    tracing::info!("Loading toolchain versions from {}", config_path.display());
    let config = papyrus::read_papyrus_file(&config_path)?;

    // Try to find an install_toolchains rule
    let result = config.deserialize_single_object::<InstallToolchains>()?;
    Ok(result)
}

#[derive(Debug, Parser)]
pub struct InstallToolchainsArgs {
    /// Keep downloaded files and reuse them if present (default: true)
    #[arg(long, default_value_t = true)]
    pub keep_downloads: bool,

    /// Discover which MSVC packages contain which files by installing ALL packages
    /// and tracking their contents. Outputs a report to .anubis-temp/msvc_package_contents.txt
    #[arg(long)]
    pub discover_msvc_packages: bool,
}

pub fn install_toolchains(args: &InstallToolchainsArgs) -> anyhow::Result<()> {
    // Flush DNS cache on Windows to avoid DNS issues
    #[cfg(windows)]
    {
        use std::process::Command;
        tracing::info!("Flushing DNS cache on Windows");
        let output = Command::new("ipconfig").arg("/flushdns").output();

        match output {
            Ok(result) => {
                if !result.status.success() {
                    tracing::warn!("Failed to flush DNS cache, continuing anyway");
                }
            }
            Err(e) => {
                tracing::warn!("Failed to run ipconfig /flushdns: {}, continuing anyway", e);
            }
        }
    }

    let project_root = env::current_dir()?;

    // Load toolchain configuration from toolchains/ANUBIS (if present)
    let config = load_install_toolchains_config(&project_root)?;

    // Initialize global storage directory
    let global_toolchains_dir = get_global_toolchains_dir();
    fs::create_dir_all(&global_toolchains_dir)?;
    tracing::info!("Global toolchain storage: {}", global_toolchains_dir);

    // Open the global toolchain database
    let global_db_path = get_global_db_path();
    tracing::info!(
        "Opening global toolchain database at {}",
        global_db_path
    );
    let global_db = GlobalToolchainDb::open(global_db_path.as_std_path())?;

    // Open/create the project toolchain database
    let project_db_path = project_root.join("toolchains").join(".anubis_db");
    tracing::info!(
        "Opening project toolchain database at {}",
        project_db_path.display()
    );
    let project_db = ProjectToolchainDb::open(&project_db_path)?;

    // Use global temp directory for downloads (shared across projects)
    let temp_dir = get_global_temp_dir();
    if !args.keep_downloads && temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)?;
    }
    fs::create_dir_all(&temp_dir)?;

    // If discovery mode, only run MSVC discovery and exit
    if args.discover_msvc_packages {
        discover_msvc_packages(&project_root, temp_dir.as_std_path(), args, config.msvc.as_ref())?;
        return Ok(());
    }

    // Install all toolchains (downloads to global, symlinks to project)
    install_zig(
        &project_root,
        temp_dir.as_std_path(),
        &global_db,
        &project_db,
        args,
        config.zig.as_ref(),
    )?;
    install_llvm(
        &project_root,
        temp_dir.as_std_path(),
        &global_db,
        &project_db,
        args,
        config.llvm.as_ref(),
    )?;
    install_nasm(
        &project_root,
        temp_dir.as_std_path(),
        &global_db,
        &project_db,
        args,
        config.nasm.as_ref(),
    )?;
    install_msvc(
        &project_root,
        temp_dir.as_std_path(),
        &global_db,
        &project_db,
        args,
        config.msvc.as_ref(),
    )?;

    // Cleanup temp directory unless keeping downloads
    if !args.keep_downloads {
        if let Err(e) = fs::remove_dir_all(&temp_dir) {
            tracing::warn!("Failed to cleanup temp directory: {}", e);
        }
    } else {
        tracing::info!("Keeping downloads at {}", temp_dir);
    }

    Ok(())
}

/// Discover which MSVC packages contain which files by installing ALL packages
/// and tracking their contents. Outputs a report to .anubis-temp/msvc_package_contents.txt
fn discover_msvc_packages(
    cwd: &Path,
    temp_dir: &Path,
    args: &InstallToolchainsArgs,
    config: Option<&MsvcConfig>,
) -> anyhow::Result<()> {
    tracing::info!("=== MSVC Package Discovery Mode ===");
    tracing::info!("This will download and extract ALL MSVC packages to discover their contents.");

    // Determine VS channel from config or use default
    let vs_channel = config
        .and_then(|c| {
            if c.vs_channel.is_empty() {
                None
            } else {
                Some(c.vs_channel.as_str())
            }
        })
        .unwrap_or(defaults::MSVC_VS_CHANNEL);

    // Download VS manifest
    let manifest_url = format!("https://aka.ms/vs/{}/stable/channel", vs_channel);
    tracing::info!("Downloading Visual Studio manifest from {}", manifest_url);
    let mut response =
        ureq::get(&manifest_url).call().map_err(|e| anyhow_loc!("Failed to download VS manifest: {}", e))?;
    let channel_manifest: JsonValue = response.body_mut().read_json()?;

    // Find the channelItems and get the VS manifest URL
    let channel_items = channel_manifest
        .get("channelItems")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow_loc!("No channelItems in manifest"))?;

    let vs_manifest_item = channel_items
        .iter()
        .find(|item| {
            item.get("type").and_then(|t| t.as_str()) == Some("Manifest")
                && item.get("id").and_then(|id| id.as_str())
                    == Some("Microsoft.VisualStudio.Manifests.VisualStudio")
        })
        .ok_or_else(|| anyhow_loc!("Could not find VS manifest item"))?;

    let vs_manifest_url = vs_manifest_item
        .get("payloads")
        .and_then(|p| p.as_array())
        .and_then(|arr| arr.first())
        .and_then(|p| p.get("url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow_loc!("Could not find VS manifest URL"))?;

    tracing::info!("Downloading VS manifest from {}", vs_manifest_url);
    let mut vs_response = ureq::get(vs_manifest_url)
        .call()
        .map_err(|e| anyhow_loc!("Failed to download VS manifest payload: {}", e))?;
    // VS manifest can be ~17MB, increase limit from default 10MB
    let vs_manifest: JsonValue = vs_response.body_mut().with_config().limit(30 * 1024 * 1024).read_json()?;

    // Get packages
    let packages = vs_manifest
        .get("packages")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow_loc!("No packages in VS manifest"))?;

    // Find latest MSVC version first
    const HOST: &str = "x64";
    const TARGET: &str = "x64";

    let mut msvc_candidates = Vec::new();
    for package in packages {
        let id = package.get("id").and_then(|i| i.as_str()).unwrap_or("");
        let id_lower = id.to_lowercase();

        if id_lower.starts_with("microsoft.vc.")
            && id_lower.contains(&format!(".tools.host{}.target{}.base", HOST, TARGET))
            && !id_lower.contains(".premium.")
        {
            if let Some(vc_pos) = id_lower.find("microsoft.vc.") {
                if let Some(tools_pos) = id_lower.find(".tools.") {
                    let version_start = vc_pos + "microsoft.vc.".len();
                    let version = &id[version_start..tools_pos];
                    msvc_candidates.push((version.to_string(), id.to_string()));
                }
            }
        }
    }

    if msvc_candidates.is_empty() {
        bail_loc!("Could not find any MSVC compiler packages");
    }

    msvc_candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let (msvc_ver, _) = &msvc_candidates[0];
    tracing::info!("Using MSVC version: {}", msvc_ver);

    // Find ALL packages that match this MSVC version
    // Use a HashSet to deduplicate by package ID (there can be multiple language variants)
    let version_prefix = format!("microsoft.vc.{}", msvc_ver.to_lowercase());
    let mut seen_package_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut all_msvc_packages: Vec<(&str, Vec<(String, String, String)>)> = Vec::new();

    for package in packages {
        let id = package.get("id").and_then(|i| i.as_str()).unwrap_or("");
        let id_lower = id.to_lowercase();

        // Match all packages for this MSVC version
        if id_lower.starts_with(&version_prefix) {
            // Skip if we've already seen this package ID (language variants)
            if seen_package_ids.contains(&id_lower) {
                continue;
            }

            // Skip non-English language packs (they have a "language" field)
            // We only want neutral packages or English ones
            if let Some(lang) = package.get("language").and_then(|l| l.as_str()) {
                if lang != "en-US" && lang != "neutral" {
                    continue;
                }
            }

            let mut payloads = Vec::new();
            if let Some(payload_array) = package.get("payloads").and_then(|p| p.as_array()) {
                for payload in payload_array {
                    if let (Some(url), Some(sha256), Some(filename)) = (
                        payload.get("url").and_then(|u| u.as_str()),
                        payload.get("sha256").and_then(|s| s.as_str()),
                        payload.get("fileName").and_then(|f| f.as_str()),
                    ) {
                        payloads.push((url.to_string(), sha256.to_string(), filename.to_string()));
                    }
                }
            }
            if !payloads.is_empty() {
                seen_package_ids.insert(id_lower);
                all_msvc_packages.push((id, payloads));
            }
        }
    }

    tracing::info!(
        "Found {} MSVC packages for version {}",
        all_msvc_packages.len(),
        msvc_ver
    );

    // Create discovery output directory
    let discovery_dir = temp_dir.join("msvc_discovery");
    if discovery_dir.exists() {
        fs::remove_dir_all(&discovery_dir)?;
    }
    fs::create_dir_all(&discovery_dir)?;

    // Track package contents: package_id -> list of files
    let mut package_contents: HashMap<String, Vec<String>> = HashMap::new();

    // Process each package
    for (package_id, payloads) in &all_msvc_packages {
        tracing::info!("Processing package: {}", package_id);

        // Create a temp dir for this package
        let pkg_extract_dir = discovery_dir.join(package_id.replace(".", "_"));
        fs::create_dir_all(&pkg_extract_dir)?;

        let mut pkg_files = Vec::new();

        for (url, sha256, filename) in payloads {
            let file_path = temp_dir.join(filename);

            // Download if needed
            if args.keep_downloads && file_path.exists() {
                if !verify_sha256(&file_path, sha256)? {
                    download_with_sha256(url, &file_path, sha256)?;
                }
            } else if !file_path.exists() {
                download_with_sha256(url, &file_path, sha256)?;
            }

            // Extract and track files
            if filename.ends_with(".vsix") || filename.ends_with(".zip") {
                let files = extract_vsix_zip_with_tracking(&file_path, &pkg_extract_dir)?;
                pkg_files.extend(files);
            } else if filename.ends_with(".msi") {
                #[cfg(windows)]
                {
                    let files = extract_msi_with_tracking(&file_path, &pkg_extract_dir)?;
                    pkg_files.extend(files);
                }
            }
        }

        package_contents.insert(package_id.to_string(), pkg_files);
    }

    // Generate report
    let report_path = temp_dir.join("msvc_package_contents.txt");
    let mut report = File::create(&report_path)?;

    writeln!(report, "MSVC Package Contents Report")?;
    writeln!(report, "MSVC Version: {}", msvc_ver)?;
    writeln!(report, "Generated: {:?}", SystemTime::now())?;
    writeln!(report, "=")?;
    writeln!(report)?;

    // Sort packages alphabetically
    let mut sorted_packages: Vec<_> = package_contents.iter().collect();
    sorted_packages.sort_by_key(|(k, _)| k.as_str());

    for (package_id, files) in &sorted_packages {
        writeln!(report, "=== {} ===", package_id)?;
        writeln!(report, "  Files: {}", files.len())?;

        // Sort files and show them
        let mut sorted_files: Vec<_> = files.iter().collect();
        sorted_files.sort();
        for file in sorted_files {
            writeln!(report, "    {}", file)?;
        }
        writeln!(report)?;
    }

    // Also create a reverse index: file -> package
    writeln!(report)?;
    writeln!(report, "=== REVERSE INDEX (file -> package) ===")?;
    writeln!(report)?;

    let mut file_to_package: HashMap<String, String> = HashMap::new();
    for (package_id, files) in &package_contents {
        for file in files {
            // Just use the filename for the index
            if let Some(filename) = Path::new(file).file_name() {
                let filename_str = filename.to_string_lossy().to_string();
                file_to_package.insert(filename_str, package_id.clone());
            }
        }
    }

    let mut sorted_files: Vec<_> = file_to_package.iter().collect();
    sorted_files.sort_by_key(|(k, _)| k.as_str());

    for (filename, package_id) in &sorted_files {
        writeln!(report, "{} -> {}", filename, package_id)?;
    }

    tracing::info!("Report written to: {}", report_path.display());

    // Search for specific file if requested (always search for oldnames.lib)
    let search_files = ["oldnames.lib", "libcmt.lib", "msvcrt.lib"];
    tracing::info!("Searching for specific files...");

    for search_file in &search_files {
        if let Some(package_id) = file_to_package.get(*search_file) {
            tracing::info!("  {} found in package: {}", search_file, package_id);
        } else {
            tracing::warn!("  {} NOT FOUND in any package", search_file);
        }
    }

    Ok(())
}

/// Extract VSIX/ZIP and return list of extracted files
fn extract_vsix_zip_with_tracking(archive_path: &Path, destination: &Path) -> anyhow::Result<Vec<String>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut extracted_files = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let entry_name = entry.name().to_string();

        // VSIX packages have contents in "Contents/" folder
        if let Some(relative_path) = entry_name.strip_prefix("Contents/") {
            let out_path = destination.join(relative_path);

            if entry.is_dir() {
                fs::create_dir_all(&out_path)?;
                continue;
            }

            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }

            let mut outfile = File::create(&out_path)?;
            std::io::copy(&mut entry, &mut outfile)?;

            extracted_files.push(relative_path.to_string());
        }
    }

    Ok(extracted_files)
}

/// Extract MSI and return list of extracted files
#[cfg(windows)]
fn extract_msi_with_tracking(msi_path: &Path, destination: &Path) -> anyhow::Result<Vec<String>> {
    // Collect files before extraction
    let mut files_before = std::collections::HashSet::new();
    collect_all_files(destination, &mut files_before);

    // Get absolute path without using canonicalize
    let abs_dest = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        env::current_dir()?.join(destination)
    };

    let dest_str = format!("{}\\", abs_dest.display());
    let log_path = abs_dest.join("msi_install.log");

    let output = Command::new("msiexec.exe")
        .arg("/a")
        .arg(msi_path.as_os_str())
        .arg("/qn")
        .arg(format!("TARGETDIR={}", dest_str))
        .arg("/L*V")
        .arg(&log_path)
        .output()
        .map_err(|e| anyhow_loc!("Failed to run msiexec: {}", e))?;

    if !output.status.success() {
        let log_contents =
            fs::read_to_string(&log_path).unwrap_or_else(|_| "Could not read log file".to_string());
        let log_tail: String = log_contents
            .lines()
            .rev()
            .take(50)
            .collect::<Vec<&str>>()
            .into_iter()
            .rev()
            .collect::<Vec<&str>>()
            .join("\n");

        bail_loc!(
            "msiexec failed with status: {}\nstdout: {}\nstderr: {}\nLog (last 50 lines):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            log_tail
        );
    }

    // Collect files after extraction
    let mut files_after = std::collections::HashSet::new();
    collect_all_files(destination, &mut files_after);

    // Find newly added files
    let new_files: Vec<String> = files_after
        .difference(&files_before)
        .map(|s| {
            // Make path relative to destination
            s.strip_prefix(&abs_dest.to_string_lossy().to_string())
                .unwrap_or(s)
                .trim_start_matches('\\')
                .trim_start_matches('/')
                .to_string()
        })
        .collect();

    Ok(new_files)
}

fn install_zig(
    project_root: &Path,
    temp_dir: &Path,
    global_db: &GlobalToolchainDb,
    project_db: &ProjectToolchainDb,
    args: &InstallToolchainsArgs,
    config: Option<&ZigConfig>,
) -> anyhow::Result<()> {
    // Use config values if provided, otherwise use defaults
    let zig_version = config
        .and_then(|c| {
            if c.version.is_empty() {
                None
            } else {
                Some(c.version.as_str())
            }
        })
        .unwrap_or(defaults::ZIG_VERSION);
    let zig_platform = config
        .and_then(|c| {
            if c.platform.is_empty() {
                None
            } else {
                Some(c.platform.as_str())
            }
        })
        .unwrap_or(defaults::ZIG_PLATFORM);

    const INDEX_URL: &str = "https://ziglang.org/download/index.json";

    tracing::info!("Installing Zig toolchain {} for {}", zig_version, zig_platform);

    // Compute paths
    // Global: ~/.anubis/toolchains/zig/{version}-{platform}/{version}/...
    // Project symlink: toolchains/zig -> global path
    let global_zig_dir =
        get_global_toolchains_dir().join("zig").join(format!("{}-{}", zig_version, zig_platform));
    let symlink_path = project_root.join("toolchains").join("zig");

    // Fast path: check if symlink is already correct
    if project_db.is_symlink_current("zig", zig_version, zig_platform)? {
        if is_symlink(&symlink_path) {
            if let Some(target) = read_symlink_target(&symlink_path) {
                if target == global_zig_dir && global_zig_dir.exists() {
                    tracing::info!("Zig {} symlink is up-to-date, skipping", zig_version);
                    return Ok(());
                }
            }
        }
    }

    // Download and parse the Zig index to get SHA256
    tracing::info!("Downloading Zig release index from {}", INDEX_URL);
    let mut response =
        ureq::get(INDEX_URL).call().map_err(|e| anyhow_loc!("Failed to download Zig index: {}", e))?;
    let index: JsonValue = response.body_mut().read_json()?;

    // Get the download URL and SHA256 hash for the specified version and platform
    let version_info = index
        .get(zig_version)
        .and_then(|v| v.get(zig_platform))
        .ok_or_else(|| anyhow_loc!("No download found for Zig {} {}", zig_version, zig_platform))?;

    let tarball_url = version_info
        .get("tarball")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow_loc!("No tarball URL found"))?;

    let tarball_sha256 = version_info
        .get("shasum")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow_loc!("No SHA256 hash found"))?;

    tracing::info!("Found download URL: {}", tarball_url);

    // Check if already installed globally with this hash
    let is_globally_installed =
        global_db.is_installed("zig", zig_version, zig_platform, tarball_sha256)? && global_zig_dir.exists();

    if is_globally_installed {
        tracing::info!(
            "Zig {} is already installed globally, creating symlink only",
            zig_version
        );
    } else {
        // Need to download and install globally
        tracing::info!("Installing Zig {} to global storage", zig_version);

        // Extract filename from URL (e.g., zig-windows-x86_64-0.15.2.zip)
        let archive_filename = tarball_url
            .split('/')
            .last()
            .ok_or_else(|| anyhow_loc!("Invalid tarball URL: {}", tarball_url))?;
        let archive_path = temp_dir.join(archive_filename);

        // Download archive if not present or if we're not reusing
        if !args.keep_downloads || !archive_path.exists() {
            download_to_path(tarball_url, &archive_path)?;
        } else {
            tracing::info!("Reusing existing download at {}", archive_path.display());
        }

        // Extract to temp directory
        tracing::info!("Extracting archive...");
        let extract_dir = temp_dir.join("zig_extract");
        if extract_dir.exists() {
            fs::remove_dir_all(&extract_dir)?;
        }
        fs::create_dir_all(&extract_dir)?;
        extract_zip(&archive_path, &extract_dir)?;

        // Find the extracted directory (e.g., zig-windows-x86_64-0.15.2)
        let extracted_dir = fs::read_dir(&extract_dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.is_dir())
            .ok_or_else(|| anyhow_loc!("Could not find extracted Zig directory in temp folder"))?;

        tracing::info!("Found extracted directory: {}", extracted_dir.display());

        // Setup global target directory: ~/.anubis/toolchains/zig/{version}-{platform}/{version}
        // The extra {version} level preserves the path structure expected by toolchains/ANUBIS
        let zig_root = global_zig_dir.join(zig_version);

        if global_zig_dir.exists() {
            tracing::info!(
                "Removing existing global installation at {}",
                global_zig_dir
            );
            fs::remove_dir_all(&global_zig_dir)?;
        }
        fs::create_dir_all(&zig_root)?;

        // Move shared files (lib, etc.) to zig_root
        tracing::info!("Installing shared files to {}", zig_root);
        for entry in fs::read_dir(&extracted_dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let file_name = entry.file_name();

            // Skip zig.exe - we'll handle it separately
            if file_name == "zig.exe" {
                continue;
            }

            let file_name_str = file_name.to_string_lossy();
            let target_path = zig_root.join(file_name_str.as_ref());
            if entry_path.is_dir() {
                copy_dir_recursive(&entry_path, target_path.as_std_path())?;
            } else {
                fs::copy(&entry_path, &target_path)?;
            }
        }

        // Move zig.exe to bin/windows_x64/zig.exe
        let zig_exe_source = extracted_dir.join("zig.exe");
        if !zig_exe_source.exists() {
            bail_loc!("Could not find zig.exe in extracted archive");
        }

        let bin_dir = zig_root.join("bin").join("windows_x64");
        fs::create_dir_all(&bin_dir)?;
        let zig_exe_dest = bin_dir.join("zig.exe");

        tracing::info!("Installing zig.exe to {}", zig_exe_dest);
        fs::copy(&zig_exe_source, &zig_exe_dest)?;

        // Record installation in global database
        let install_path_str = global_zig_dir.to_string();
        global_db.record_installation(
            "zig",
            zig_version,
            zig_platform,
            tarball_sha256,
            &install_path_str,
        )?;

        // Mark all files as read-only to prevent accidental modification
        tracing::info!("Setting Zig toolchain files as read-only");
        set_readonly_recursive(global_zig_dir.as_std_path())?;

        tracing::info!(
            "Successfully installed Zig toolchain globally at {}",
            global_zig_dir
        );
    }

    // Create symlink in project: toolchains/zig -> global_zig_dir
    tracing::info!(
        "Creating symlink: {} -> {}",
        symlink_path.display(),
        global_zig_dir
    );
    create_directory_symlink(global_zig_dir.as_std_path(), &symlink_path)?;

    // Record symlink in project database
    let target_path_str = global_zig_dir.to_string();
    project_db.record_symlink("zig", "zig", zig_version, zig_platform, &target_path_str)?;

    tracing::info!(
        "Successfully linked Zig toolchain: {} -> {}",
        symlink_path.display(),
        global_zig_dir
    );
    Ok(())
}

fn install_llvm(
    project_root: &Path,
    temp_dir: &Path,
    global_db: &GlobalToolchainDb,
    project_db: &ProjectToolchainDb,
    args: &InstallToolchainsArgs,
    config: Option<&LlvmConfig>,
) -> anyhow::Result<()> {
    // Use config values if provided, otherwise use defaults
    let llvm_version = config
        .and_then(|c| {
            if c.version.is_empty() {
                None
            } else {
                Some(c.version.as_str())
            }
        })
        .unwrap_or(defaults::LLVM_VERSION);
    let llvm_platform = config
        .and_then(|c| {
            if c.platform.is_empty() {
                None
            } else {
                Some(c.platform.as_str())
            }
        })
        .unwrap_or(defaults::LLVM_PLATFORM);

    let llvm_release_name = format!("LLVM {}", llvm_version);
    let llvm_platform_suffix = format!("{}.tar.xz", llvm_platform);

    const RELEASES_URL: &str = "https://api.github.com/repos/llvm/llvm-project/releases";

    tracing::info!("Installing LLVM toolchain {}", llvm_release_name);

    // Compute paths
    // Global: ~/.anubis/toolchains/llvm/{version}-{platform}/{platform}/...
    // Project symlink: toolchains/llvm -> global path
    let global_llvm_dir =
        get_global_toolchains_dir().join("llvm").join(format!("{}-{}", llvm_version, llvm_platform));
    let symlink_path = project_root.join("toolchains").join("llvm");

    // Fast path: check if symlink is already correct
    if project_db.is_symlink_current("llvm", llvm_version, llvm_platform)? {
        if is_symlink(&symlink_path) {
            if let Some(target) = read_symlink_target(&symlink_path) {
                if target == global_llvm_dir && global_llvm_dir.exists() {
                    tracing::info!("LLVM {} symlink is up-to-date, skipping", llvm_version);
                    return Ok(());
                }
            }
        }
    }

    // Download and parse GitHub releases
    tracing::info!("Downloading LLVM release index from {}", RELEASES_URL);
    let mut response =
        ureq::get(RELEASES_URL).call().map_err(|e| anyhow_loc!("Failed to download LLVM releases: {}", e))?;
    let releases: Vec<JsonValue> = response.body_mut().read_json()?;

    // Find the release with the specified name
    let release = releases
        .iter()
        .find(|r| r.get("name").and_then(|n| n.as_str()) == Some(&llvm_release_name))
        .ok_or_else(|| anyhow_loc!("Could not find release '{}'", llvm_release_name))?;

    // Find the asset with the platform-specific suffix
    let assets = release
        .get("assets")
        .and_then(|a| a.as_array())
        .ok_or_else(|| anyhow_loc!("Release has no assets array"))?;

    let asset = assets
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(|n| n.as_str())
                .map(|name| name.ends_with(&llvm_platform_suffix))
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow_loc!("Could not find asset ending with '{}'", llvm_platform_suffix))?;

    let download_url = asset
        .get("browser_download_url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow_loc!("Asset has no browser_download_url"))?;

    let asset_name =
        asset.get("name").and_then(|n| n.as_str()).ok_or_else(|| anyhow_loc!("Asset has no name"))?;

    tracing::info!("Found LLVM download: {}", download_url);

    // Download archive if not present
    let archive_path = temp_dir.join(asset_name);
    if !args.keep_downloads || !archive_path.exists() {
        download_to_path(download_url, &archive_path)?;
    } else {
        tracing::info!("Reusing existing download at {}", archive_path.display());
    }

    // Compute SHA256 of the archive
    tracing::info!("Computing SHA256 hash of downloaded archive...");
    let archive_sha256 = compute_file_sha256(&archive_path)?;

    // Check if already installed globally with this hash
    let is_globally_installed =
        global_db.is_installed("llvm", llvm_version, llvm_platform, &archive_sha256)?
            && global_llvm_dir.exists();

    if is_globally_installed {
        tracing::info!(
            "LLVM {} is already installed globally, creating symlink only",
            llvm_version
        );
    } else {
        // Need to download and install globally
        tracing::info!("Installing LLVM {} to global storage", llvm_version);

        // Extract to temp directory first
        let extract_dir = temp_dir.join("llvm_extract");
        if extract_dir.exists() {
            fs::remove_dir_all(&extract_dir)?;
        }
        fs::create_dir_all(&extract_dir)?;

        tracing::info!("Extracting LLVM archive...");
        extract_tar_xz(&archive_path, &extract_dir)?;

        // Find the extracted directory (should be something like clang+llvm-21.1.6-x86_64-pc-windows-msvc)
        let extracted_dir = fs::read_dir(&extract_dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.is_dir())
            .ok_or_else(|| anyhow_loc!("Could not find extracted LLVM directory"))?;

        tracing::info!("Found extracted directory: {}", extracted_dir.display());

        // Setup global target directory: ~/.anubis/toolchains/llvm/{version}-{platform}/{platform}
        // The extra {platform} level preserves the path structure expected by toolchains/ANUBIS
        let llvm_root = global_llvm_dir.join(llvm_platform);

        if global_llvm_dir.exists() {
            tracing::info!(
                "Removing existing global installation at {}",
                global_llvm_dir
            );
            fs::remove_dir_all(&global_llvm_dir)?;
        }

        // Move extracted directory to final location
        fs::create_dir_all(llvm_root.parent().unwrap())?;
        fs::rename(&extracted_dir, &llvm_root)?;

        // Deduplicate files in bin directory
        let bin_dir = llvm_root.join("bin");
        if bin_dir.exists() && bin_dir.is_dir() {
            tracing::info!("Deduplicating files in {}", bin_dir);
            deduplicate_files(&bin_dir)?;
        }

        // Record installation in global database
        let install_path_str = global_llvm_dir.to_string();
        global_db.record_installation(
            "llvm",
            llvm_version,
            llvm_platform,
            &archive_sha256,
            &install_path_str,
        )?;

        // Mark all files as read-only to prevent accidental modification
        tracing::info!("Setting LLVM toolchain files as read-only");
        set_readonly_recursive(&global_llvm_dir)?;

        tracing::info!(
            "Successfully installed LLVM toolchain globally at {}",
            global_llvm_dir
        );
    }

    // Create symlink in project: toolchains/llvm -> global_llvm_dir
    tracing::info!(
        "Creating symlink: {} -> {}",
        symlink_path.display(),
        global_llvm_dir
    );
    create_directory_symlink(&global_llvm_dir, &symlink_path)?;

    // Record symlink in project database
    let target_path_str = global_llvm_dir.to_string();
    project_db.record_symlink("llvm", "llvm", llvm_version, llvm_platform, &target_path_str)?;

    tracing::info!(
        "Successfully linked LLVM toolchain: {} -> {}",
        symlink_path.display(),
        global_llvm_dir
    );
    Ok(())
}

fn install_nasm(
    project_root: &Path,
    temp_dir: &Path,
    global_db: &GlobalToolchainDb,
    project_db: &ProjectToolchainDb,
    args: &InstallToolchainsArgs,
    config: Option<&NasmConfig>,
) -> anyhow::Result<()> {
    // Use config values if provided, otherwise use defaults
    let nasm_version = config
        .and_then(|c| {
            if c.version.is_empty() {
                None
            } else {
                Some(c.version.as_str())
            }
        })
        .unwrap_or(defaults::NASM_VERSION);
    let nasm_platform = config
        .and_then(|c| {
            if c.platform.is_empty() {
                None
            } else {
                Some(c.platform.as_str())
            }
        })
        .unwrap_or(defaults::NASM_PLATFORM);

    // Construct download URL based on version and platform
    let nasm_url = format!(
        "https://www.nasm.us/pub/nasm/releasebuilds/{}/{}/nasm-{}-{}.zip",
        nasm_version, nasm_platform, nasm_version, nasm_platform
    );
    let archive_filename = format!("nasm-{}-{}.zip", nasm_version, nasm_platform);

    tracing::info!("Installing NASM assembler {}", nasm_version);

    // Compute paths
    // Global: ~/.anubis/toolchains/nasm/{version}-{platform}/{platform}/...
    // Project symlink: toolchains/nasm -> global path
    let global_nasm_dir =
        get_global_toolchains_dir().join("nasm").join(format!("{}-{}", nasm_version, nasm_platform));
    let symlink_path = project_root.join("toolchains").join("nasm");

    // Fast path: check if symlink is already correct
    if project_db.is_symlink_current("nasm", nasm_version, nasm_platform)? {
        if is_symlink(&symlink_path) {
            if let Some(target) = read_symlink_target(&symlink_path) {
                if target == global_nasm_dir && global_nasm_dir.exists() {
                    tracing::info!("NASM {} symlink is up-to-date, skipping", nasm_version);
                    return Ok(());
                }
            }
        }
    }

    // Download archive if not present
    let archive_path = temp_dir.join(&archive_filename);
    if !archive_path.exists() || !args.keep_downloads {
        download_to_path(&nasm_url, &archive_path)?;
    } else {
        tracing::info!("Reusing existing download at {}", archive_path.display());
    }

    // Compute SHA256 of the downloaded archive for tracking
    tracing::info!("Computing SHA256 hash of downloaded archive...");
    let archive_sha256 = compute_file_sha256(&archive_path)?;

    // Check if already installed globally with this hash
    let is_globally_installed =
        global_db.is_installed("nasm", nasm_version, nasm_platform, &archive_sha256)?
            && global_nasm_dir.exists();

    if is_globally_installed {
        tracing::info!(
            "NASM {} is already installed globally, creating symlink only",
            nasm_version
        );
    } else {
        // Need to install globally
        tracing::info!("Installing NASM {} to global storage", nasm_version);

        // Extract to temp directory
        tracing::info!("Extracting archive...");
        let extract_dir = temp_dir.join("nasm_extract");
        if extract_dir.exists() {
            fs::remove_dir_all(&extract_dir)?;
        }
        fs::create_dir_all(&extract_dir)?;
        extract_zip(&archive_path, &extract_dir)?;

        // Find the extracted directory (e.g., nasm-3.01)
        let extracted_dir = fs::read_dir(&extract_dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.is_dir())
            .ok_or_else(|| anyhow_loc!("Could not find extracted NASM directory in temp folder"))?;

        tracing::info!("Found extracted directory: {}", extracted_dir.display());

        // Setup global target directory: ~/.anubis/toolchains/nasm/{version}-{platform}/{platform}
        // The extra {platform} level preserves the path structure expected by toolchains/ANUBIS
        let nasm_root = global_nasm_dir.join(nasm_platform);

        if global_nasm_dir.exists() {
            tracing::info!(
                "Removing existing global installation at {}",
                global_nasm_dir
            );
            fs::remove_dir_all(&global_nasm_dir)?;
        }

        // Move extracted directory to final location
        fs::create_dir_all(nasm_root.parent().unwrap())?;
        fs::rename(&extracted_dir, &nasm_root)?;

        // Record installation in global database
        let install_path_str = global_nasm_dir.to_string();
        global_db.record_installation(
            "nasm",
            nasm_version,
            nasm_platform,
            &archive_sha256,
            &install_path_str,
        )?;

        // Mark all files as read-only to prevent accidental modification
        tracing::info!("Setting NASM files as read-only");
        set_readonly_recursive(&global_nasm_dir)?;

        tracing::info!(
            "Successfully installed NASM globally at {}",
            global_nasm_dir
        );
    }

    // Create symlink in project: toolchains/nasm -> global_nasm_dir
    tracing::info!(
        "Creating symlink: {} -> {}",
        symlink_path.display(),
        global_nasm_dir
    );
    create_directory_symlink(&global_nasm_dir, &symlink_path)?;

    // Record symlink in project database
    let target_path_str = global_nasm_dir.to_string();
    project_db.record_symlink("nasm", "nasm", nasm_version, nasm_platform, &target_path_str)?;

    tracing::info!(
        "Successfully linked NASM: {} -> {}",
        symlink_path.display(),
        global_nasm_dir
    );
    Ok(())
}

fn install_msvc(
    project_root: &Path,
    temp_dir: &Path,
    global_db: &GlobalToolchainDb,
    project_db: &ProjectToolchainDb,
    args: &InstallToolchainsArgs,
    config: Option<&MsvcConfig>,
) -> anyhow::Result<()> {
    tracing::info!("Installing MSVC toolchain and Windows SDK");

    const MSVC_PLATFORM: &str = "x64";

    // Determine VS channel from config or use default
    let vs_channel = config
        .and_then(|c| {
            if c.vs_channel.is_empty() {
                None
            } else {
                Some(c.vs_channel.as_str())
            }
        })
        .unwrap_or(defaults::MSVC_VS_CHANNEL);

    // Get optional specific versions from config
    let requested_msvc_version = config.and_then(|c| c.msvc_version.as_deref());
    let requested_sdk_version = config.and_then(|c| c.sdk_version.as_deref());

    // Download VS manifest
    let manifest_url = format!("https://aka.ms/vs/{}/stable/channel", vs_channel);

    tracing::info!("Downloading Visual Studio manifest from {}", manifest_url);
    let mut response =
        ureq::get(&manifest_url).call().map_err(|e| anyhow_loc!("Failed to download VS manifest: {}", e))?;
    let channel_manifest: JsonValue = response.body_mut().read_json()?;

    // Find the channelItems and get the VS manifest URL
    let channel_items = channel_manifest
        .get("channelItems")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow_loc!("No channelItems in manifest"))?;

    let vs_manifest_item = channel_items
        .iter()
        .find(|item| {
            item.get("type").and_then(|t| t.as_str()) == Some("Manifest")
                && item.get("id").and_then(|id| id.as_str())
                    == Some("Microsoft.VisualStudio.Manifests.VisualStudio")
        })
        .ok_or_else(|| anyhow_loc!("Could not find VS manifest item"))?;

    let vs_manifest_url = vs_manifest_item
        .get("payloads")
        .and_then(|p| p.as_array())
        .and_then(|arr| arr.first())
        .and_then(|p| p.get("url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow_loc!("Could not find VS manifest URL"))?;

    tracing::info!("Downloading VS manifest from {}", vs_manifest_url);
    let mut vs_response = ureq::get(vs_manifest_url)
        .call()
        .map_err(|e| anyhow_loc!("Failed to download VS manifest payload: {}", e))?;
    // VS manifest can be ~17MB, increase limit from default 10MB
    let vs_manifest: JsonValue = vs_response.body_mut().with_config().limit(30 * 1024 * 1024).read_json()?;

    // Get packages
    let packages = vs_manifest
        .get("packages")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow_loc!("No packages in VS manifest"))?;

    // Find MSVC and SDK packages
    const HOST: &str = "x64";
    const TARGET: &str = "x64";

    // Find latest MSVC version
    // Look for packages matching: Microsoft.VC.{version}.Tools.Host{host}.Target{target}.base
    let mut msvc_candidates = Vec::new();

    for package in packages {
        let id = package.get("id").and_then(|i| i.as_str()).unwrap_or("");
        let id_lower = id.to_lowercase();

        // Check if this is an MSVC tools package for our host/target
        // Skip "Premium" variants - we want the base toolchain
        if id_lower.starts_with("microsoft.vc.")
            && id_lower.contains(&format!(".tools.host{}.target{}.base", HOST, TARGET))
            && !id_lower.contains(".premium.")
        {
            // Extract the full version string between "microsoft.vc." and ".tools."
            if let Some(vc_pos) = id_lower.find("microsoft.vc.") {
                if let Some(tools_pos) = id_lower.find(".tools.") {
                    let version_start = vc_pos + "microsoft.vc.".len();
                    let version = &id[version_start..tools_pos];

                    tracing::info!("Found MSVC candidate: version={}, id={}", version, id);
                    msvc_candidates.push((version.to_string(), id.to_string()));
                }
            }
        }
    }

    if msvc_candidates.is_empty() {
        bail_loc!(
            "Could not find any MSVC compiler packages for host={} target={}",
            HOST,
            TARGET
        );
    }

    // Sort by version (lexicographically) - latest first
    msvc_candidates.sort_by(|a, b| b.0.cmp(&a.0));

    // Select version: use requested version if specified, otherwise take the latest
    let (msvc_ver, msvc_package_id) = if let Some(requested) = requested_msvc_version {
        // Find the candidate matching the requested version
        msvc_candidates
            .iter()
            .find(|(ver, _)| ver == requested)
            .ok_or_else(|| {
                let available: Vec<_> = msvc_candidates.iter().map(|(v, _)| v.as_str()).collect();
                anyhow_loc!(
                    "Requested MSVC version '{}' not found. Available versions: {:?}",
                    requested,
                    available
                )
            })?
            .clone()
    } else {
        msvc_candidates[0].clone()
    };
    tracing::info!(
        "Selected MSVC version: {} (from package {})",
        msvc_ver,
        msvc_package_id
    );

    // Find SDK version - check both Windows 10 and Windows 11 SDKs
    let mut sdk_candidates = Vec::new();
    for package in packages {
        let id = package.get("id").and_then(|i| i.as_str()).unwrap_or("");

        // Check Windows 10 SDK
        if let Some(version) = id.strip_prefix("Microsoft.VisualStudio.Component.Windows10SDK.") {
            // Only accept numeric SDK versions (e.g., "19041", "22000")
            if !version.is_empty() && version.chars().all(|c| c.is_numeric() || c == '.') {
                tracing::info!("Found Windows 10 SDK candidate: {}", version);
                sdk_candidates.push((version.to_string(), id.to_string()));
            }
        }

        // Check Windows 11 SDK
        if let Some(version) = id.strip_prefix("Microsoft.VisualStudio.Component.Windows11SDK.") {
            // Only accept numeric SDK versions
            if !version.is_empty() && version.chars().all(|c| c.is_numeric() || c == '.') {
                tracing::info!("Found Windows 11 SDK candidate: {}", version);
                sdk_candidates.push((version.to_string(), id.to_string()));
            }
        }
    }

    if sdk_candidates.is_empty() {
        // Log all SDK-related packages for debugging
        tracing::warn!("No valid SDK packages found. Listing all SDK-related packages:");
        for package in packages {
            let id = package.get("id").and_then(|i| i.as_str()).unwrap_or("");
            if id.contains("SDK") && id.contains("Windows") {
                tracing::warn!("  Found: {}", id);
            }
        }
        bail_loc!("Could not find any Windows SDK packages");
    }

    // Sort by version - latest first
    sdk_candidates.sort_by(|a, b| b.0.cmp(&a.0));

    // Select version: use requested version if specified, otherwise take the latest
    let (sdk_ver, sdk_package_id) = if let Some(requested) = requested_sdk_version {
        // Find the candidate matching the requested version
        sdk_candidates.iter().find(|(ver, _)| ver == requested).ok_or_else(|| {
            let available: Vec<_> = sdk_candidates.iter().map(|(v, _)| v.as_str()).collect();
            anyhow_loc!(
                "Requested Windows SDK version '{}' not found. Available versions: {:?}",
                requested,
                available
            )
        })?
    } else {
        &sdk_candidates[0]
    };
    tracing::info!(
        "Selected Windows SDK version: {} (from package {})",
        sdk_ver,
        sdk_package_id
    );

    // Compute paths
    // Global: ~/.anubis/toolchains/msvc/{version}/...
    // Project symlink: toolchains/msvc -> global path
    let global_msvc_dir = get_global_toolchains_dir().join("msvc").join(&msvc_ver);
    let symlink_path = project_root.join("toolchains").join("msvc");

    // Fast path: check if symlink is already correct
    if project_db.is_symlink_current("msvc", &msvc_ver, MSVC_PLATFORM)? {
        if is_symlink(&symlink_path) {
            if let Some(target) = read_symlink_target(&symlink_path) {
                if target == global_msvc_dir && global_msvc_dir.exists() {
                    tracing::info!("MSVC {} symlink is up-to-date, checking SDK...", msvc_ver);
                    // Still install SDK if needed
                    install_windows_sdk(
                        project_root,
                        temp_dir,
                        global_db,
                        project_db,
                        packages,
                        sdk_package_id,
                        sdk_ver,
                        args,
                    )?;
                    return Ok(());
                }
            }
        }
    }

    // Collect packages to download
    let mut downloads: Vec<(String, String, String)> = Vec::new(); // (url, sha256, filename)

    // Helper to find and add package
    let add_package = |downloads: &mut Vec<(String, String, String)>, pkg_id: &str| -> anyhow::Result<()> {
        for package in packages {
            let id = package.get("id").and_then(|i| i.as_str()).unwrap_or("");
            if id.to_lowercase() == pkg_id.to_lowercase() {
                if let Some(payloads) = package.get("payloads").and_then(|p| p.as_array()) {
                    for payload in payloads {
                        if let (Some(url), Some(sha256), Some(filename)) = (
                            payload.get("url").and_then(|u| u.as_str()),
                            payload.get("sha256").and_then(|s| s.as_str()),
                            payload.get("fileName").and_then(|f| f.as_str()),
                        ) {
                            downloads.push((url.to_string(), sha256.to_string(), filename.to_string()));
                        }
                    }
                }
                return Ok(());
            }
        }
        Err(anyhow_loc!("Package not found: {}", pkg_id))
    };

    // Add MSVC packages - use the exact package ID we found
    add_package(&mut downloads, &msvc_package_id)?;

    // Add related MSVC packages for the same version
    let target_lower = TARGET.to_lowercase();
    add_package(
        &mut downloads,
        &format!("Microsoft.VC.{}.CRT.Headers.base", &msvc_ver),
    )?;
    add_package(
        &mut downloads,
        &format!("Microsoft.VC.{}.CRT.{}.Desktop.base", &msvc_ver, target_lower),
    )?;
    // Add CRT Store package which contains additional libs like oldnames.lib
    add_package(
        &mut downloads,
        &format!("Microsoft.VC.{}.CRT.{}.Store.base", &msvc_ver, target_lower),
    )?;

    tracing::info!("Downloading {} MSVC packages", downloads.len());

    // Create a combined hash of all packages for database tracking
    let mut combined_hashes = String::new();
    for (_url, sha256, _filename) in &downloads {
        combined_hashes.push_str(sha256);
        combined_hashes.push('\n');
    }
    let mut hasher = Sha256::new();
    hasher.update(combined_hashes.as_bytes());
    let installation_hash = format!("{:x}", hasher.finalize());

    // Check if already installed globally
    let is_globally_installed =
        global_db.is_installed("msvc", &msvc_ver, MSVC_PLATFORM, &installation_hash)?
            && global_msvc_dir.exists();

    if is_globally_installed {
        tracing::info!(
            "MSVC {} is already installed globally, creating symlink only",
            msvc_ver
        );
    } else {
        // Need to install globally
        tracing::info!("Installing MSVC {} to global storage", msvc_ver);

        // Download all packages
        for (url, sha256, filename) in &downloads {
            let file_path = temp_dir.join(filename);

            if args.keep_downloads && file_path.exists() {
                tracing::info!("Reusing existing download: {}", filename);
                // Verify hash
                if verify_sha256(&file_path, sha256)? {
                    continue;
                } else {
                    tracing::warn!("Hash mismatch for cached file, re-downloading: {}", filename);
                }
            }

            download_with_sha256(url, &file_path, sha256)?;
        }

        // Extract packages to global location
        if global_msvc_dir.exists() {
            fs::remove_dir_all(&global_msvc_dir)?;
        }
        fs::create_dir_all(&global_msvc_dir)?;

        tracing::info!("Extracting packages to {}", global_msvc_dir);

        // Extract packages
        for (_url, _sha256, filename) in &downloads {
            let file_path = temp_dir.join(filename);

            // VSIX files are ZIP files
            if filename.ends_with(".vsix") || filename.ends_with(".zip") {
                tracing::info!("Extracting VSIX: {}", filename);
                extract_vsix_zip(&file_path, global_msvc_dir.as_ref())?;
            } else if filename.ends_with(".msi") {
                // Extract MSI using msiexec on Windows
                #[cfg(windows)]
                {
                    tracing::info!("Extracting MSI: {}", filename);
                    extract_msi(&file_path, &global_msvc_dir)?;
                }
            } else {
                tracing::warn!("Skipping unknown file type: {}", filename);
            }
        }

        // Record installation in global database
        let install_path_str = global_msvc_dir.to_string();
        global_db.record_installation(
            "msvc",
            &msvc_ver,
            MSVC_PLATFORM,
            &installation_hash,
            &install_path_str,
        )?;

        // Mark all files as read-only to prevent accidental modification
        tracing::info!("Setting MSVC toolchain files as read-only");
        set_readonly_recursive(&global_msvc_dir)?;

        tracing::info!(
            "Successfully installed MSVC toolchain globally at {}",
            global_msvc_dir
        );
    }

    // Create symlink in project: toolchains/msvc -> global_msvc_dir
    tracing::info!(
        "Creating symlink: {} -> {}",
        symlink_path.display(),
        global_msvc_dir
    );
    create_directory_symlink(&global_msvc_dir, &symlink_path)?;

    // Record symlink in project database
    let target_path_str = global_msvc_dir.to_string();
    project_db.record_symlink("msvc", "msvc", &msvc_ver, MSVC_PLATFORM, &target_path_str)?;

    tracing::info!(
        "Successfully linked MSVC: {} -> {}",
        symlink_path.display(),
        global_msvc_dir
    );

    // Install Windows SDK
    install_windows_sdk(
        project_root,
        temp_dir,
        global_db,
        project_db,
        packages,
        sdk_package_id,
        sdk_ver,
        args,
    )?;

    Ok(())
}

fn install_windows_sdk(
    project_root: &Path,
    temp_dir: &Path,
    global_db: &GlobalToolchainDb,
    project_db: &ProjectToolchainDb,
    packages: &[JsonValue],
    sdk_package_id: &str,
    sdk_ver: &str,
    args: &InstallToolchainsArgs,
) -> anyhow::Result<()> {
    const SDK_PLATFORM: &str = "x64";

    tracing::info!("Installing Windows SDK {} to separate directory", sdk_ver);

    // Compute paths
    // Global: ~/.anubis/toolchains/windows_kits/{sdk_ver}/...
    // Project symlink: toolchains/windows_kits -> global path
    let global_sdk_dir = get_global_toolchains_dir().join("windows_kits").join(sdk_ver);
    let symlink_path = project_root.join("toolchains").join("windows_kits");

    // Fast path: check if symlink is already correct
    if project_db.is_symlink_current("windows_kits", sdk_ver, SDK_PLATFORM)? {
        if is_symlink(&symlink_path) {
            if let Some(target) = read_symlink_target(&symlink_path) {
                if target == global_sdk_dir && global_sdk_dir.exists() {
                    tracing::info!("Windows SDK {} symlink is up-to-date, skipping", sdk_ver);
                    return Ok(());
                }
            }
        }
    }

    // Find the SDK component package - this is a meta-package with dependencies
    let sdk_component = packages
        .iter()
        .find(|p| p.get("id").and_then(|id| id.as_str()) == Some(sdk_package_id))
        .ok_or_else(|| anyhow_loc!("Could not find SDK component {}", sdk_package_id))?;

    // Get dependencies from the SDK component
    let dependencies = sdk_component
        .get("dependencies")
        .and_then(|d| d.as_object())
        .ok_or_else(|| anyhow_loc!("SDK component has no dependencies"))?;

    tracing::info!("SDK component has {} dependency entries", dependencies.len());

    // Collect all SDK dependency package IDs
    let mut sdk_dep_packages = Vec::new();
    for (dep_key, _dep_value) in dependencies {
        // Dependencies are package IDs like "Win11SDK_10.0.26100"
        tracing::info!("Found SDK dependency: {}", dep_key);
        sdk_dep_packages.push(dep_key.as_str());
    }

    // Essential SDK MSI files we need for C/C++ development
    // Based on the portable-msvc.py script, we need these MSI files
    const TARGET: &str = "x64"; // TODO: make this configurable
    let essential_msis = vec![
        "Universal CRT Headers Libraries and Sources".to_string(),
        "Windows SDK Desktop Headers x86".to_string(), // x86 contains extras like d3d10misc.h
        format!("Windows SDK Desktop Libs {}", TARGET),
        "Windows SDK OnecoreUap Headers".to_string(),
        "Windows SDK for Windows Store Apps Headers".to_string(),
        "Windows SDK for Windows Store Apps Libs".to_string(),
        "Windows SDK for Windows Store Apps Tools".to_string(),
    ];

    // First, collect all payloads info to create installation hash
    let mut all_payloads: Vec<(String, String, String)> = Vec::new(); // (url, sha256, filename)

    for dep_id in &sdk_dep_packages {
        if let Some(dep_package) =
            packages.iter().find(|p| p.get("id").and_then(|id| id.as_str()) == Some(*dep_id))
        {
            if let Some(payloads) = dep_package.get("payloads").and_then(|p| p.as_array()) {
                for payload in payloads {
                    if let (Some(url), Some(sha256), Some(filename)) = (
                        payload.get("url").and_then(|u| u.as_str()),
                        payload.get("sha256").and_then(|s| s.as_str()),
                        payload.get("fileName").and_then(|f| f.as_str()),
                    ) {
                        all_payloads.push((url.to_string(), sha256.to_string(), filename.to_string()));
                    }
                }
            }
        }
    }

    // Create a combined hash of all SDK packages for database tracking
    let mut combined_hashes = String::new();
    for (_url, sha256, _filename) in &all_payloads {
        combined_hashes.push_str(sha256);
        combined_hashes.push('\n');
    }
    let mut hasher = Sha256::new();
    hasher.update(combined_hashes.as_bytes());
    let installation_hash = format!("{:x}", hasher.finalize());

    // Check if already installed globally
    let is_globally_installed =
        global_db.is_installed("windows_kits", sdk_ver, SDK_PLATFORM, &installation_hash)?
            && global_sdk_dir.exists();

    if is_globally_installed {
        tracing::info!(
            "Windows SDK {} is already installed globally, creating symlink only",
            sdk_ver
        );
    } else {
        // Need to install globally
        tracing::info!("Installing Windows SDK {} to global storage", sdk_ver);

        // Download ALL payloads (MSI and CAB files)
        // The MSI files need their associated CAB files to be present during extraction
        let mut sdk_msi_files = Vec::new();
        let mut total_downloaded = 0;

        for dep_id in &sdk_dep_packages {
            // Find the dependency package
            if let Some(dep_package) =
                packages.iter().find(|p| p.get("id").and_then(|id| id.as_str()) == Some(*dep_id))
            {
                tracing::info!("Processing dependency package: {}", dep_id);

                // Get payloads from the dependency package
                if let Some(payloads) = dep_package.get("payloads").and_then(|p| p.as_array()) {
                    for payload in payloads {
                        if let (Some(url), Some(sha256), Some(filename)) = (
                            payload.get("url").and_then(|u| u.as_str()),
                            payload.get("sha256").and_then(|s| s.as_str()),
                            payload.get("fileName").and_then(|f| f.as_str()),
                        ) {
                            let file_path = temp_dir.join(filename);

                            // Download all payloads (MSI and CAB files)
                            // CAB files are needed by MSI during extraction
                            if args.keep_downloads && file_path.exists() {
                                tracing::debug!("Reusing cached file: {}", filename);
                            } else if !file_path.exists() {
                                download_with_sha256(url, &file_path, sha256)?;
                                total_downloaded += 1;
                            }

                            // Track only the essential MSI files for extraction
                            if filename.ends_with(".msi") {
                                // Strip the "Installers\" prefix if present
                                let base_filename = filename.strip_prefix("Installers\\").unwrap_or(filename);

                                // Check if this is an essential MSI
                                let is_essential = essential_msis
                                    .iter()
                                    .any(|essential| base_filename.starts_with(essential.as_str()));

                                if is_essential {
                                    tracing::info!("Will extract SDK MSI: {}", filename);
                                    sdk_msi_files.push(file_path);
                                }
                            }
                        }
                    }
                }
            }
        }

        tracing::info!(
            "Downloaded {} new files, will extract {} SDK MSI files",
            total_downloaded,
            sdk_msi_files.len()
        );

        // Extract SDK MSIs to a temp directory first
        let sdk_temp = temp_dir.join("sdk_extract");
        if sdk_temp.exists() {
            fs::remove_dir_all(&sdk_temp)?;
        }
        fs::create_dir_all(&sdk_temp)?;

        tracing::info!("Extracting SDK MSI files to temp directory");

        #[cfg(windows)]
        {
            for msi_path in &sdk_msi_files {
                let filename = msi_path.file_name().unwrap().to_string_lossy();
                tracing::info!("Extracting SDK MSI: {}", filename);

                // Collect list of files before extraction
                let mut files_before = std::collections::HashSet::new();
                if tracing::enabled!(tracing::Level::TRACE) {
                    collect_all_files(&sdk_temp, &mut files_before);
                }

                // Extract the MSI
                extract_msi(msi_path, &sdk_temp)?;

                // Collect list of files after extraction
                if tracing::enabled!(tracing::Level::TRACE) {
                    let mut files_after = std::collections::HashSet::new();
                    collect_all_files(&sdk_temp, &mut files_after);

                    // Find newly added files (files in after but not in before)
                    let mut new_files: Vec<_> = files_after.difference(&files_before).collect();
                    new_files.sort();

                    // Log newly extracted files at TRACE level
                    if !new_files.is_empty() {
                        tracing::trace!("MSI '{}' extracted {} new files:", filename, new_files.len());
                        for file in &new_files {
                            tracing::trace!("  {}", file);
                        }
                    }
                }
            }
        }

        // Move extracted contents to global location
        // MSI extracts to: sdk_temp/Windows Kits/10/*
        // We want: ~/.anubis/toolchains/windows_kits/{sdk_ver}/*
        if global_sdk_dir.exists() {
            fs::remove_dir_all(&global_sdk_dir)?;
        }
        fs::create_dir_all(&global_sdk_dir)?;

        let windows_kits_extracted = sdk_temp.join("Windows Kits").join("10");
        if windows_kits_extracted.exists() {
            tracing::info!("Moving SDK contents to {}", global_sdk_dir);

            // Move all contents from "Windows Kits/10/*" to global_sdk_dir
            for entry in fs::read_dir(&windows_kits_extracted)? {
                let entry = entry?;
                let file_name = entry.file_name().to_string_lossy().to_string();
                let dest = global_sdk_dir.join(&file_name);
                tracing::debug!("Moving {} to {}", entry.path().display(), dest);
                fs::rename(entry.path(), &dest)?;
            }
        } else {
            bail_loc!("Expected Windows Kits/10 directory not found after MSI extraction");
        }

        // Clean up temp extraction directory
        tracing::info!("Cleaning up SDK temp directory");
        fs::remove_dir_all(&sdk_temp)?;

        // Record installation in global database
        let install_path_str = global_sdk_dir.to_string();
        global_db.record_installation(
            "windows_kits",
            sdk_ver,
            SDK_PLATFORM,
            &installation_hash,
            &install_path_str,
        )?;

        // Mark all files as read-only to prevent accidental modification
        tracing::info!("Setting Windows SDK files as read-only");
        set_readonly_recursive(&global_sdk_dir)?;

        tracing::info!(
            "Successfully installed Windows SDK globally at {}",
            global_sdk_dir
        );
    }

    // Create symlink in project: toolchains/windows_kits -> global_sdk_dir
    tracing::info!(
        "Creating symlink: {} -> {}",
        symlink_path.display(),
        global_sdk_dir
    );
    create_directory_symlink(&global_sdk_dir, &symlink_path)?;

    // Record symlink in project database
    let target_path_str = global_sdk_dir.to_string();
    project_db.record_symlink(
        "windows_kits",
        "windows_kits",
        sdk_ver,
        SDK_PLATFORM,
        &target_path_str,
    )?;

    tracing::info!(
        "Successfully linked Windows SDK: {} -> {}",
        symlink_path.display(),
        global_sdk_dir
    );
    Ok(())
}

fn download_to_path(url: &str, destination: &Path) -> anyhow::Result<()> {
    tracing::info!("Downloading {} -> {}", url, destination.display());
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let response = ureq::get(url).call().map_err(|err| anyhow_loc!("Failed to download {}: {}", url, err))?;

    if response.status().as_u16() >= 400 {
        bail_loc!("Failed to download {}: HTTP {}", url, response.status());
    }

    let mut reader = response.into_body().into_reader();
    let mut file = File::create(destination)?;
    std::io::copy(&mut reader, &mut file)?;
    Ok(())
}

fn extract_zip(archive_path: &Path, destination: &Path) -> anyhow::Result<()> {
    tracing::info!(
        "Extracting {} -> {}",
        archive_path.display(),
        destination.display()
    );

    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let relative_path = entry
            .enclosed_name()
            .ok_or_else(|| anyhow_loc!("Archive entry has invalid path: {}", entry.name()))?;
        let out_path = destination.join(relative_path);

        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut outfile = File::create(&out_path)?;
        std::io::copy(&mut entry, &mut outfile)?;
    }

    Ok(())
}

fn extract_tar_xz(archive_path: &Path, destination: &Path) -> anyhow::Result<()> {
    tracing::info!(
        "Extracting {} -> {}",
        archive_path.display(),
        destination.display()
    );

    let file = File::open(archive_path)?;
    let decompressor = XzDecoder::new(file);
    let mut archive = Archive::new(decompressor);

    archive.unpack(destination)?;
    Ok(())
}

fn deduplicate_files(dir: impl AsRef<Path>) -> anyhow::Result<()> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let dir = dir.as_ref();
    // Collect all files with their sizes
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let metadata = entry.metadata()?;
            files.push((path, metadata.len()));
        }
    }

    // Group files by size (quick pre-filter)
    let mut size_groups: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for (path, size) in files {
        size_groups.entry(size).or_insert_with(Vec::new).push(path);
    }

    // For each size group with multiple files, check if they're identical
    let mut total_saved = 0u64;
    let mut dedup_count = 0usize;

    for (size, paths) in size_groups.iter() {
        if paths.len() < 2 || *size == 0 {
            continue; // Skip if only one file or empty files
        }

        // Compute hash for each file in this size group
        let mut hash_groups: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        for path in paths {
            match compute_file_hash(path) {
                Ok(hash) => {
                    hash_groups.entry(hash).or_insert_with(Vec::new).push(path.clone());
                }
                Err(e) => {
                    tracing::warn!("Failed to hash {}: {}", path.display(), e);
                }
            }
        }

        // For files with same hash, verify they're truly identical and deduplicate
        for (hash, same_hash_paths) in hash_groups.iter() {
            if same_hash_paths.len() < 2 {
                continue;
            }

            // Verify files are actually identical (hash collision check)
            let mut verified_groups: Vec<Vec<PathBuf>> = Vec::new();
            for path in same_hash_paths {
                let mut found_group = false;
                for group in &mut verified_groups {
                    if files_are_identical(path, &group[0])? {
                        group.push(path.clone());
                        found_group = true;
                        break;
                    }
                }
                if !found_group {
                    verified_groups.push(vec![path.clone()]);
                }
            }

            // For each verified group, keep the first file and replace others with hard links
            for group in verified_groups {
                if group.len() < 2 {
                    continue;
                }

                let original = &group[0];
                for duplicate in &group[1..] {
                    // Remove the duplicate and create a hard link
                    fs::remove_file(duplicate)?;
                    fs::hard_link(original, duplicate)?;

                    dedup_count += 1;
                    total_saved += size;

                    tracing::debug!(
                        "Deduplicated: {} -> {}",
                        duplicate.file_name().unwrap().to_string_lossy(),
                        original.file_name().unwrap().to_string_lossy()
                    );
                }
            }
        }
    }

    if dedup_count > 0 {
        tracing::info!(
            "Deduplicated {} files, saved {:.2} MB",
            dedup_count,
            total_saved as f64 / (1024.0 * 1024.0)
        );
    }

    Ok(())
}

fn compute_file_hash(path: &Path) -> anyhow::Result<u64> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut file = File::open(path)?;
    let mut hasher = DefaultHasher::new();
    let mut buffer = vec![0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.write(&buffer[..bytes_read]);
    }

    Ok(hasher.finish())
}

fn files_are_identical(path1: &Path, path2: &Path) -> anyhow::Result<bool> {
    let mut file1 = File::open(path1)?;
    let mut file2 = File::open(path2)?;

    let mut buffer1 = vec![0u8; 8192];
    let mut buffer2 = vec![0u8; 8192];

    loop {
        let bytes1 = file1.read(&mut buffer1)?;
        let bytes2 = file2.read(&mut buffer2)?;

        if bytes1 != bytes2 {
            return Ok(false);
        }

        if bytes1 == 0 {
            return Ok(true); // Both reached EOF
        }

        if buffer1[..bytes1] != buffer2[..bytes2] {
            return Ok(false);
        }
    }
}

fn compute_file_sha256(file_path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(file_path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    let result = hasher.finalize();
    Ok(format!("{:x}", result))
}

fn copy_dir_recursive(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> anyhow::Result<()> {
    let source = source.as_ref();
    let destination = destination.as_ref();
    if !source.is_dir() {
        bail_loc!("Source {} is not a directory", source.display());
    }

    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let entry_path = entry.path();
        let target_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            copy_dir_recursive(&entry_path, &target_path)?;
        } else if file_type.is_file() {
            fs::copy(&entry_path, &target_path)?;
        }
    }

    Ok(())
}

fn verify_sha256(file_path: &Path, expected_hash: &str) -> anyhow::Result<bool> {
    let mut file = File::open(file_path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    let result = hasher.finalize();
    let computed_hash = format!("{:x}", result);

    Ok(computed_hash.to_lowercase() == expected_hash.to_lowercase())
}

fn download_with_sha256(url: &str, destination: &Path, expected_hash: &str) -> anyhow::Result<()> {
    tracing::info!("Downloading {} -> {}", url, destination.display());

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let response = ureq::get(url).call().map_err(|err| anyhow_loc!("Failed to download {}: {}", url, err))?;

    if response.status().as_u16() >= 400 {
        bail_loc!("Failed to download {}: HTTP {}", url, response.status());
    }

    let mut reader = response.into_body().into_reader();
    let mut file = File::create(destination)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8192];

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buffer[..bytes_read])?;
        hasher.update(&buffer[..bytes_read]);
    }

    let result = hasher.finalize();
    let computed_hash = format!("{:x}", result);

    if computed_hash.to_lowercase() != expected_hash.to_lowercase() {
        bail_loc!(
            "SHA256 mismatch for {}: expected {}, got {}",
            destination.display(),
            expected_hash,
            computed_hash
        );
    }

    tracing::info!(
        "✓ Verified SHA256 for {}",
        destination.file_name().unwrap().to_string_lossy()
    );
    Ok(())
}

fn extract_vsix_zip(archive_path: &Path, destination: &Path) -> anyhow::Result<()> {
    tracing::info!(
        "Extracting {} to {}",
        archive_path.file_name().unwrap().to_string_lossy(),
        destination.display()
    );

    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let entry_name = entry.name().to_string();

        // VSIX packages have contents in "Contents/" folder
        if let Some(relative_path) = entry_name.strip_prefix("Contents/") {
            let out_path = destination.join(relative_path);

            if entry.is_dir() {
                fs::create_dir_all(&out_path)?;
                continue;
            }

            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }

            let mut outfile = File::create(&out_path)?;
            std::io::copy(&mut entry, &mut outfile)?;
        }
    }

    Ok(())
}

#[cfg(windows)]
fn extract_msi(msi_path: impl AsRef<Path>, destination: impl AsRef<Path>) -> anyhow::Result<()> {
    let msi_path = msi_path.as_ref();
    let destination = destination.as_ref();
    tracing::info!(
        "Extracting MSI {} to {}",
        msi_path.file_name().unwrap().to_string_lossy(),
        destination.display()
    );

    // Get absolute path without using canonicalize (which adds \\?\ prefix that msiexec doesn't like)
    let abs_dest = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        env::current_dir()?.join(destination)
    };

    // TARGETDIR must end with backslash for msiexec
    let dest_str = format!("{}\\", abs_dest.display());

    let log_path = abs_dest.join("msi_install.log");

    let output = Command::new("msiexec.exe")
        .arg("/a")
        .arg(msi_path.as_os_str())
        .arg("/qn")
        .arg(format!("TARGETDIR={}", dest_str))
        .arg("/L*V")
        .arg(&log_path)
        .output()
        .map_err(|e| anyhow_loc!("Failed to run msiexec: {}", e))?;

    if !output.status.success() {
        // Read log file for more details
        let log_contents =
            fs::read_to_string(&log_path).unwrap_or_else(|_| "Could not read log file".to_string());
        let log_tail: String = log_contents
            .lines()
            .rev()
            .take(50)
            .collect::<Vec<&str>>()
            .into_iter()
            .rev()
            .collect::<Vec<&str>>()
            .join("\n");

        bail_loc!(
            "msiexec failed with status: {}\nstdout: {}\nstderr: {}\nLog (last 50 lines):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            log_tail
        );
    }

    Ok(())
}

/// Recursively collect all file paths in a directory
fn collect_all_files(path: &Path, files: &mut std::collections::HashSet<String>) {
    if let Ok(metadata) = fs::metadata(path) {
        if metadata.is_file() {
            // Add file path to set
            if let Some(path_str) = path.to_str() {
                files.insert(path_str.to_string());
            }
        } else if metadata.is_dir() {
            // Recurse into directories
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    collect_all_files(&entry.path(), files);
                }
            }
        }
    }
}

/// Recursively set all files in a directory as read-only.
/// This prevents accidental modification of shared toolchain files.
/// Uses jwalk for parallel directory walking to improve performance on large toolchains.
fn set_readonly_recursive(path: impl AsRef<Path>) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    let path = path.as_ref();
    if !path.exists() {
        return Ok(());
    }

    let start_time = Instant::now();
    let file_count = AtomicUsize::new(0);
    let error_count = AtomicUsize::new(0);

    // Use jwalk for parallel directory walking
    let walker = jwalk::WalkDir::new(path).skip_hidden(false).follow_links(false);

    // Process entries in parallel using jwalk's built-in parallelism
    for entry_result in walker {
        match entry_result {
            Ok(entry) => {
                let entry_path = entry.path();
                // Only process files, not directories
                if entry.file_type().is_file() {
                    match fs::metadata(&entry_path) {
                        Ok(metadata) => {
                            let mut perms = metadata.permissions();
                            perms.set_readonly(true);
                            if let Err(e) = fs::set_permissions(&entry_path, perms) {
                                tracing::warn!("Failed to set read-only on {}: {}", entry_path.display(), e);
                                error_count.fetch_add(1, Ordering::Relaxed);
                            } else {
                                file_count.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to get metadata for {}: {}", entry_path.display(), e);
                            error_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Error walking directory: {}", e);
                error_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let duration = start_time.elapsed();
    let files = file_count.load(Ordering::Relaxed);
    let errors = error_count.load(Ordering::Relaxed);

    tracing::debug!(
        "Set {} files as read-only in {:.2}ms (errors: {})",
        files,
        duration.as_secs_f64() * 1000.0,
        errors
    );

    Ok(())
}
