//! Tests for `src/providers/apple_sdk.rs`: a synthetic CLT package
//! (xar → pbzx → odc cpio) built in memory.

use super::*;
use crate::cache::Cache;
use std::io::Write;
use tempfile::TempDir;

enum Entry<'a> {
    Dir(&'a str),
    File(&'a str, &'a [u8]),
    Link(&'a str, &'a str),
}

fn cpio(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut push = |name: &str, mode: u32, data: &[u8]| {
        let name_z = format!("{name}\0");
        out.extend(
            format!(
                "070707{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:011o}{:06o}{:011o}",
                0, 0, mode, 0, 0, 1, 0, 0, name_z.len(), data.len()
            )
            .as_bytes(),
        );
        out.extend(name_z.as_bytes());
        out.extend(data);
    };
    for e in entries {
        match e {
            Entry::Dir(n) => push(n, 0o040755, b""),
            Entry::File(n, d) => push(n, 0o100644, d),
            Entry::Link(n, t) => push(n, 0o120755, t.as_bytes()),
        }
    }
    push("TRAILER!!!", 0, b"");
    out
}

/// pbzx with two chunks: one xz-compressed, one stored.
fn pbzx(data: &[u8]) -> Vec<u8> {
    let mut out = b"pbzx".to_vec();
    out.extend(16u64.to_be_bytes());
    let (a, b) = data.split_at(data.len() / 2);
    let mut xz = Vec::new();
    {
        let mut enc = liblzma::write::XzEncoder::new(&mut xz, 1);
        enc.write_all(a).unwrap();
        enc.finish().unwrap();
    }
    out.extend((a.len() as u64).to_be_bytes());
    out.extend((xz.len() as u64).to_be_bytes());
    out.extend(&xz);
    out.extend((b.len() as u64).to_be_bytes());
    out.extend((b.len() as u64).to_be_bytes());
    out.extend(b);
    out
}

/// A xar with `Payload` (+ a decoy member before it), TOC flavoured like
/// Apple's (`toc_comment` varies the signed part, as Apple's re-publishes do).
fn xar(payload: &[u8], toc_comment: &str) -> Vec<u8> {
    let decoy = b"<?xml?><pkg-info/>";
    let toc = format!(
        "<?xml version=\"1.0\"?><xar><toc><creation-time>{toc_comment}</creation-time>\
         <file id=\"1\"><data><length>{dl}</length><offset>0</offset><size>{dl}</size>\
         <encoding style=\"application/octet-stream\"/></data><name>PackageInfo</name><type>file</type></file>\
         <file id=\"2\"><data><length>{pl}</length><offset>{dl}</offset><size>{pl}</size>\
         <encoding style=\"application/octet-stream\"/></data><name>Payload</name><type>file</type></file>\
         </toc></xar>",
        dl = decoy.len(),
        pl = payload.len()
    );
    let mut z = Vec::new();
    {
        let mut enc = flate2::write::ZlibEncoder::new(&mut z, flate2::Compression::fast());
        enc.write_all(toc.as_bytes()).unwrap();
        enc.finish().unwrap();
    }
    let mut out = b"xar!".to_vec();
    out.extend(28u16.to_be_bytes());
    out.extend(1u16.to_be_bytes());
    out.extend((z.len() as u64).to_be_bytes());
    out.extend((toc.len() as u64).to_be_bytes());
    out.extend(1u32.to_be_bytes());
    out.extend(&z);
    out.extend(decoy);
    out.extend(payload);
    out
}

const SDK: &str = "MacOSX26.5.sdk";

fn sample_payload() -> Vec<u8> {
    let root = format!("./Library/Developer/CommandLineTools/SDKs/{SDK}");
    let n = |s: &str| format!("{root}/{s}");
    let a = n("SDKSettings.json");
    let b = n("usr/include/stdio.h");
    let c = n("System/Library/Frameworks/Foo.framework/Versions/A/Headers/Foo.h");
    let d = n("System/Library/Frameworks/Foo.framework/Versions/Current");
    let e = n("System/Library/Frameworks/Foo.framework/Headers");
    let f = n("System/iOSSupport/usr/include/ios.h");
    let g = n("usr/lib/swift/Foo.swiftmodule/arm64.swiftinterface");
    let h = n("usr/include/to_ios.h");
    let dirs = [
        root.clone(),
        n("usr"),
        n("usr/include"),
        n("System/Library/Frameworks/Foo.framework/Versions/A/Headers"),
        n("System/iOSSupport/usr/include"),
        n("usr/lib/swift/Foo.swiftmodule"),
    ];
    let mut entries: Vec<Entry> = vec![
        Entry::Dir("./Library/Developer/CommandLineTools/SDKs"),
        Entry::Link("./Library/Developer/CommandLineTools/SDKs/MacOSX26.sdk", SDK),
        Entry::File("./Library/Developer/CommandLineTools/usr/bin/clang", b"not ours"),
    ];
    entries.extend(dirs.iter().map(|d| Entry::Dir(d)));
    entries.extend([
        Entry::File(&a, b"{\"Version\":\"26.5\"}"),
        Entry::File(&b, b"// stdio"),
        Entry::File(&c, b"// Foo"),
        // A link to a link, listed before its target exists.
        Entry::Link(&e, "Versions/Current/Headers"),
        Entry::Link(&d, "A"),
        Entry::File(&f, b"// ios"),
        Entry::File(&g, b"swift"),
        // Points into a pruned directory: dropped as dangling.
        Entry::Link(&h, "../../System/iOSSupport/usr/include/ios.h"),
    ]);
    pbzx(&cpio(&entries))
}

#[test]
fn extracts_sdk_root_pruned_with_links() {
    let tmp = TempDir::new().unwrap();
    let pkg = tmp.path().join("CLTools_macOSNMOS_SDK.pkg");
    std::fs::write(&pkg, xar(&sample_payload(), "2026-05-11")).unwrap();

    assert_eq!(sdk_names(&pkg).unwrap(), vec![SDK.to_string()]);

    let dest = tmp.path().join("out");
    let excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
    let r = extract_sdk(&pkg, SDK, &dest, &excludes).unwrap();
    assert_eq!(std::fs::read(dest.join("usr/include/stdio.h")).unwrap(), b"// stdio");
    assert_eq!(
        std::fs::read(dest.join("System/Library/Frameworks/Foo.framework/Headers/Foo.h")).unwrap(),
        b"// Foo",
        "framework Headers resolve through the Versions/Current chain"
    );
    assert!(!dest.join("System/iOSSupport").exists());
    assert!(!dest.join("usr/lib/swift/Foo.swiftmodule").exists());
    assert!(!dest.join("usr/include/to_ios.h").exists() && std::fs::symlink_metadata(dest.join("usr/include/to_ios.h")).is_err());
    assert!(!tmp.path().join("out/../clang").exists());
    assert_eq!(r.symlinks, 2);
    assert_eq!(r.dangling, 1);
    assert_eq!(r.files, 3);
}

#[test]
fn payload_hash_ignores_the_signed_toc() {
    let tmp = TempDir::new().unwrap();
    let payload = sample_payload();
    let a = tmp.path().join("a.pkg");
    let b = tmp.path().join("b.pkg");
    std::fs::write(&a, xar(&payload, "2026-05-11")).unwrap();
    std::fs::write(&b, xar(&payload, "2026-06-26T05:06:55")).unwrap();
    assert_ne!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    assert_eq!(payload_sha256(&a).unwrap(), payload_sha256(&b).unwrap());
}

#[test]
fn provider_skips_wrong_payloads_and_uses_the_matching_url() {
    let tmp = TempDir::new().unwrap();
    let good = tmp.path().join("good/CLTools_macOSNMOS_SDK.pkg");
    let other = tmp.path().join("other/CLTools_macOSNMOS_SDK.pkg");
    std::fs::create_dir_all(good.parent().unwrap()).unwrap();
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&good, xar(&sample_payload(), "x")).unwrap();
    std::fs::write(&other, xar(&pbzx(&cpio(&[])), "y")).unwrap();
    let sha = payload_sha256(&good).unwrap();

    let url = |p: &std::path::Path| url::Url::from_file_path(p).unwrap().to_string();
    let mut opts = toml::Table::new();
    opts.insert("sdk".into(), SDK.into());
    opts.insert("package".into(), "CLTools_macOSNMOS_SDK.pkg".into());
    opts.insert("payload_sha256".into(), sha.clone().into());
    opts.insert("catalog".into(), false.into());
    opts.insert(
        "urls".into(),
        toml::Value::Array(vec![
            "file:///nonexistent/CLTools_macOSNMOS_SDK.pkg".into(),
            url(&other).into(),
            url(&good).into(),
        ]),
    );
    let cache = Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let inst = AppleSdkProvider::new().install(&opts, &mut ctx).unwrap();
    assert!(inst.freshly_extracted);
    assert!(inst.fingerprint.starts_with("apple-sdk-MacOSX26.5-"));
    assert_eq!(inst.options.get("resolved_url").and_then(|v| v.as_str()), Some(url(&good).as_str()));
    assert!(ctx.staging_dir().unwrap().join("SDKSettings.json").is_file());

    // Wrong pin: every candidate is reported, nothing is installed.
    opts.insert("payload_sha256".into(), "0".repeat(64).into());
    let err = AppleSdkProvider::new().install(&opts, &mut InstallCtx::new(Cache::at(tmp.path().join("c2")))).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("natron apple-sdk list") && msg.contains(&sha), "{msg}");
}

#[test]
fn rejects_escaping_symlinks() {
    assert!(link_stays_inside(Path::new("a/b/link"), "../c"));
    assert!(link_stays_inside(Path::new("a/link"), "../c"));
    assert!(!link_stays_inside(Path::new("a/link"), "../../c"));
    assert!(!link_stays_inside(Path::new("link"), "/etc/passwd"));
}

#[test]
fn parses_catalog_products_newest_first() {
    let cat = r#"<plist><dict><key>CatalogVersion</key><integer>2</integer>
<key>Products</key><dict>
 <key>001-11111</key><dict>
   <key>Packages</key><array>
     <dict><key>Size</key><integer>5000</integer><key>URL</key><string>https://swcdn.apple.com/x/001-11111-A/CLTools_macOSNMOS_SDK.pkg</string></dict>
   </array>
   <key>PostDate</key><date>2025-05-28T17:00:00Z</date>
   <key>ExtendedMetaInfo</key><dict><key>Nested</key><dict/></dict>
 </dict>
 <key>002-22222</key><dict>
   <key>Packages</key><array>
     <dict><key>URL</key><string>https://swcdn.apple.com/x/002-22222-A/Other.pkg</string></dict>
   </array>
   <key>PostDate</key><date>2026-01-01T00:00:00Z</date>
 </dict>
 <key>003-33333</key><dict>
   <key>PostDate</key><date>2026-06-26T00:00:00Z</date>
   <key>Packages</key><array>
     <dict><key>URL</key><string>https://swcdn.apple.com/x/003-33333-A/CLTools_macOSNMOS_SDK.pkg</string><key>Size</key><integer>61622368</integer></dict>
     <dict><key>URL</key><string>https://swcdn.apple.com/x/003-33333-A/CLTools_macOS_SDK.pkg</string></dict>
   </array>
 </dict>
</dict></dict></plist>"#;
    let pkgs = parse_catalog(cat);
    let got: Vec<_> = pkgs.iter().map(|p| (p.product.as_str(), p.url.rsplit('/').next().unwrap(), p.size)).collect();
    assert_eq!(
        got,
        vec![
            ("003-33333", "CLTools_macOSNMOS_SDK.pkg", Some(61622368)),
            ("003-33333", "CLTools_macOS_SDK.pkg", None),
            ("001-11111", "CLTools_macOSNMOS_SDK.pkg", Some(5000)),
        ]
    );
}
