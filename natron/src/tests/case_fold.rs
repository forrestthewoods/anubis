//! Tests for `src/case_fold.rs`.

use super::*;
use tempfile::TempDir;

fn write(p: &Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, bytes).unwrap();
}

#[test]
fn include_basenames_parses_both_forms() {
    let mut out = BTreeSet::new();
    include_basenames(
        "#include <DriverSpecs.h>\n  #  include \"sub/WinDef.h\"\n#define X\n#include MACRO\n",
        &mut out,
    );
    let got: Vec<_> = out.into_iter().collect();
    assert_eq!(got, vec!["DriverSpecs.h".to_string(), "WinDef.h".to_string()]);
}

#[cfg(unix)]
#[test]
fn adds_lowercase_and_include_aliases() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(&root.join("Lib/x64/kernel32.Lib"), b"lib");
    write(&root.join("Lib/x64/already.lib"), b"lib");
    write(&root.join("Include/shared/driverspecs.h"), b"// real");
    write(&root.join("Include/shared/kernelspecs.h"), b"#include <DriverSpecs.h>\n#include <GL/gl.h>\n");
    write(&root.join("Include/um/gl/gl.h"), b"// gl");

    let report = add_case_aliases(root).unwrap();
    assert_eq!(report.lowercase_aliases, 1);
    assert_eq!(report.include_aliases, 1);
    // Kernelspecs.h, Driverspecs.h and Gl.h (DriverSpecs.h from pass 2 is a
    // different name on a case-sensitive filesystem).
    assert_eq!(report.capitalized_aliases, 3);
    assert_eq!(report.dir_aliases, 1);
    assert_eq!(std::fs::read(root.join("Include/um/GL/gl.h")).unwrap(), b"// gl");
    assert_eq!(
        std::fs::read_link(root.join("Include/shared/Kernelspecs.h")).unwrap(),
        Path::new("kernelspecs.h")
    );

    let lib_alias = root.join("Lib/x64/kernel32.lib");
    assert_eq!(std::fs::read_link(&lib_alias).unwrap(), Path::new("kernel32.Lib"));
    assert_eq!(std::fs::read(&lib_alias).unwrap(), b"lib");

    let hdr_alias = root.join("Include/shared/DriverSpecs.h");
    assert_eq!(std::fs::read_link(&hdr_alias).unwrap(), Path::new("driverspecs.h"));

    // Idempotent: a second run finds nothing new to create and does not fail.
    add_case_aliases(root).unwrap();
    assert_eq!(std::fs::read(&hdr_alias).unwrap(), b"// real");
}
