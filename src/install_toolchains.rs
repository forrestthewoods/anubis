use anyhow::{anyhow, bail};
use clap::Parser;
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

use crate::toolchain_db::ToolchainDb;

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

    let cwd = env::current_dir()?;

    // Open the toolchain database
    let db_path = cwd.join(".anubis_db");
    tracing::info!("Opening toolchain database at {}", db_path.display());
    let db = ToolchainDb::open(&db_path)?;

    // Use a temp directory relative to the project to avoid env var issues
    let temp_dir = cwd.join(".anubis-temp");
    if !args.keep_downloads && temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)?;
    }
    fs::create_dir_all(&temp_dir)?;

    // If discovery mode, only run MSVC discovery and exit
    if args.discover_msvc_packages {
        discover_msvc_packages(&cwd, &temp_dir, args)?;
        return Ok(());
    }

    // Install all toolchains
    install_zig(&cwd, &temp_dir, &db, args)?;
    install_llvm(&cwd, &temp_dir, &db, args)?;
    install_nasm(&cwd, &temp_dir, &db, args)?;
    install_msvc(&cwd, &temp_dir, &db, args)?;

    // Cleanup temp directory unless keeping downloads
    if !args.keep_downloads {
        if let Err(e) = fs::remove_dir_all(&temp_dir) {
            tracing::warn!("Failed to cleanup temp directory: {}", e);
        }
    } else {
        tracing::info!("Keeping downloads at {}", temp_dir.display());
    }

    Ok(())
}

/// Discover which MSVC packages contain which files by installing ALL packages
/// and tracking their contents. Outputs a report to .anubis-temp/msvc_package_contents.txt
fn discover_msvc_packages(
    cwd: &Path,
    temp_dir: &Path,
    args: &InstallToolchainsArgs,
) -> anyhow::Result<()> {
    tracing::info!("=== MSVC Package Discovery Mode ===");
    tracing::info!("This will download and extract ALL MSVC packages to discover their contents.");

    // Download VS manifest
    const MANIFEST_URL: &str = "https://aka.ms/vs/18/stable/channel";

    tracing::info!("Downloading Visual Studio manifest from {}", MANIFEST_URL);
    let response =
        ureq::get(MANIFEST_URL).call().map_err(|e| anyhow!("Failed to download VS manifest: {}", e))?;
    let channel_manifest: JsonValue = response.into_json()?;

    // Find the channelItems and get the VS manifest URL
    let channel_items = channel_manifest
        .get("channelItems")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("No channelItems in manifest"))?;

    let vs_manifest_item = channel_items
        .iter()
        .find(|item| {
            item.get("type").and_then(|t| t.as_str()) == Some("Manifest")
                && item.get("id").and_then(|id| id.as_str())
                    == Some("Microsoft.VisualStudio.Manifests.VisualStudio")
        })
        .ok_or_else(|| anyhow!("Could not find VS manifest item"))?;

    let vs_manifest_url = vs_manifest_item
        .get("payloads")
        .and_then(|p| p.as_array())
        .and_then(|arr| arr.first())
        .and_then(|p| p.get("url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("Could not find VS manifest URL"))?;

    tracing::info!("Downloading VS manifest from {}", vs_manifest_url);
    let vs_response = ureq::get(vs_manifest_url)
        .call()
        .map_err(|e| anyhow!("Failed to download VS manifest payload: {}", e))?;
    let vs_manifest: JsonValue = vs_response.into_json()?;

    // Get packages
    let packages = vs_manifest
        .get("packages")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow!("No packages in VS manifest"))?;

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
        bail!("Could not find any MSVC compiler packages");
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

    tracing::info!("Found {} MSVC packages for version {}", all_msvc_packages.len(), msvc_ver);

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
        .map_err(|e| anyhow!("Failed to run msiexec: {}", e))?;

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

        bail!(
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
    cwd: &Path,
    temp_dir: &Path,
    db: &ToolchainDb,
    args: &InstallToolchainsArgs,
) -> anyhow::Result<()> {
    const ZIG_VERSION: &str = "0.15.2";
    const ZIG_PLATFORM: &str = "x86_64-windows";
    const INDEX_URL: &str = "https://ziglang.org/download/index.json";

    let toolchain_name = format!("zig-{}-{}", ZIG_VERSION, ZIG_PLATFORM);

    tracing::info!("Installing Zig toolchain {} for {}", ZIG_VERSION, ZIG_PLATFORM);

    // Download and parse the Zig index
    tracing::info!("Downloading Zig release index from {}", INDEX_URL);
    let response = ureq::get(INDEX_URL).call().map_err(|e| anyhow!("Failed to download Zig index: {}", e))?;
    let index: JsonValue = response.into_json()?;

    // Get the download URL and SHA256 hash for the specified version and platform
    let version_info = index
        .get(ZIG_VERSION)
        .and_then(|v| v.get(ZIG_PLATFORM))
        .ok_or_else(|| anyhow!("No download found for Zig {} {}", ZIG_VERSION, ZIG_PLATFORM))?;

    let tarball_url = version_info
        .get("tarball")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("No tarball URL found"))?;

    let tarball_sha256 =
        version_info.get("shasum").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("No SHA256 hash found"))?;

    tracing::info!("Found download URL: {}", tarball_url);

    // Check if already installed with this hash AND directory exists
    let zig_install_dir = cwd.join("toolchains").join("zig");
    let is_in_database = db.is_toolchain_installed(&toolchain_name, tarball_sha256)?;
    let dir_exists = zig_install_dir.exists();

    if is_in_database && dir_exists {
        tracing::info!(
            "Zig {} is already installed and up-to-date, skipping",
            ZIG_VERSION
        );
        return Ok(());
    }

    // If database says not installed (or wrong hash) but directory exists, delete it
    if !is_in_database && dir_exists {
        tracing::info!(
            "Removing invalid Zig installation at {}",
            zig_install_dir.display()
        );
        fs::remove_dir_all(&zig_install_dir)?;
    }

    // Extract filename from URL (e.g., zig-windows-x86_64-0.15.2.zip)
    let archive_filename =
        tarball_url.split('/').last().ok_or_else(|| anyhow!("Invalid tarball URL: {}", tarball_url))?;
    let archive_path = temp_dir.join(archive_filename);

    // Download archive if not present or if we're not reusing
    if !args.keep_downloads || !archive_path.exists() {
        download_to_path(tarball_url, &archive_path)?;
    } else {
        tracing::info!("Reusing existing download at {}", archive_path.display());
    }

    // Extract to temp directory
    tracing::info!("Extracting archive...");
    let extract_dir = temp_dir.join("extract");
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
        .ok_or_else(|| anyhow!("Could not find extracted Zig directory in temp folder"))?;

    tracing::info!("Found extracted directory: {}", extracted_dir.display());

    // Setup target directory: toolchains/zig/{version}
    let zig_root = cwd.join("toolchains").join("zig").join(ZIG_VERSION);

    if zig_root.exists() {
        tracing::info!("Removing existing installation at {}", zig_root.display());
        fs::remove_dir_all(&zig_root)?;
    }
    fs::create_dir_all(&zig_root)?;

    // Move shared files (lib, etc.) to zig_root
    tracing::info!("Installing shared files to {}", zig_root.display());
    for entry in fs::read_dir(&extracted_dir)? {
        let entry = entry?;
        let entry_path = entry.path();
        let file_name = entry.file_name();

        // Skip zig.exe - we'll handle it separately
        if file_name == "zig.exe" {
            continue;
        }

        let target_path = zig_root.join(&file_name);
        if entry_path.is_dir() {
            copy_dir_recursive(&entry_path, &target_path)?;
        } else {
            fs::copy(&entry_path, &target_path)?;
        }
    }

    // Move zig.exe to bin/windows_x64/zig.exe
    let zig_exe_source = extracted_dir.join("zig.exe");
    if !zig_exe_source.exists() {
        bail!("Could not find zig.exe in extracted archive");
    }

    let bin_dir = zig_root.join("bin").join("windows_x64");
    fs::create_dir_all(&bin_dir)?;
    let zig_exe_dest = bin_dir.join("zig.exe");

    tracing::info!("Installing zig.exe to {}", zig_exe_dest.display());
    fs::copy(&zig_exe_source, &zig_exe_dest)?;

    // Record installation in database
    let install_path_str = zig_root.to_string_lossy().to_string();
    db.record_installation(
        &toolchain_name,
        archive_filename,
        tarball_sha256,
        &install_path_str,
        tarball_sha256, // Use archive hash as installed hash for now
    )?;

    tracing::info!("Successfully installed Zig toolchain at {}", zig_root.display());
    tracing::info!("  - Shared files: {}", zig_root.display());
    tracing::info!("  - Windows binary: {}", zig_exe_dest.display());
    Ok(())
}

fn install_llvm(
    cwd: &Path,
    temp_dir: &Path,
    db: &ToolchainDb,
    args: &InstallToolchainsArgs,
) -> anyhow::Result<()> {
    const LLVM_VERSION: &str = "LLVM 21.1.6";
    const LLVM_PLATFORM_SUFFIX: &str = "x86_64-pc-windows-msvc.tar.xz";
    const RELEASES_URL: &str = "https://api.github.com/repos/llvm/llvm-project/releases";

    let toolchain_name = format!(
        "llvm-21.1.6-{}",
        LLVM_PLATFORM_SUFFIX.strip_suffix(".tar.xz").unwrap_or("x86_64-pc-windows-msvc")
    );

    tracing::info!("Installing LLVM toolchain {}", LLVM_VERSION);

    // Download and parse GitHub releases
    tracing::info!("Downloading LLVM release index from {}", RELEASES_URL);
    let response =
        ureq::get(RELEASES_URL).call().map_err(|e| anyhow!("Failed to download LLVM releases: {}", e))?;
    let releases: Vec<JsonValue> = response.into_json()?;

    // Find the release with the specified name
    let release = releases
        .iter()
        .find(|r| r.get("name").and_then(|n| n.as_str()) == Some(LLVM_VERSION))
        .ok_or_else(|| anyhow!("Could not find release '{}'", LLVM_VERSION))?;

    // Find the asset with the platform-specific suffix
    let assets = release
        .get("assets")
        .and_then(|a| a.as_array())
        .ok_or_else(|| anyhow!("Release has no assets array"))?;

    let asset = assets
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(|n| n.as_str())
                .map(|name| name.ends_with(LLVM_PLATFORM_SUFFIX))
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("Could not find asset ending with '{}'", LLVM_PLATFORM_SUFFIX))?;

    let download_url = asset
        .get("browser_download_url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("Asset has no browser_download_url"))?;

    let asset_name =
        asset.get("name").and_then(|n| n.as_str()).ok_or_else(|| anyhow!("Asset has no name"))?;

    tracing::info!("Found LLVM download: {}", download_url);

    // Download archive if not present or if we're not reusing
    let archive_path = temp_dir.join(asset_name);
    if !args.keep_downloads || !archive_path.exists() {
        download_to_path(download_url, &archive_path)?;
    } else {
        tracing::info!("Reusing existing download at {}", archive_path.display());
    }

    // Compute SHA256 of the archive to track in database
    tracing::info!("Computing SHA256 hash of downloaded archive...");
    let archive_sha256 = compute_file_sha256(&archive_path)?;

    // Check if already installed with this hash AND directory exists
    let llvm_install_dir = cwd.join("toolchains").join("llvm");
    let is_in_database = db.is_toolchain_installed(&toolchain_name, &archive_sha256)?;
    let dir_exists = llvm_install_dir.exists();

    if is_in_database && dir_exists {
        tracing::info!(
            "LLVM {} is already installed and up-to-date, skipping",
            LLVM_VERSION
        );
        return Ok(());
    }

    // If database says not installed (or wrong hash) but directory exists, delete it
    if !is_in_database && dir_exists {
        tracing::info!(
            "Removing invalid LLVM installation at {}",
            llvm_install_dir.display()
        );
        fs::remove_dir_all(&llvm_install_dir)?;
    }

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
        .ok_or_else(|| anyhow!("Could not find extracted LLVM directory"))?;

    tracing::info!("Found extracted directory: {}", extracted_dir.display());

    // Extract the platform suffix from the asset name (e.g., "x86_64-pc-windows-msvc")
    // Asset name is like: clang+llvm-21.1.6-x86_64-pc-windows-msvc.tar.xz
    let platform_suffix = asset_name
        .strip_suffix(".tar.xz")
        .and_then(|name| {
            // Find the last occurrence of a version-like pattern and take everything after it
            // Looking for pattern like "21.1.6-" and taking what comes after
            let parts: Vec<&str> = name.split('-').collect();
            // Find where the version ends (typically after something like "21.1.6")
            // Then take the remaining parts
            if parts.len() >= 3 {
                // Try to find index where platform starts (after version)
                for i in 0..parts.len() {
                    if parts[i].chars().all(|c| c.is_numeric() || c == '.') && i + 1 < parts.len() {
                        // Found version part, join remaining parts
                        return Some(parts[i + 1..].join("-"));
                    }
                }
            }
            None
        })
        .ok_or_else(|| {
            anyhow!(
                "Could not extract platform suffix from asset name: {}",
                asset_name
            )
        })?;

    tracing::info!("Using platform suffix: {}", platform_suffix);

    // Setup target directory: toolchains/llvm/{platform_suffix}
    let llvm_root = cwd.join("toolchains").join("llvm").join(&platform_suffix);
    if llvm_root.exists() {
        tracing::info!("Removing existing installation at {}", llvm_root.display());
        fs::remove_dir_all(&llvm_root)?;
    }

    // Move extracted directory to final location
    fs::create_dir_all(llvm_root.parent().unwrap())?;
    fs::rename(&extracted_dir, &llvm_root)?;

    // Deduplicate files in bin directory
    let bin_dir = llvm_root.join("bin");
    if bin_dir.exists() && bin_dir.is_dir() {
        tracing::info!("Deduplicating files in {}", bin_dir.display());
        deduplicate_files(&bin_dir)?;
    }

    // Record installation in database
    let install_path_str = llvm_root.to_string_lossy().to_string();
    db.record_installation(
        &toolchain_name,
        asset_name,
        &archive_sha256,
        &install_path_str,
        &archive_sha256, // Use archive hash as installed hash
    )?;

    tracing::info!("Successfully installed LLVM toolchain at {}", llvm_root.display());
    Ok(())
}

fn install_nasm(
    cwd: &Path,
    temp_dir: &Path,
    db: &ToolchainDb,
    args: &InstallToolchainsArgs,
) -> anyhow::Result<()> {
    const NASM_VERSION: &str = "3.01";
    const NASM_URL: &str = "https://www.nasm.us/pub/nasm/releasebuilds/3.01/win64/nasm-3.01-win64.zip";

    let toolchain_name = format!("nasm-{}-win64", NASM_VERSION);
    let archive_filename = format!("nasm-{}-win64.zip", NASM_VERSION);

    tracing::info!("Installing NASM assembler {}", NASM_VERSION);

    let nasm_install_dir = cwd.join("toolchains").join("nasm").join("win64");
    let archive_path = temp_dir.join(&archive_filename);

    // Download archive if not present
    if !archive_path.exists() || !args.keep_downloads {
        download_to_path(NASM_URL, &archive_path)?;
    } else {
        tracing::info!("Reusing existing download at {}", archive_path.display());
    }

    // Compute SHA256 of the downloaded archive for tracking
    // (NASM doesn't publish hashes, so we compute after download)
    tracing::info!("Computing SHA256 hash of downloaded archive...");
    let archive_sha256 = compute_file_sha256(&archive_path)?;

    // Check if already installed with this hash AND directory exists
    let is_in_database = db.is_toolchain_installed(&toolchain_name, &archive_sha256)?;
    let dir_exists = nasm_install_dir.exists();

    if is_in_database && dir_exists {
        tracing::info!(
            "NASM {} is already installed and up-to-date, skipping",
            NASM_VERSION
        );
        return Ok(());
    }

    // If database says not installed (or wrong hash) but directory exists, delete it
    if !is_in_database && dir_exists {
        tracing::info!(
            "Removing invalid NASM installation at {}",
            nasm_install_dir.display()
        );
        fs::remove_dir_all(&nasm_install_dir)?;
    }

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
        .ok_or_else(|| anyhow!("Could not find extracted NASM directory in temp folder"))?;

    tracing::info!("Found extracted directory: {}", extracted_dir.display());

    // Setup target directory: toolchains/nasm/win64
    let nasm_root = cwd.join("toolchains").join("nasm").join("win64");

    if nasm_root.exists() {
        tracing::info!("Removing existing installation at {}", nasm_root.display());
        fs::remove_dir_all(&nasm_root)?;
    }

    // Move extracted directory to final location
    fs::create_dir_all(nasm_root.parent().unwrap())?;
    fs::rename(&extracted_dir, &nasm_root)?;

    // Record installation in database
    let install_path_str = nasm_root.to_string_lossy().to_string();
    db.record_installation(
        &toolchain_name,
        &archive_filename,
        &archive_sha256,
        &install_path_str,
        &archive_sha256,
    )?;

    tracing::info!("Successfully installed NASM at {}", nasm_root.display());

    // Log location of nasm.exe
    let nasm_exe = nasm_root.join("nasm.exe");
    if nasm_exe.exists() {
        tracing::info!("  - nasm.exe: {}", nasm_exe.display());
    }

    Ok(())
}

fn install_msvc(
    cwd: &Path,
    temp_dir: &Path,
    db: &ToolchainDb,
    args: &InstallToolchainsArgs,
) -> anyhow::Result<()> {
    tracing::info!("Installing MSVC toolchain and Windows SDK");

    // Download VS manifest
    const MANIFEST_URL: &str = "https://aka.ms/vs/18/stable/channel";

    tracing::info!("Downloading Visual Studio manifest from {}", MANIFEST_URL);
    let response =
        ureq::get(MANIFEST_URL).call().map_err(|e| anyhow!("Failed to download VS manifest: {}", e))?;
    let channel_manifest: JsonValue = response.into_json()?;

    // Find the channelItems and get the VS manifest URL
    let channel_items = channel_manifest
        .get("channelItems")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("No channelItems in manifest"))?;

    let vs_manifest_item = channel_items
        .iter()
        .find(|item| {
            item.get("type").and_then(|t| t.as_str()) == Some("Manifest")
                && item.get("id").and_then(|id| id.as_str())
                    == Some("Microsoft.VisualStudio.Manifests.VisualStudio")
        })
        .ok_or_else(|| anyhow!("Could not find VS manifest item"))?;

    let vs_manifest_url = vs_manifest_item
        .get("payloads")
        .and_then(|p| p.as_array())
        .and_then(|arr| arr.first())
        .and_then(|p| p.get("url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("Could not find VS manifest URL"))?;

    tracing::info!("Downloading VS manifest from {}", vs_manifest_url);
    let vs_response = ureq::get(vs_manifest_url)
        .call()
        .map_err(|e| anyhow!("Failed to download VS manifest payload: {}", e))?;
    let vs_manifest: JsonValue = vs_response.into_json()?;

    // Get packages
    let packages = vs_manifest
        .get("packages")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow!("No packages in VS manifest"))?;

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
        bail!(
            "Could not find any MSVC compiler packages for host={} target={}",
            HOST,
            TARGET
        );
    }

    // Sort by version (lexicographically) and take the latest
    msvc_candidates.sort_by(|a, b| b.0.cmp(&a.0)); // Reverse sort for latest first
    let (msvc_ver, msvc_package_id) = &msvc_candidates[0];
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
        bail!("Could not find any Windows SDK packages");
    }

    // Sort and take the latest
    sdk_candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let (sdk_ver, sdk_package_id) = &sdk_candidates[0];
    tracing::info!(
        "Selected Windows SDK version: {} (from package {})",
        sdk_ver,
        sdk_package_id
    );

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
        Err(anyhow!("Package not found: {}", pkg_id))
    };

    // Add MSVC packages - use the exact package ID we found
    add_package(&mut downloads, msvc_package_id)?;

    // Add related MSVC packages for the same version
    let target_lower = TARGET.to_lowercase();
    add_package(
        &mut downloads,
        &format!("Microsoft.VC.{}.CRT.Headers.base", msvc_ver),
    )?;
    add_package(
        &mut downloads,
        &format!("Microsoft.VC.{}.CRT.{}.Desktop.base", msvc_ver, target_lower),
    )?;
    // Add CRT Store package which contains additional libs like oldnames.lib
    add_package(
        &mut downloads,
        &format!("Microsoft.VC.{}.CRT.{}.Store.base", msvc_ver, target_lower),
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

    // Create toolchain name for MSVC only
    let toolchain_name = format!("msvc-{}", msvc_ver);

    // Check if MSVC is already installed AND directory exists
    let msvc_install_dir = cwd.join("toolchains").join("msvc");
    let is_in_database = db.is_toolchain_installed(&toolchain_name, &installation_hash)?;
    let dir_exists = msvc_install_dir.exists();

    if is_in_database && dir_exists {
        tracing::info!("MSVC {} is already installed and up-to-date, skipping", msvc_ver);
        // Still install SDK if needed
        install_windows_sdk(cwd, temp_dir, db, packages, sdk_package_id, sdk_ver, args)?;
        return Ok(());
    }

    // If database says not installed (or wrong hash) but directory exists, delete it
    if !is_in_database && dir_exists {
        tracing::info!(
            "Removing invalid MSVC installation at {}",
            msvc_install_dir.display()
        );
        fs::remove_dir_all(&msvc_install_dir)?;
    }

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

    // Extract packages
    let msvc_root = cwd.join("toolchains").join("msvc");
    if msvc_root.exists() {
        fs::remove_dir_all(&msvc_root)?;
    }
    fs::create_dir_all(&msvc_root)?;

    tracing::info!("Extracting packages to {}", msvc_root.display());

    // Extract packages
    for (_url, _sha256, filename) in &downloads {
        let file_path = temp_dir.join(filename);

        // VSIX files are ZIP files
        if filename.ends_with(".vsix") || filename.ends_with(".zip") {
            tracing::info!("Extracting VSIX: {}", filename);
            extract_vsix_zip(&file_path, &msvc_root)?;
        } else if filename.ends_with(".msi") {
            // Extract MSI using msiexec on Windows
            #[cfg(windows)]
            {
                tracing::info!("Extracting MSI: {}", filename);
                extract_msi(&file_path, &msvc_root)?;
            }
        } else {
            tracing::warn!("Skipping unknown file type: {}", filename);
        }
    }

    // Record installation in database
    let install_path_str = msvc_root.to_string_lossy().to_string();
    let archive_list =
        downloads.iter().map(|(_url, _sha256, filename)| filename.as_str()).collect::<Vec<_>>().join(", ");

    db.record_installation(
        &toolchain_name,
        &archive_list,
        &installation_hash,
        &install_path_str,
        &installation_hash,
    )?;

    tracing::info!("Successfully installed MSVC toolchain at {}", msvc_root.display());

    // Install Windows SDK
    install_windows_sdk(cwd, temp_dir, db, packages, sdk_package_id, sdk_ver, args)?;

    Ok(())
}

fn install_windows_sdk(
    cwd: &Path,
    temp_dir: &Path,
    db: &ToolchainDb,
    packages: &[JsonValue],
    sdk_package_id: &str,
    sdk_ver: &str,
    args: &InstallToolchainsArgs,
) -> anyhow::Result<()> {
    tracing::info!("Installing Windows SDK {} to separate directory", sdk_ver);

    // Find the SDK component package - this is a meta-package with dependencies
    let sdk_component = packages
        .iter()
        .find(|p| p.get("id").and_then(|id| id.as_str()) == Some(sdk_package_id))
        .ok_or_else(|| anyhow!("Could not find SDK component {}", sdk_package_id))?;

    // Get dependencies from the SDK component
    let dependencies = sdk_component
        .get("dependencies")
        .and_then(|d| d.as_object())
        .ok_or_else(|| anyhow!("SDK component has no dependencies"))?;

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

    // Create toolchain name for SDK
    let toolchain_name = format!("windows-sdk-{}", sdk_ver);

    // Check if SDK is already installed AND directory exists
    let sdk_install_dir = cwd.join("toolchains").join("windows_kits");
    let is_in_database = db.is_toolchain_installed(&toolchain_name, &installation_hash)?;
    let dir_exists = sdk_install_dir.exists();

    if is_in_database && dir_exists {
        tracing::info!(
            "Windows SDK {} is already installed and up-to-date, skipping",
            sdk_ver
        );
        return Ok(());
    }

    // If database says not installed (or wrong hash) but directory exists, delete it
    if !is_in_database && dir_exists {
        tracing::info!(
            "Removing invalid Windows SDK installation at {}",
            sdk_install_dir.display()
        );
        fs::remove_dir_all(&sdk_install_dir)?;
    }

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

    // Move extracted contents to final location
    // MSI extracts to: sdk_temp/Windows Kits/10/*
    // We want: toolchains/windows_kits/*
    let sdk_final = cwd.join("toolchains").join("windows_kits");
    if sdk_final.exists() {
        fs::remove_dir_all(&sdk_final)?;
    }
    fs::create_dir_all(&sdk_final)?;

    let windows_kits_extracted = sdk_temp.join("Windows Kits").join("10");
    if windows_kits_extracted.exists() {
        tracing::info!("Moving SDK contents from temp to {}", sdk_final.display());

        // Move all contents from "Windows Kits/10/*" to "toolchains/windows_kits/*"
        for entry in fs::read_dir(&windows_kits_extracted)? {
            let entry = entry?;
            let dest = sdk_final.join(entry.file_name());
            tracing::debug!("Moving {} to {}", entry.path().display(), dest.display());
            fs::rename(entry.path(), dest)?;
        }
    } else {
        bail!("Expected Windows Kits/10 directory not found after MSI extraction");
    }

    // Clean up temp extraction directory
    tracing::info!("Cleaning up SDK temp directory");
    fs::remove_dir_all(&sdk_temp)?;

    // Record installation in database
    let install_path_str = sdk_final.to_string_lossy().to_string();
    let archive_list = all_payloads
        .iter()
        .filter(|(_url, _sha256, filename)| filename.ends_with(".msi"))
        .map(|(_url, _sha256, filename)| filename.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    db.record_installation(
        &toolchain_name,
        &archive_list,
        &installation_hash,
        &install_path_str,
        &installation_hash,
    )?;

    tracing::info!("Successfully installed Windows SDK at {}", sdk_final.display());
    Ok(())
}

fn download_to_path(url: &str, destination: &Path) -> anyhow::Result<()> {
    tracing::info!("Downloading {} -> {}", url, destination.display());
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let response = ureq::get(url).call().map_err(|err| anyhow!("Failed to download {}: {}", url, err))?;

    if response.status() >= 400 {
        bail!("Failed to download {}: HTTP {}", url, response.status());
    }

    let mut reader = response.into_reader();
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
            .ok_or_else(|| anyhow!("Archive entry has invalid path: {}", entry.name()))?;
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

fn deduplicate_files(dir: &Path) -> anyhow::Result<()> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

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

fn copy_dir_recursive(source: &Path, destination: &Path) -> anyhow::Result<()> {
    if !source.is_dir() {
        bail!("Source {} is not a directory", source.display());
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

    let response = ureq::get(url).call().map_err(|err| anyhow!("Failed to download {}: {}", url, err))?;

    if response.status() >= 400 {
        bail!("Failed to download {}: HTTP {}", url, response.status());
    }

    let mut reader = response.into_reader();
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
        bail!(
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
fn extract_msi(msi_path: &Path, destination: &Path) -> anyhow::Result<()> {
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
        .map_err(|e| anyhow!("Failed to run msiexec: {}", e))?;

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

        bail!(
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
