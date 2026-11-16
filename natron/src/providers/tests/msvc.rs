//! Tests for `src/providers/msvc.rs`.

use super::*;
use crate::providers::vs_manifest::tests::{
    build_vsix, file_url, pkg, pkg_with_lang, pkg_with_payload, test_ctx, FxSnapshot,
    MirrorFixture, TEST_COMMIT,
};
use tempfile::TempDir;

// ---- Options::parse --------------------------------------------------------

#[test]
fn options_require_build_version() {
    let opts = toml::Table::new();
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("build_version"), "got: {err}");
}

#[test]
fn options_reject_unknown_major() {
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("15.0.0.0".into()),
    );
    opts.insert("manifest_commit".into(), toml::Value::String(TEST_COMMIT.into()));
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("15"), "got: {err}");
}

#[test]
fn options_default_base_is_default() {
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("18.6.11819.183".into()),
    );
    opts.insert("manifest_commit".into(), toml::Value::String(TEST_COMMIT.into()));
    let parsed = Options::parse(&opts).unwrap();
    assert_eq!(parsed.base, BaseInstall::Default);
}

#[test]
fn options_none_with_empty_extras_rejected() {
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("18.6.11819.183".into()),
    );
    opts.insert("manifest_commit".into(), toml::Value::String(TEST_COMMIT.into()));
    opts.insert(
        "base_install".into(),
        toml::Value::String("none".into()),
    );
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("nothing"), "got: {err}");
}

#[test]
fn options_full_with_extras_rejected() {
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("18.6.11819.183".into()),
    );
    opts.insert("manifest_commit".into(), toml::Value::String(TEST_COMMIT.into()));
    opts.insert(
        "base_install".into(),
        toml::Value::String("full".into()),
    );
    opts.insert(
        "extras".into(),
        toml::Value::Array(vec![toml::Value::String("ATL.*".into())]),
    );
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("full"), "got: {err}");
}

// ---- find_primary_compiler -------------------------------------------------

#[test]
fn primary_picks_highest_matching_vs_major() {
    let m: VsManifest = serde_json::from_str(&format!(
        r#"{{"packages":[
            {a},
            {b},
            {c}
        ]}}"#,
        a = pkg("Microsoft.VC.14.50.18.0.Tools.HostX64.TargetX64.base", "14.50.35731"),
        b = pkg("Microsoft.VC.14.52.18.5.Tools.HostX64.TargetX64.base", "14.52.36328"),
        c = pkg("Microsoft.VC.14.51.18.4.Tools.HostX64.TargetX64.base", "14.51.36244"),
    ))
    .unwrap();
    let primary = find_primary_compiler(&m, VsVersion::Vs2026).unwrap();
    assert_eq!(primary.id, "Microsoft.VC.14.52.18.5.Tools.HostX64.TargetX64.base");
}

#[test]
fn primary_excludes_premium_and_other_majors() {
    let m: VsManifest = serde_json::from_str(&format!(
        r#"{{"packages":[
            {a}, {b}, {c}, {d}
        ]}}"#,
        a = pkg("Microsoft.VC.14.52.18.5.Premium.Tools.HostX64.TargetX64.base", "14.52.36328"),
        b = pkg("Microsoft.VC.14.41.17.11.Tools.HostX64.TargetX64.base", "14.41.34123"),
        c = pkg("Microsoft.VisualC.14.16.Tools.HostX64.TargetX64", "14.16.27054"),
        d = pkg("Microsoft.VC.14.50.18.0.Tools.HostX64.TargetX64.base", "14.50.35731"),
    ))
    .unwrap();
    let primary = find_primary_compiler(&m, VsVersion::Vs2026).unwrap();
    assert_eq!(primary.id, "Microsoft.VC.14.50.18.0.Tools.HostX64.TargetX64.base");
}

#[test]
fn primary_errors_when_no_match_for_channel() {
    // Only a vs2022-era package; asking for vs2026 should fail.
    let m: VsManifest = serde_json::from_str(&format!(
        r#"{{"packages":[{a}]}}"#,
        a = pkg("Microsoft.VC.14.41.17.11.Tools.HostX64.TargetX64.base", "14.41.34123"),
    ))
    .unwrap();
    let err = find_primary_compiler(&m, VsVersion::Vs2026).unwrap_err();
    assert!(err.to_string().contains("18"), "got: {err}");
}

#[test]
fn family_prefix_reads_compiler_id() {
    assert_eq!(
        family_prefix("Microsoft.VC.14.52.18.5.Tools.HostX64.TargetX64.base").unwrap(),
        "Microsoft.VC.14.52.18.5."
    );
    assert!(family_prefix("Microsoft.Random.Package").is_err());
}

// ---- selection -------------------------------------------------------------

#[test]
fn default_selection_covers_compiler_crt_redist() {
    let m = full_fixture_manifest("14.52.18.5", "14.52.36328");
    let compiler = find_primary_compiler(&m, VsVersion::Vs2026).unwrap();
    let family = family_prefix(&compiler.id).unwrap();
    let opts = parsed(&[("build_version", "18.6.11819.183")]);

    let selected = select_packages(&m, &family, &opts).unwrap();
    let ids: Vec<&str> = selected.iter().map(|r| r.id.as_str()).collect();

    assert!(ids.iter().any(|s| s.contains(".Tools.HostX64.TargetX64.base")));
    assert!(ids.iter().any(|s| s.ends_with(".CRT.Headers.base")));
    assert!(ids.iter().any(|s| s.ends_with(".CRT.x64.Desktop.base")));
    assert!(ids.iter().any(|s| s.ends_with(".CRT.x64.Store.base")));
    assert!(ids.iter().any(|s| s.ends_with(".CRT.Redist.X64.base")));
    // ATL is NOT in default.
    assert!(!ids.iter().any(|s| s.contains("ATL")));
}

#[test]
fn full_selection_includes_everything_in_snapshot() {
    let m = full_fixture_manifest("14.52.18.5", "14.52.36328");
    let compiler = find_primary_compiler(&m, VsVersion::Vs2026).unwrap();
    let family = family_prefix(&compiler.id).unwrap();
    let opts = parsed(&[
        ("build_version", "18.6.11819.183"),
        ("base_install", "full"),
    ]);

    let selected = select_packages(&m, &family, &opts).unwrap();
    let ids: Vec<&str> = selected.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.iter().any(|s| s.contains("ATL")));
    assert!(ids.iter().any(|s| s.contains("Preview")));
}

#[test]
fn none_with_extras_selects_only_extras() {
    let m = full_fixture_manifest("14.52.18.5", "14.52.36328");
    let compiler = find_primary_compiler(&m, VsVersion::Vs2026).unwrap();
    let family = family_prefix(&compiler.id).unwrap();
    let opts = parsed_with_extras(
        &[("build_version", "18.6.11819.183"), ("base_install", "none")],
        &["ATL.X64.base"],
    );

    let selected = select_packages(&m, &family, &opts).unwrap();
    let ids: Vec<&str> = selected.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.iter().any(|s| s.ends_with(".ATL.X64.base")));
    assert!(!ids.iter().any(|s| s.contains(".Tools.HostX64")));
}

#[test]
fn extras_additive_on_top_of_default() {
    let m = full_fixture_manifest("14.52.18.5", "14.52.36328");
    let compiler = find_primary_compiler(&m, VsVersion::Vs2026).unwrap();
    let family = family_prefix(&compiler.id).unwrap();
    let opts = parsed_with_extras(
        &[("build_version", "18.6.11819.183")],
        &["ATL.X64.base"],
    );

    let selected = select_packages(&m, &family, &opts).unwrap();
    let ids: Vec<&str> = selected.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.iter().any(|s| s.ends_with(".ATL.X64.base")));
    assert!(ids.iter().any(|s| s.ends_with(".CRT.Headers.base")));
}

#[test]
fn extras_raw_microsoft_prefix_targets_outside_family() {
    let m = full_fixture_manifest("14.52.18.5", "14.52.36328");
    let compiler = find_primary_compiler(&m, VsVersion::Vs2026).unwrap();
    let family = family_prefix(&compiler.id).unwrap();
    let opts = parsed_with_extras(
        &[("build_version", "18.6.11819.183"), ("base_install", "none")],
        &["Microsoft.VC.Preview.DIA.*"],
    );

    let selected = select_packages(&m, &family, &opts).unwrap();
    let ids: Vec<&str> = selected.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"Microsoft.VC.Preview.DIA.SDK"));
}

#[test]
fn extras_zero_match_errors() {
    let m = full_fixture_manifest("14.52.18.5", "14.52.36328");
    let compiler = find_primary_compiler(&m, VsVersion::Vs2026).unwrap();
    let family = family_prefix(&compiler.id).unwrap();
    let opts = parsed_with_extras(
        &[("build_version", "18.6.11819.183")],
        &["Definitely.Not.Real.*"],
    );
    let err = select_packages(&m, &family, &opts).unwrap_err();
    assert!(err.to_string().contains("matched no packages"), "got: {err}");
}

// ---- fingerprint -----------------------------------------------------------

#[test]
fn fingerprint_stable_across_reordered_extras() {
    let a = parsed_with_extras(
        &[("build_version", "18.6.11819.183")],
        &["ATL.*", "MFC.*"],
    );
    let b = parsed_with_extras(
        &[("build_version", "18.6.11819.183")],
        &["MFC.*", "ATL.*"],
    );
    assert_eq!(fingerprint(&a), fingerprint(&b));
}

#[test]
fn fingerprint_changes_with_base_install() {
    let default = parsed(&[("build_version", "18.6.11819.183")]);
    let full = parsed(&[("build_version", "18.6.11819.183"), ("base_install", "full")]);
    assert_ne!(fingerprint(&default), fingerprint(&full));
}

// ---- end-to-end install ----------------------------------------------------

#[test]
fn install_default_extracts_expected_files() {
    let tmp = TempDir::new().unwrap();
    let fixtures = tmp.path().join("vsix");
    std::fs::create_dir_all(&fixtures).unwrap();
    let snapshot_packages = installable_packages_json(&fixtures, "14.52.18.5", "14.52.36328");
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "abc1234",
                date: "2026-05-13T00:00:00Z",
                build_version: "18.6.11819.183",
                display_version: "18.6.1",
                product_line_version: "18",
                manifest_packages_json: snapshot_packages,
            }],
        )],
    );

    let ctx = test_ctx(&tmp);
    let mut ctx = ctx; // mut for install
    let provider = MsvcProvider::with_remote(fx.remote.clone());
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("18.6.11819.183".into()),
    );
    opts.insert("manifest_commit".into(), toml::Value::String(fx.commit("abc1234")));
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(installed.freshly_extracted);
    assert!(installed.fingerprint.starts_with("msvc-18.6.11819.183-"));

    let raw = ctx.staging_dir().unwrap();
    assert!(raw.join("VC/Tools/MSVC/14.52.36328/bin/Hostx64/x64/cl.exe").is_file());
    assert!(raw.join("VC/Tools/MSVC/14.52.36328/include/vcruntime.h").is_file());
    assert!(raw.join("VC/Tools/MSVC/14.52.36328/lib/x64/vcruntime.lib").is_file());
    assert!(raw.join("VC/Tools/MSVC/14.52.36328/lib/x64/store/store.lib").is_file());
    assert!(raw
        .join("VC/Redist/MSVC/14.52.36328/x64/Microsoft.VC145.CRT/vcruntime140.dll")
        .is_file());
    // Regression: locale resources (.Res.base payloads) must land in the
    // default install. They're how cl.exe's error messages get localized —
    // without `1033/clui.dll` cl.exe fails to start. The closure deletion
    // relies on DEFAULT_PATTERNS' `Tools.HostX64.TargetX64*` glob covering
    // Res.base; this assertion locks that in.
    assert!(
        raw.join("VC/Tools/MSVC/14.52.36328/bin/Hostx64/x64/1033/clui.dll")
            .is_file(),
        "default install missing locale resources (clui.dll)"
    );
}

#[test]
fn install_includes_crt_when_compiler_patched() {
    // Regression: Microsoft ships compiler patches that bump the primary
    // compiler's version without bumping CRT/ATL/MFC versions inside the
    // same snapshot. Selection must include the older-versioned family
    // packages, not filter them out by exact version match against the
    // primary compiler. Bug: the old `match_pattern_into` had
    // `pkg.version == compiler_version`, which rejected CRT here.
    let tmp = TempDir::new().unwrap();
    let fixtures = tmp.path().join("vsix");
    std::fs::create_dir_all(&fixtures).unwrap();

    // Compiler-side packages at the PATCHED version.
    let compiler_v = "14.50.35731";
    // CRT-side packages at the OLDER version (not bumped by the patch).
    let crt_v = "14.50.35728";

    let mk = |id: &str, ver: &str, filename: &str, entry: &str| {
        let archive = fixtures.join(filename);
        build_vsix(&archive, &[(entry, id.as_bytes())]);
        let url = file_url(&archive);
        pkg_with_payload(id, ver, &url, filename)
    };

    let packages = format!(
        "{a},{b},{c},{d},{e},{f}",
        a = mk(
            "Microsoft.VC.14.50.18.0.Tools.HostX64.TargetX64.base",
            compiler_v,
            "tools.vsix",
            "VC/Tools/MSVC/14.50.35731/bin/Hostx64/x64/cl.exe",
        ),
        b = mk(
            "Microsoft.VC.14.50.18.0.Tools.HostX64.TargetX64.Res.base",
            compiler_v,
            "tools-res.vsix",
            "VC/Tools/MSVC/14.50.35731/bin/Hostx64/x64/1033/clui.dll",
        ),
        c = mk(
            "Microsoft.VC.14.50.18.0.CRT.Headers.base",
            crt_v,
            "crt-headers.vsix",
            "VC/Tools/MSVC/14.50.35728/include/vcruntime.h",
        ),
        d = mk(
            "Microsoft.VC.14.50.18.0.CRT.x64.Desktop.base",
            crt_v,
            "crt-desktop.vsix",
            "VC/Tools/MSVC/14.50.35728/lib/x64/vcruntime.lib",
        ),
        e = mk(
            "Microsoft.VC.14.50.18.0.CRT.x64.Store.base",
            crt_v,
            "crt-store.vsix",
            "VC/Tools/MSVC/14.50.35728/lib/x64/store/store.lib",
        ),
        f = mk(
            "Microsoft.VC.14.50.18.0.CRT.Redist.X64.base",
            crt_v,
            "crt-redist.vsix",
            "VC/Redist/MSVC/14.50.35728/x64/Microsoft.VC145.CRT/vcruntime140.dll",
        ),
    );
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "abc1234",
                date: "2026-05-13T00:00:00Z",
                build_version: "18.6.11819.183",
                display_version: "18.6.1",
                product_line_version: "18",
                manifest_packages_json: packages,
            }],
        )],
    );

    let ctx = test_ctx(&tmp);
    let mut ctx = ctx;
    let provider = MsvcProvider::with_remote(fx.remote.clone());
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("18.6.11819.183".into()),
    );
    opts.insert("manifest_commit".into(), toml::Value::String(fx.commit("abc1234")));
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(installed.freshly_extracted);

    let raw = ctx.staging_dir().unwrap();
    // Compiler (patched version) is present.
    assert!(raw.join("VC/Tools/MSVC/14.50.35731/bin/Hostx64/x64/cl.exe").is_file());
    // CRT.Headers (OLDER version) is also present — the bug would drop it.
    assert!(
        raw.join("VC/Tools/MSVC/14.50.35728/include/vcruntime.h").is_file(),
        "CRT.Headers at older version was filtered out — version-equality \
         filter regression?"
    );
    // The other older-versioned CRT packages too.
    assert!(raw.join("VC/Tools/MSVC/14.50.35728/lib/x64/vcruntime.lib").is_file());
    assert!(raw
        .join("VC/Redist/MSVC/14.50.35728/x64/Microsoft.VC145.CRT/vcruntime140.dll")
        .is_file());
}

/// A one-package snapshot for the pinning tests, built at `build_version`.
fn tiny_snapshot(fixtures: &std::path::Path, label: &'static str, date: &'static str, build_version: &'static str, with_sha: bool) -> FxSnapshot {
    let archive = fixtures.join(format!("{label}.vsix"));
    build_vsix(&archive, &[(&format!("VC/{label}.txt"), label.as_bytes())]);
    let url = file_url(&archive);
    let mut pkgs = [
        "Tools.HostX64.TargetX64.base",
        "CRT.Headers.base",
        "CRT.x64.Desktop.base",
        "CRT.x64.Store.base",
        "CRT.Redist.X64.base",
    ]
    .iter()
    .map(|tail| pkg_with_payload(&format!("Microsoft.VC.14.52.18.5.{tail}"), "14.52.1", &url, &format!("{label}.vsix")))
    .collect::<Vec<_>>()
    .join(",");
    if !with_sha {
        pkgs = pkgs.replace(&format!(r#","sha256":"{}""#, crate::providers::vs_manifest::tests::file_url_sha256(&url)), "");
    }
    FxSnapshot {
        sha: label,
        date,
        build_version,
        display_version: build_version,
        product_line_version: "18",
        manifest_packages_json: pkgs,
    }
}

fn install_opts(build_version: &str, commit: &str) -> toml::Table {
    let mut opts = toml::Table::new();
    opts.insert("build_version".into(), toml::Value::String(build_version.into()));
    opts.insert("manifest_commit".into(), toml::Value::String(commit.into()));
    opts
}

#[test]
fn install_uses_pinned_commit_even_when_a_newer_one_has_the_same_build() {
    // Someone controlling the mirror publishes a second snapshot claiming the
    // same build_version, with a later author date. The old code picked it;
    // the pin ignores it.
    let tmp = TempDir::new().unwrap();
    let fixtures = tmp.path().join("vsix");
    std::fs::create_dir_all(&fixtures).unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[
                tiny_snapshot(&fixtures, "real", "2026-05-13T00:00:00Z", "18.6.11819.183", true),
                tiny_snapshot(&fixtures, "evil", "2099-01-01T00:00:00Z", "18.6.11819.183", true),
            ],
        )],
    );
    let mut ctx = test_ctx(&tmp);
    let provider = MsvcProvider::with_remote(fx.remote.clone());
    provider
        .install(&install_opts("18.6.11819.183", &fx.commit("real")), &mut ctx)
        .unwrap();
    let raw = ctx.staging_dir().unwrap();
    assert!(raw.join("VC/real.txt").is_file());
    assert!(!raw.join("VC/evil.txt").exists());
}

#[test]
fn install_rejects_commit_of_another_build() {
    let tmp = TempDir::new().unwrap();
    let fixtures = tmp.path().join("vsix");
    std::fs::create_dir_all(&fixtures).unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[tiny_snapshot(&fixtures, "s", "2026-05-13T00:00:00Z", "18.5.0.0", true)],
        )],
    );
    let mut ctx = test_ctx(&tmp);
    let provider = MsvcProvider::with_remote(fx.remote.clone());
    let err = provider
        .install(&install_opts("18.6.11819.183", &fx.commit("s")), &mut ctx)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("18.5.0.0") && msg.contains("not build_version"), "got: {msg}");
}

#[test]
fn install_rejects_payload_without_sha256() {
    let tmp = TempDir::new().unwrap();
    let fixtures = tmp.path().join("vsix");
    std::fs::create_dir_all(&fixtures).unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[tiny_snapshot(&fixtures, "s", "2026-05-13T00:00:00Z", "18.6.11819.183", false)],
        )],
    );
    let mut ctx = test_ctx(&tmp);
    let provider = MsvcProvider::with_remote(fx.remote.clone());
    let err = provider
        .install(&install_opts("18.6.11819.183", &fx.commit("s")), &mut ctx)
        .unwrap_err();
    assert!(format!("{err:#}").contains("no sha256"), "got: {err:#}");
}

#[test]
fn options_require_full_manifest_commit() {
    let mut opts = toml::Table::new();
    opts.insert("build_version".into(), toml::Value::String("18.6.11819.183".into()));
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("manifest_commit"), "got: {err}");
    opts.insert("manifest_commit".into(), toml::Value::String("HEAD".into()));
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("40-hex"), "got: {err}");
    // Uppercase is accepted and normalized, so the fingerprint is stable.
    opts.insert("manifest_commit".into(), toml::Value::String(TEST_COMMIT.to_uppercase()));
    assert_eq!(Options::parse(&opts).unwrap().manifest_commit, TEST_COMMIT);
}

#[test]
fn install_cache_hit_skips_fetch() {
    let tmp = TempDir::new().unwrap();
    let cache = crate::cache::Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();

    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("18.6.11819.183".into()),
    );
    opts.insert("manifest_commit".into(), toml::Value::String(TEST_COMMIT.into()));
    let parsed_opts = Options::parse(&opts).unwrap();
    let fp = fingerprint(&parsed_opts);
    let install_dir = cache.install_dir(&fp);
    std::fs::create_dir_all(install_dir.join("tree")).unwrap();
    let md = crate::cache::InstallMetadata::new(
        "msvc",
        fp.clone(),
        "cached",
        toml::Table::new(),
    );
    md.write(&cache.install_metadata_path(&fp)).unwrap();

    let mut ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_remote("file:///never/exists");
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(!installed.freshly_extracted);
    assert!(ctx.staging_root().is_none());
}

// ---- fixture helpers -------------------------------------------------------

fn parsed(pairs: &[(&str, &str)]) -> Options {
    let mut t = toml::Table::new();
    t.insert("manifest_commit".into(), toml::Value::String(TEST_COMMIT.into()));
    for (k, v) in pairs {
        t.insert((*k).into(), toml::Value::String((*v).into()));
    }
    Options::parse(&t).unwrap()
}

fn parsed_with_extras(pairs: &[(&str, &str)], extras: &[&str]) -> Options {
    let mut t = toml::Table::new();
    t.insert("manifest_commit".into(), toml::Value::String(TEST_COMMIT.into()));
    for (k, v) in pairs {
        t.insert((*k).into(), toml::Value::String((*v).into()));
    }
    t.insert(
        "extras".into(),
        toml::Value::Array(
            extras
                .iter()
                .map(|s| toml::Value::String((*s).into()))
                .collect(),
        ),
    );
    Options::parse(&t).unwrap()
}

/// Realistic family at one version + one out-of-family preview, at the
/// same version. No payloads (selection-only tests).
fn full_fixture_manifest(id_version: &str, package_version: &str) -> VsManifest {
    let json = format!(
        r#"{{"packages":[
            {a}, {b}, {c}, {d}, {e}, {f}, {g}, {h}
        ]}}"#,
        a = pkg(&format!("Microsoft.VC.{id_version}.Tools.HostX64.TargetX64.base"), package_version),
        b = pkg_with_lang(
            &format!("Microsoft.VC.{id_version}.Tools.HostX64.TargetX64.Res.base"),
            package_version,
            "en-US"
        ),
        c = pkg(&format!("Microsoft.VC.{id_version}.CRT.Headers.base"), package_version),
        d = pkg(&format!("Microsoft.VC.{id_version}.CRT.x64.Desktop.base"), package_version),
        e = pkg(&format!("Microsoft.VC.{id_version}.CRT.x64.Store.base"), package_version),
        f = pkg(&format!("Microsoft.VC.{id_version}.CRT.Redist.X64.base"), package_version),
        g = pkg(&format!("Microsoft.VC.{id_version}.ATL.X64.base"), package_version),
        h = pkg("Microsoft.VC.Preview.DIA.SDK", package_version),
    );
    serde_json::from_str(&json).unwrap()
}

/// Manifest packages array (just the inner list, no enclosing `{}`) with
/// real VSIX payloads on disk. For the end-to-end install test.
fn installable_packages_json(
    fixtures_dir: &std::path::Path,
    id_version: &str,
    package_version: &str,
) -> String {
    let specs = [
        (
            format!("Microsoft.VC.{id_version}.Tools.HostX64.TargetX64.base"),
            "tools.vsix",
            format!("VC/Tools/MSVC/{package_version}/bin/Hostx64/x64/cl.exe"),
        ),
        (
            // Locale-resource package — must be matched by DEFAULT_PATTERNS'
            // `Tools.HostX64.TargetX64*` glob.
            format!("Microsoft.VC.{id_version}.Tools.HostX64.TargetX64.Res.base"),
            "tools-res.vsix",
            format!("VC/Tools/MSVC/{package_version}/bin/Hostx64/x64/1033/clui.dll"),
        ),
        (
            format!("Microsoft.VC.{id_version}.CRT.Headers.base"),
            "crt-headers.vsix",
            format!("VC/Tools/MSVC/{package_version}/include/vcruntime.h"),
        ),
        (
            format!("Microsoft.VC.{id_version}.CRT.x64.Desktop.base"),
            "crt-desktop.vsix",
            format!("VC/Tools/MSVC/{package_version}/lib/x64/vcruntime.lib"),
        ),
        (
            format!("Microsoft.VC.{id_version}.CRT.x64.Store.base"),
            "crt-store.vsix",
            format!("VC/Tools/MSVC/{package_version}/lib/x64/store/store.lib"),
        ),
        (
            format!("Microsoft.VC.{id_version}.CRT.Redist.X64.base"),
            "crt-redist.vsix",
            format!("VC/Redist/MSVC/{package_version}/x64/Microsoft.VC145.CRT/vcruntime140.dll"),
        ),
    ];
    specs
        .iter()
        .map(|(id, filename, entry)| {
            let archive = fixtures_dir.join(filename);
            build_vsix(&archive, &[(entry.as_str(), id.as_bytes())]);
            let url = file_url(&archive);
            pkg_with_payload(id, package_version, &url, filename)
        })
        .collect::<Vec<_>>()
        .join(",")
}
