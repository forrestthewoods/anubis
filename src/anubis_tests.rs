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

#[test]
fn anubis_target_is_relative() {
    // Relative targets have separator_idx == 0
    let relative = AnubisTarget::new(":foo").unwrap();
    assert!(relative.is_relative());
    assert_eq!(relative.target_name(), "foo");

    let relative2 = AnubisTarget::new(":my_target").unwrap();
    assert!(relative2.is_relative());
    assert_eq!(relative2.target_name(), "my_target");

    // Absolute targets have separator_idx > 0
    let absolute = AnubisTarget::new("//path/to:bar").unwrap();
    assert!(!absolute.is_relative());
    assert_eq!(absolute.target_name(), "bar");
    assert_eq!(absolute.get_relative_dir(), "path/to");
}

#[test]
fn anubis_target_resolve() {
    // Resolving a relative target should produce an absolute target
    let relative = AnubisTarget::new(":foo").unwrap();
    let resolved = relative.resolve("examples/myproject");

    assert!(!resolved.is_relative());
    assert_eq!(resolved.target_path(), "//examples/myproject:foo");
    assert_eq!(resolved.target_name(), "foo");
    assert_eq!(resolved.get_relative_dir(), "examples/myproject");

    // Resolving an already absolute target should return a clone
    let absolute = AnubisTarget::new("//path/to:bar").unwrap();
    let resolved_absolute = absolute.resolve("some/other/path");

    assert!(!resolved_absolute.is_relative());
    assert_eq!(resolved_absolute.target_path(), "//path/to:bar");
    assert_eq!(resolved_absolute.target_name(), "bar");
    assert_eq!(resolved_absolute.get_relative_dir(), "path/to");
}

#[test]
fn anubis_config_relpath_get_dir_relpath() {
    // Test the get_dir_relpath method on AnubisConfigRelPath
    let target = AnubisTarget::new("//examples/simple_cpp:simple_cpp").unwrap();
    let config_relpath = target.get_config_relpath();

    assert_eq!(config_relpath.get_dir_relpath(), "examples/simple_cpp");

    let target2 = AnubisTarget::new("//libs/common/utils:helpers").unwrap();
    let config_relpath2 = target2.get_config_relpath();

    assert_eq!(config_relpath2.get_dir_relpath(), "libs/common/utils");

    // Edge case: target at root level
    let target3 = AnubisTarget::new("//mode:win_dev").unwrap();
    let config_relpath3 = target3.get_config_relpath();

    assert_eq!(config_relpath3.get_dir_relpath(), "mode");
}
