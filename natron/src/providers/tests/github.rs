//! Tests for `src/providers/github.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
use std::fs;
use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use tempfile::TempDir;

fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let f = File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::FileOptions::default();
    for (name, bytes) in entries {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(bytes).unwrap();
    }
    zw.finish().unwrap();
}

/// Place `archive` where the provider expects a release asset:
/// `<base_dir>/{repo}/releases/download/{tag}/{asset}`.
fn place_asset(base_dir: &Path, repo: &str, tag: &str, asset: &str, archive: &Path) {
    let dst = base_dir
        .join(repo)
        .join("releases")
        .join("download")
        .join(tag)
        .join(asset);
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    fs::copy(archive, &dst).unwrap();
}

fn download_base(dir: &Path) -> String {
    url::Url::from_directory_path(dir)
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_string()
}

#[test]
fn github_provider_id() {
    assert_eq!(GithubProvider::new().id(), "github");
}

#[test]
fn github_provider_resolves_release_and_extracts() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("dl");
    let archive = tmp.path().join("clang.zip");
    build_zip(&archive, &[("bin/clang.exe", b"BIN"), ("LICENSE", b"LIC")]);
    place_asset(&base, "llvm/llvm-project", "llvmorg-21.1.6", "clang.zip", &archive);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String("llvm/llvm-project".into()));
    opts.insert("tag".into(), toml::Value::String("llvmorg-21.1.6".into()));
    opts.insert("asset".into(), toml::Value::String("clang.zip".into()));
    opts.insert("sha256".into(), toml::Value::String(crate::download::sha256_of_file(&archive).unwrap()));

    let provider = GithubProvider::with_download_base(download_base(&base));
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(installed.freshly_extracted);
    assert!(installed.fingerprint.contains("llvmorg-21.1.6"));

    let raw = ctx.staging_dir().unwrap();
    assert_eq!(fs::read(raw.join("bin").join("clang.exe")).unwrap(), b"BIN");
}

#[test]
fn github_provider_missing_asset_errors() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("dl");
    // Place a different asset; the one we request is absent.
    let archive = tmp.path().join("real.zip");
    build_zip(&archive, &[("f", b"F")]);
    place_asset(&base, "owner/repo", "v1", "real.zip", &archive);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String("owner/repo".into()));
    opts.insert("tag".into(), toml::Value::String("v1".into()));
    opts.insert("asset".into(), toml::Value::String("does-not-exist.zip".into()));
    opts.insert("sha256".into(), toml::Value::String("0".repeat(64)));

    let provider = GithubProvider::with_download_base(download_base(&base));
    let err = provider.install(&opts, &mut ctx).unwrap_err();
    assert!(
        format!("{err:#}").contains("does-not-exist.zip"),
        "got: {err:#}"
    );
}

#[test]
fn github_provider_required_fields() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let opts = toml::Table::new();
    let err = GithubProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("repo"));
}

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn fp(tag: &str, sha: &str, strip: Option<&str>, kind: Option<ArchiveKind>) -> String {
    compute_fingerprint("llvm/llvm-project", tag, "clang.tar.xz", sha, strip, kind)
}

#[test]
fn github_fingerprint_deterministic() {
    let xz = Some(ArchiveKind::TarXz);
    let a = fp("llvmorg-21.1.6", SHA_A, None, xz);
    assert_eq!(a, fp("llvmorg-21.1.6", SHA_A, None, xz));
    assert_ne!(a, fp("llvmorg-18.1.8", SHA_A, None, xz));
    // The hash's case is not part of the pin.
    assert_eq!(a, fp("llvmorg-21.1.6", &SHA_A.to_ascii_uppercase(), None, xz));
}

/// #197 / #239: every option that changes the installed tree changes the
/// fingerprint, so editing it in natron.toml reinstalls.
#[test]
fn github_fingerprint_covers_sha256_strip_prefix_and_archive() {
    let xz = Some(ArchiveKind::TarXz);
    let a = fp("t", SHA_A, None, xz);
    assert_ne!(a, fp("t", SHA_B, None, xz), "sha256");
    assert_ne!(a, fp("t", SHA_A, Some("clang"), xz), "strip_prefix");
    assert_ne!(a, fp("t", SHA_A, None, Some(ArchiveKind::TarGz)), "archive kind");
    assert_ne!(a, fp("t", SHA_A, None, None), "archive = \"file\"");
}

fn plant_install(cache: &Cache, fingerprint: &str, opts: &toml::Table) {
    let sanitized = crate::cache::sanitize_fingerprint(fingerprint);
    fs::create_dir_all(cache.install_dir(&sanitized).join("tree")).unwrap();
    crate::cache::InstallMetadata::new("github", sanitized.clone(), "test", opts.clone())
        .write(&cache.install_metadata_path(&sanitized))
        .unwrap();
}

/// #197: an install made from other bytes (an older pin, or an upstream asset
/// replaced under the same tag) is not reused when `sha256` changes.
#[test]
fn github_new_sha256_does_not_reuse_the_old_install() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String("x/y".into()));
    opts.insert("tag".into(), toml::Value::String("v".into()));
    opts.insert("asset".into(), toml::Value::String("a.zip".into()));
    opts.insert("sha256".into(), toml::Value::String(SHA_A.into()));
    plant_install(&cache, &compute_fingerprint("x/y", "v", "a.zip", SHA_A, None, Some(ArchiveKind::Zip)), &opts);

    opts.insert("sha256".into(), toml::Value::String(SHA_B.into()));
    let mut ctx = InstallCtx::new(cache);
    let provider = GithubProvider::with_download_base("file:///never/exists");
    let err = provider.install(&opts, &mut ctx).unwrap_err();
    assert!(format!("{err:#}").contains("downloading"), "expected a download attempt, got: {err:#}");
}

#[test]
fn github_archive_kind_inference() {
    let mk = |asset: &str| {
        let opts = toml::Table::new();
        resolve_archive_kind(&opts, asset)
    };
    assert_eq!(mk("foo.tar.xz").unwrap(), ArchiveKind::TarXz);
    assert_eq!(mk("foo.zip").unwrap(), ArchiveKind::Zip);
    assert!(mk("foo.exe").is_err());
}

#[test]
fn github_cache_hit_short_circuits() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String("x/y".into()));
    opts.insert("tag".into(), toml::Value::String("v".into()));
    opts.insert("asset".into(), toml::Value::String("a.zip".into()));
    opts.insert("sha256".into(), toml::Value::String(SHA_A.into()));

    // Pre-plant the install dir + metadata to simulate a prior install.
    plant_install(&cache, &compute_fingerprint("x/y", "v", "a.zip", SHA_A, None, Some(ArchiveKind::Zip)), &opts);

    let mut ctx = InstallCtx::new(cache);
    // Download base points nowhere real — a cache hit must not touch it.
    let provider = GithubProvider::with_download_base("file:///never/exists");
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(!installed.freshly_extracted, "should have hit cache");
    // No staging dir should have been allocated.
    assert!(ctx.staging_root().is_none());
}

#[test]
fn github_provider_installs_a_bare_file() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("dl");
    let exe = tmp.path().join("radlink.exe");
    fs::write(&exe, b"MZ fake exe").unwrap();
    place_asset(&base, "EpicGamesExt/raddebugger", "v0.9.29-alpha", "radlink.exe", &exe);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String("EpicGamesExt/raddebugger".into()));
    opts.insert("tag".into(), toml::Value::String("v0.9.29-alpha".into()));
    opts.insert("asset".into(), toml::Value::String("radlink.exe".into()));
    opts.insert("archive".into(), toml::Value::String("file".into()));
    opts.insert("sha256".into(), toml::Value::String(crate::download::sha256_of_file(&exe).unwrap()));

    let provider = GithubProvider::with_download_base(download_base(&base));
    assert!(provider.install(&opts, &mut ctx).unwrap().freshly_extracted);
    let raw = ctx.staging_dir().unwrap();
    assert_eq!(fs::read(raw.join("radlink.exe")).unwrap(), b"MZ fake exe");
}
