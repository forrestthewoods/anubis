use anyhow::{anyhow, bail};
use clap::Parser;
use serde_json::Value as JsonValue;
use std::env;
use std::fs::{self, File};
use std::io::Read;
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

    // Extract to toolchains/llvm
    let llvm_root = cwd.join("toolchains").join("llvm");
    if llvm_root.exists() {
        tracing::info!("Removing existing installation at {}", llvm_root.display());
        fs::remove_dir_all(&llvm_root)?;
    }
    fs::create_dir_all(&llvm_root)?;

    tracing::info!("Extracting LLVM archive to {}", llvm_root.display());
    extract_tar_xz(&archive_path, &llvm_root)?;

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
