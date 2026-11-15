//! Tests for `src/providers/flatten.rs`.

use super::*;
use tempfile::TempDir;

fn options(s: &str) -> toml::Table {
    s.parse::<toml::Table>().unwrap()
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn parse(s: &str) -> Result<Option<Flatten>> {
    Flatten::parse(&options(s), "git")
}

#[test]
fn absent_option_is_none_and_adds_nothing_to_the_key() {
    let f = parse("repo = 'x'").unwrap();
    assert_eq!(f, None);
    assert_eq!(Flatten::key(&f), "");
}

#[test]
fn key_changes_with_the_option() {
    let a = parse("flatten = { into = 'inc', files = ['*.h'] }").unwrap();
    let b = parse("flatten = { into = 'inc', files = ['*.h', '*.hxx'] }").unwrap();
    let c = parse("flatten = { into = 'include', files = ['*.h'] }").unwrap();
    assert_eq!(Flatten::key(&a), r#"|flatten:inc:["*.h"]"#);
    // One pattern with a comma is not two patterns.
    let d = parse("flatten = { into = 'inc', files = ['a,b'] }").unwrap();
    let e = parse("flatten = { into = 'inc', files = ['a', 'b'] }").unwrap();
    assert_ne!(Flatten::key(&d), Flatten::key(&e));
    assert_ne!(Flatten::key(&a), Flatten::key(&b));
    assert_ne!(Flatten::key(&a), Flatten::key(&c));
}

#[test]
fn rejects_bad_options() {
    for bad in [
        "flatten = 'inc'",
        "flatten = { files = ['*.h'] }",
        "flatten = { into = 'inc' }",
        "flatten = { into = 'inc', files = [] }",
        "flatten = { into = '../inc', files = ['*.h'] }",
        "flatten = { into = 'C:\\\\inc', files = ['*.h'] }",
        "flatten = { into = '', files = ['*.h'] }",
        "flatten = { into = 'inc', files = ['a/*.h'] }",
        "flatten = { into = 'inc', files = ['[.h'] }",
        "flatten = { into = 'inc', files = ['*.h'], extra = 1 }",
    ] {
        assert!(parse(bad).is_err(), "should be rejected: {bad}");
    }
}

#[test]
fn copies_matching_files_and_leaves_the_originals() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "gp/gp_Pnt.hxx", "pnt");
    write(root, "gp/gp_Pnt.cxx", "src");
    write(root, "TopoDS/TopoDS_Shape.hxx", "shape");
    write(root, "TopoDS/deep/er/TopoDS.lxx", "inline");
    write(root, "README.md", "readme");

    let f = parse("flatten = { into = 'inc', files = ['*.hxx', '*.lxx'] }").unwrap().unwrap();
    assert_eq!(f.apply(root).unwrap(), 3);

    let mut names: Vec<String> = std::fs::read_dir(root.join("inc"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["TopoDS.lxx", "TopoDS_Shape.hxx", "gp_Pnt.hxx"]);
    assert_eq!(std::fs::read_to_string(root.join("inc/gp_Pnt.hxx")).unwrap(), "pnt");
    assert_eq!(std::fs::read_to_string(root.join("gp/gp_Pnt.hxx")).unwrap(), "pnt");
}

#[test]
fn nested_into_directory() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "a/x.h", "x");
    let f = parse("flatten = { into = 'out/inc/', files = ['*.h'] }").unwrap().unwrap();
    assert_eq!(f.apply(tmp.path()).unwrap(), 1);
    assert!(tmp.path().join("out/inc/x.h").is_file());
}

#[test]
fn same_name_in_two_directories_is_an_error_naming_both() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "a/config.h", "a");
    write(tmp.path(), "b/Config.h", "b");
    let f = parse("flatten = { into = 'inc', files = ['*.h'] }").unwrap().unwrap();
    let err = f.apply(tmp.path()).unwrap_err().to_string();
    assert!(err.contains("a/config.h") && err.contains("b/Config.h"), "{err}");
    assert!(!tmp.path().join("inc").exists());
}

#[test]
fn existing_into_or_no_match_is_an_error() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "inc/x.h", "x");
    let f = parse("flatten = { into = 'inc', files = ['*.h'] }").unwrap().unwrap();
    assert!(f.apply(tmp.path()).unwrap_err().to_string().contains("already exists"));

    let f = parse("flatten = { into = 'flat', files = ['*.nothing'] }").unwrap().unwrap();
    assert!(f.apply(tmp.path()).unwrap_err().to_string().contains("no file matches"));
}

#[test]
fn into_under_a_file_or_symlink_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    write(tmp.path(), "src/a.h", "a");
    write(tmp.path(), "file", "x");
    let f = parse("flatten = { into = 'file/inc', files = ['*.h'] }").unwrap().unwrap();
    assert!(f.apply(tmp.path()).unwrap_err().to_string().contains("not a directory"));

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
        let f = parse("flatten = { into = 'link/inc', files = ['*.h'] }").unwrap().unwrap();
        assert!(f.apply(tmp.path()).unwrap_err().to_string().contains("not a directory"));
        assert!(!outside.path().join("inc").exists());
    }
}
