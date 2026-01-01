//! Tests for anubis.rs

use std::path::PathBuf;
use std::str::FromStr;

use crate::anubis::*;
use crate::{assert_err, assert_ok};

#[test]
fn anubis_target_invalid() {
    assert_err!(AnubisTarget::new("foo"));
    assert_err!(AnubisTarget::new("foo:bar"));
    assert_err!(AnubisTarget::new("//foo:bar:baz"));
    assert_err!(AnubisTarget::new("//foo"));
    assert_err!(AnubisTarget::new("//ham/eggs"));
    assert_err!(AnubisTarget::new("//ham:eggs/bacon"));
}

#[test]
fn anubis_target_valid() {
    assert_ok!(AnubisTarget::new("//foo:bar"));
    assert_ok!(AnubisTarget::new("//foo/bar:baz"));

    let t = AnubisTarget::new("//foo/bar:baz").unwrap();
    assert_eq!(t.get_relative_dir(), "foo/bar");
    assert_eq!(t.target_name(), "baz");

    assert_ok!(AnubisTarget::new(":eggs"));
}

#[test]
fn anubis_abspath() {
    let root = PathBuf::from_str("c:/stuff/proj_root").unwrap();

    assert_eq!(
        AnubisTarget::new("//hello:world")
            .unwrap()
            .get_config_abspath(&root)
            .to_string_lossy(),
        "c:/stuff/proj_root/hello/ANUBIS"
    );
}
