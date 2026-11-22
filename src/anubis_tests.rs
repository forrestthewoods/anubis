//! Tests for anubis.rs

use camino::Utf8PathBuf;

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
    let root = Utf8PathBuf::from("c:/stuff/proj_root");

    assert_eq!(
        AnubisTarget::new("//hello:world").unwrap().get_config_abspath(&root).as_str(),
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
    let resolved = relative.resolve("samples/basic/myproject");

    assert!(!resolved.is_relative());
    assert_eq!(resolved.target_path(), "//samples/basic/myproject:foo");
    assert_eq!(resolved.target_name(), "foo");
    assert_eq!(resolved.get_relative_dir(), "samples/basic/myproject");

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
    let target = AnubisTarget::new("//samples/basic/simple_cpp:simple_cpp").unwrap();
    let config_relpath = target.get_config_relpath();

    assert_eq!(config_relpath.get_dir_relpath(), "samples/basic/simple_cpp");

    let target2 = AnubisTarget::new("//libs/common/utils:helpers").unwrap();
    let config_relpath2 = target2.get_config_relpath();

    assert_eq!(config_relpath2.get_dir_relpath(), "libs/common/utils");

    // Edge case: target at root level
    let target3 = AnubisTarget::new("//mode:win_dev").unwrap();
    let config_relpath3 = target3.get_config_relpath();

    assert_eq!(config_relpath3.get_dir_relpath(), "mode");

    // A target in the project root's own ANUBIS file.
    let root = AnubisTarget::new("//:py").unwrap();
    assert_eq!(root.get_config_relpath().get_dir_relpath(), "");
}

#[test]
fn target_pattern_parse() {
    // Parse valid patterns
    let pattern = TargetPattern::parse("//examples/...").unwrap();
    assert_eq!(pattern.dir_relpath, "examples");

    let pattern2 = TargetPattern::parse("//foo/bar/baz/...").unwrap();
    assert_eq!(pattern2.dir_relpath, "foo/bar/baz");

    // The whole repo: `//...`, or `///...`.
    assert_eq!(TargetPattern::parse("//...").unwrap().dir_relpath, "");
    assert_eq!(TargetPattern::parse("///...").unwrap().dir_relpath, "");

    // Invalid patterns return None
    assert!(TargetPattern::parse("//examples:target").is_none());
    assert!(TargetPattern::parse(":foo").is_none());
    assert!(TargetPattern::parse("examples/...").is_none());
    assert!(TargetPattern::parse("//examples/..").is_none()); // Only two dots
    assert!(TargetPattern::parse("//examples/....").is_none()); // Four dots
    assert!(TargetPattern::parse("//....").is_none());
}

#[test]
fn target_names_are_usable_as_directories() {
    for bad in ["//pkg:..", "//pkg:.", "//pkg:", ":..", "//../outside:x", "//pkg/../other:x", "//./pkg:x"] {
        assert_err!(AnubisTarget::new(bad));
    }
    assert_ok!(AnubisTarget::new("//pkg/sub:a.b"));
    assert_ok!(AnubisTarget::new("//pkg:..a"));
}

#[test]
fn target_names_in_one_file_are_distinct_ignoring_case() {
    let names = |text: &str| {
        let config = crate::papyrus::read_papyrus_str(text, "ANUBIS").unwrap();
        check_target_names(&config).map_err(|e| e.to_string())
    };
    assert_ok!(names(r#"a(name = "x") b(name = "y")"#));
    assert!(names(r#"a(name = "App") b(name = "app")"#).unwrap_err().contains("differ only by case"));
    assert!(names(r#"a(name = "x") b(name = "x")"#).unwrap_err().contains("two targets are named"));
    assert!(names(r#"a(name = "..")"#).unwrap_err().contains("not a valid target name"));
}

#[test]
fn every_target_gets_its_own_output_directory() {
    let name = |t: &str| AnubisTarget::new(t).unwrap().out_dir_name();

    // <dirname>_<target name>_<8 hex of the label's xxh3>. The values are
    // pinned: changing the hash moves every output and misses every cache entry.
    assert_eq!(name("//libs/foo:bar"), "foo_bar_eaa562ef");
    assert_eq!(name("//mode:win_dev"), "mode_win_dev_fc0d131e");

    // `//a:b` used to own `a/b/`, the parent of every `//a/b:*` target, and
    // `//a/b:lib` shared `a/b/lib/` with `//a:b`'s archive.
    let all = ["//a:b", "//a/b:c", "//a/b:lib", "//x/a:b", "//a_b:c", "//a:b_c"].map(name);
    for (i, a) in all.iter().enumerate() {
        assert!(!a.contains('/'), "{a} must be one path component");
        for b in &all[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn intermediate_names_are_distinct_per_source_path() {
    let root = camino::Utf8Path::new("/ws");
    let name = |src: &str| crate::util::intermediate_name(root, camino::Utf8Path::new(src), "obj");

    assert_eq!(name("/ws/libs/foo/src/util/str.cpp"), "str_debb8726.obj");
    // Same name wherever the checkout is, and with either slash.
    assert_eq!(crate::util::intermediate_name(camino::Utf8Path::new("C:/other"), camino::Utf8Path::new("C:/other/libs/foo/src/util/str.cpp"), "obj"), name("/ws/libs/foo/src/util/str.cpp"));
    assert_eq!(crate::util::intermediate_name(camino::Utf8Path::new("C:\\other"), camino::Utf8Path::new("C:\\other\\libs\\foo\\src\\util\\str.cpp"), "obj"), name("/ws/libs/foo/src/util/str.cpp"));
    // Same stem, different directory or extension.
    assert_ne!(name("/ws/a/x.c"), name("/ws/b/x.c"));
    assert_ne!(name("/ws/a/x.c"), name("/ws/a/x.cpp"));
}

#[test]
fn find_anubis_files_skips_hidden_and_root_target_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for dir in ["", "a", "a/b", "x/target", "target", ".hidden", "a/node_modules"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join("ANUBIS"), "").unwrap();
    }
    // A directory named ANUBIS is not an ANUBIS file.
    std::fs::create_dir_all(root.join("c/ANUBIS")).unwrap();

    let rel = |files: Vec<std::path::PathBuf>| -> Vec<String> {
        files
            .iter()
            .map(|f| f.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"))
            .collect()
    };
    let files = rel(find_anubis_files(root, root).unwrap());
    assert_eq!(files, ["ANUBIS", "a/ANUBIS", "a/b/ANUBIS", "a/node_modules/ANUBIS", "x/target/ANUBIS"]);

    // `target` is skipped only at the workspace root.
    let files = rel(find_anubis_files(&root.join("x"), root).unwrap());
    assert_eq!(files, ["x/target/ANUBIS"]);
}

#[cfg(unix)]
#[test]
fn find_anubis_files_does_not_follow_symlinked_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a/ANUBIS"), "").unwrap();
    std::fs::create_dir_all(root.join("outside/pkg")).unwrap();
    std::fs::write(root.join("outside/pkg/ANUBIS"), "").unwrap();
    std::fs::create_dir_all(root.join("ws")).unwrap();
    let ws = root.join("ws");
    std::fs::write(ws.join("ANUBIS"), "").unwrap();
    std::os::unix::fs::symlink("..", ws.join("loop")).unwrap();
    std::os::unix::fs::symlink(root.join("outside"), ws.join("linked")).unwrap();
    // A symlinked ANUBIS file is still an ANUBIS file; a dangling one is not.
    std::fs::create_dir_all(ws.join("f")).unwrap();
    std::os::unix::fs::symlink(root.join("a/ANUBIS"), ws.join("f/ANUBIS")).unwrap();
    std::fs::create_dir_all(ws.join("g")).unwrap();
    std::os::unix::fs::symlink(root.join("missing"), ws.join("g/ANUBIS")).unwrap();

    let files = find_anubis_files(&ws, &ws).unwrap();
    assert_eq!(files, [ws.join("ANUBIS"), ws.join("f/ANUBIS")]);

    // A symlinked start directory is followed: it was asked for by name.
    let files = find_anubis_files(&ws.join("linked"), &ws).unwrap();
    assert_eq!(files, [ws.join("linked/pkg/ANUBIS")]);
}
