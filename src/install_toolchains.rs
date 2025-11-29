use anyhow::{anyhow, bail};
use clap::Parser;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use tar::Archive;
use xz2::read::XzDecoder;
use zip::ZipArchive;

#[derive(Debug, Parser)]
pub struct InstallToolchainsArgs {
    /// Keep downloaded files and reuse them if present
    #[arg(long)]
    pub keep_downloads: bool,
}

pub fn install_toolchains(args: &InstallToolchainsArgs) -> anyhow::Result<()> {
    // Flush DNS cache on Windows to avoid DNS issues
    #[cfg(windows)]
    {
        use std::process::Command;
        tracing::info!("Flushing DNS cache on Windows");
        let output = Command::new("ipconfig")
            .arg("/flushdns")
            .output();

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

    // Use a temp directory relative to the project to avoid env var issues
    let temp_dir = cwd.join(".anubis-temp");
    if !args.keep_downloads && temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)?;
    }
    fs::create_dir_all(&temp_dir)?;

    // Install both toolchains
    install_zig(&cwd, &temp_dir, args)?;
    install_llvm(&cwd, &temp_dir, args)?;

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

fn install_zig(cwd: &Path, temp_dir: &Path, args: &InstallToolchainsArgs) -> anyhow::Result<()> {
    const ZIG_VERSION: &str = "0.15.2";
    const ZIG_PLATFORM: &str = "x86_64-windows";
    const INDEX_URL: &str = "https://ziglang.org/download/index.json";

    tracing::info!("Installing Zig toolchain {} for {}", ZIG_VERSION, ZIG_PLATFORM);

    // Download and parse the Zig index
    tracing::info!("Downloading Zig release index from {}", INDEX_URL);
    let response = ureq::get(INDEX_URL)
        .call()
        .map_err(|e| anyhow!("Failed to download Zig index: {}", e))?;
    let index: JsonValue = response.into_json()?;

    // Get the download URL for the specified version and platform
    let tarball_url = index
        .get(ZIG_VERSION)
        .and_then(|v| v.get(ZIG_PLATFORM))
        .and_then(|v| v.get("tarball"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("No download found for Zig {} {}", ZIG_VERSION, ZIG_PLATFORM))?;

    tracing::info!("Found download URL: {}", tarball_url);

    // Extract filename from URL (e.g., zig-windows-x86_64-0.15.2.zip)
    let archive_filename = tarball_url
        .split('/')
        .last()
        .ok_or_else(|| anyhow!("Invalid tarball URL: {}", tarball_url))?;
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

    tracing::info!("Successfully installed Zig toolchain at {}", zig_root.display());
    tracing::info!("  - Shared files: {}", zig_root.display());
    tracing::info!("  - Windows binary: {}", zig_exe_dest.display());
    Ok(())
}

fn install_llvm(cwd: &Path, temp_dir: &Path, args: &InstallToolchainsArgs) -> anyhow::Result<()> {
    const LLVM_VERSION: &str = "LLVM 21.1.6";
    const LLVM_PLATFORM_SUFFIX: &str = "x86_64-pc-windows-msvc.tar.xz";
    const RELEASES_URL: &str = "https://api.github.com/repos/llvm/llvm-project/releases";

    tracing::info!("Installing LLVM toolchain {}", LLVM_VERSION);

    // Download and parse GitHub releases
    tracing::info!("Downloading LLVM release index from {}", RELEASES_URL);
    let response = ureq::get(RELEASES_URL)
        .call()
        .map_err(|e| anyhow!("Failed to download LLVM releases: {}", e))?;
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

    let asset_name = asset
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| anyhow!("Asset has no name"))?;

    tracing::info!("Found LLVM download: {}", download_url);

    // Download archive if not present or if we're not reusing
    let archive_path = temp_dir.join(asset_name);
    if !args.keep_downloads || !archive_path.exists() {
        download_to_path(download_url, &archive_path)?;
    } else {
        tracing::info!("Reusing existing download at {}", archive_path.display());
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
        .ok_or_else(|| anyhow!("Could not extract platform suffix from asset name: {}", asset_name))?;

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

    tracing::info!("Successfully installed LLVM toolchain at {}", llvm_root.display());
    Ok(())
}

fn download_to_path(url: &str, destination: &Path) -> anyhow::Result<()> {
    tracing::info!("Downloading {} -> {}", url, destination.display());
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let response = ureq::get(url)
        .call()
        .map_err(|err| anyhow!("Failed to download {}: {}", url, err))?;

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
