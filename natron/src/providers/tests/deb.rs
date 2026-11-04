//! Tests for `src/providers/deb.rs`.

use super::*;
use crate::cache::Cache;
use sha2::{Digest, Sha256};
use std::path::Path;
use tempfile::TempDir;

/// Build a minimal .deb: an ar archive whose data.tar.gz holds `files`
/// under `./`, the way dpkg-deb lays them out.
fn build_deb(path: &Path, files: &[(&str, &[u8])]) {
    let mut tar_bytes = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::fast());
        let mut tb = tar::Builder::new(gz);
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tb.append_data(&mut h, format!("./{name}"), *data).unwrap();
        }
        tb.into_inner().unwrap().finish().unwrap();
    }
    let mut out = b"!<arch>\n".to_vec();
    for (name, data) in [("debian-binary", &b"2.0\n"[..]), ("control.tar.gz", &b"x"[..]), ("data.tar.gz", &tar_bytes[..])] {
        out.extend(format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", name, 0, 0, 0, 100644, data.len()).as_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    std::fs::write(path, out).unwrap();
}

fn sha(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

#[test]
fn deb_provider_merges_usr_include_from_packages() {
    let tmp = TempDir::new().unwrap();
    let pool = tmp.path().join("mirror/pool");
    std::fs::create_dir_all(&pool).unwrap();
    build_deb(
        &pool.join("libfoo-dev_1_amd64.deb"),
        &[("usr/include/foo/foo.h", b"// foo"), ("usr/lib/libfoo.a", b"lib")],
    );
    build_deb(&pool.join("libbar-dev_1_amd64.deb"), &[("usr/include/bar.h", b"// bar")]);

    let mirror = url::Url::from_directory_path(tmp.path().join("mirror")).unwrap().to_string();
    let mut pkgs = toml::value::Array::new();
    for f in ["libfoo-dev_1_amd64.deb", "libbar-dev_1_amd64.deb"] {
        let mut t = toml::Table::new();
        t.insert("path".into(), format!("pool/{f}").into());
        t.insert("sha256".into(), sha(&pool.join(f)).into());
        pkgs.push(toml::Value::Table(t));
    }
    let mut opts = toml::Table::new();
    opts.insert("mirror".into(), mirror.into());
    opts.insert("packages".into(), toml::Value::Array(pkgs));

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let installed = DebProvider::new().install(&opts, &mut ctx).unwrap();
    assert!(installed.freshly_extracted);
    assert!(installed.fingerprint.starts_with("deb-libfoo-dev-"));

    let raw = ctx.staging_dir().unwrap();
    assert_eq!(std::fs::read(raw.join("foo/foo.h")).unwrap(), b"// foo");
    assert_eq!(std::fs::read(raw.join("bar.h")).unwrap(), b"// bar");
    assert!(!raw.join("libfoo.a").exists() && !raw.join("usr").exists());
}

#[test]
fn deb_provider_requires_sha256() {
    let mut t = toml::Table::new();
    t.insert("path".into(), "pool/x.deb".into());
    let mut opts = toml::Table::new();
    opts.insert("packages".into(), toml::Value::Array(vec![toml::Value::Table(t)]));
    assert!(parse_packages(&opts).unwrap_err().to_string().contains("sha256"));
}
