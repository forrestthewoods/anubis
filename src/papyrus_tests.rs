use crate::papyrus::*;
use crate::rules::cc_rules::CcBinary;
use anyhow::Result;
use logos::{Lexer, Logos, Span};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[test]
fn test_parse_valid_config() -> Result<()> {
    let config_str = r#"
    cpp_binary(
        name = "test_binary",
        srcs = [ "main.cpp" ],
        srcs2 = glob([
            "*.cpp",
            "*.h",
            "src/**/*.cpp",
        ]),
        srcs3 = select(
            (platform, arch) => {
                (windows, x64) = ["foo", "foofoo"],
                (linux | macos, _) = ["bar"],
                default = ["baz"],
            }
        ),
        srcs4 = ["foo"] + ["bar"] + select(
            (platform) => {
                default = ["baz"]
            })
    )
    "#;

    let value = crate::papyrus::read_papyrus_str(&config_str, &"test")?;
    assert!(matches!(value, crate::papyrus::Value::Array(_)));
    Ok(())
}

#[test]
fn test_parse_invalid_config() {
    let config_str = r#"
    cpp_binary(
        name = "test_binary",
        srcs = [ "main.cpp" ],
        srcs2 = glob([
            "*.cpp",
            "*.h",
            "src/**/*.cpp",
        ]),
        srcs3 = select(
            (platform, arch) => {
                (windows, x64) = ["foo", "foofoo"],
                (linux | macos, _) = ["bar"],
                default = ["baz"],
            }
        ),
        srcs4 = ["foo"] + ["bar"] + select(
            (platform) => {
                default = ["baz"]
            })
    // Missing closing parenthesis
    "#;

    let result = crate::papyrus::read_papyrus_str(&config_str, &"test");
    assert!(result.is_err());
}

#[test]
fn test_deserialize_valid_object() -> Result<()> {
    let config_str = r#"
    cc_binary(
        name = "test_binary",
        lang = "cpp",
        srcs = [ "main.cpp" ],
    )
    "#;

    let value = crate::papyrus::read_papyrus_str(&config_str, &"test")?;
    let binary: CcBinary = value.deserialize_named_object("test_binary")?;
    assert_eq!(binary.name, "test_binary");
    Ok(())
}

#[test]
fn test_deserialize_missing_object() -> Result<()> {
    let config_str = r#"
    cc_binary(
        name = "test_binary",
        lang = "cpp",
        srcs = [ "main.cpp" ],
    )
    "#;

    let value = crate::papyrus::read_papyrus_str(&config_str, &"test")?;
    let result: Result<CcBinary> = value.deserialize_named_object("non_existent");
    assert!(result.is_err());
    Ok(())
}

// Basic parsing tests
#[test]
fn test_parse_basic_types() -> Result<()> {
    let config_str = r#"
    test_rule(
        string = "hello world",
        array = ["a", "b", "c"],
        map = { key1 = "value1", key2 = "value2" },
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    if let Value::Array(arr) = value {
        if let Value::Object(obj) = &arr[0] {
            assert_eq!(obj.typename, "test_rule");
            if let Value::String(s) = &obj.fields[&Identifier("string".to_string())] {
                assert_eq!(s, "hello world");
            } else {
                panic!("Expected string value");
            }
            if let Value::Array(arr) = &obj.fields[&Identifier("array".to_string())] {
                assert_eq!(arr.len(), 3);
            } else {
                panic!("Expected array value");
            }
            if let Value::Map(map) = &obj.fields[&Identifier("map".to_string())] {
                assert_eq!(map.len(), 2);
            } else {
                panic!("Expected map value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_parse_invalid_syntax() {
    let invalid_cases = vec![
        // Missing closing parenthesis
        r#"test_rule(name = "test""#,
        // Invalid map syntax
        r#"test_rule(map = { key1: "value1" })"#,
        // Unclosed string
        r#"test_rule(name = "test)"#,
        // Invalid array syntax
        r#"test_rule(arr = [1, 2,])"#,
    ];

    for case in invalid_cases {
        assert!(read_papyrus_str(case, "test").is_err());
    }
}

// Glob tests
#[test]
fn test_glob_parsing_simple() -> Result<()> {
    let config_str = r#"
    test_rule(
        files = glob([
            "*.cpp",
            "src/**/*.h"
        ])
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    if let Value::Array(arr) = value {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Glob(glob) = &obj.fields[&Identifier("files".to_string())] {
                assert_eq!(glob.includes.len(), 2);
                assert_eq!(glob.includes[0], "*.cpp");
                assert_eq!(glob.includes[1], "src/**/*.h");
            } else {
                panic!("Expected glob value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_glob_parsing_with_exclude() -> Result<()> {
    let config_str = r#"
    test_rule(
        files = glob(
            includes = ["*.cpp", "src/**/*.h"],
            excludes = ["*_template.cpp"],
        )
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    if let Value::Array(arr) = value {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Glob(glob) = &obj.fields[&Identifier("files".to_string())] {
                assert_eq!(glob.includes.len(), 2);
                assert_eq!(glob.includes[0], "*.cpp");
                assert_eq!(glob.includes[1], "src/**/*.h");

                assert_eq!(glob.excludes.len(), 1);
                assert_eq!(glob.excludes[0], "*_template.cpp");
            } else {
                panic!("Expected glob value");
            }
        }
    }
    Ok(())
}

// Select tests
#[test]
fn test_select_parsing() -> Result<()> {
    let config_str = r#"
    test_rule(
        config = select(
            (platform, arch) => {
                (windows, x64) = "win64",
                (linux, _) = "linux",
                default = "unknown"
            }
        )
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    if let Value::Array(arr) = value {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Select(select) = &obj.fields[&Identifier("config".to_string())] {
                assert_eq!(select.inputs.len(), 2);
                assert_eq!(select.filters.len(), 3);
            } else {
                panic!("Expected select value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_select_resolution() -> Result<()> {
    let config_str = r#"
    test_rule(
        config = select(
            (platform, arch) => {
                (windows, x64) = "win64",
                (linux, _) = "linux",
                default = "unknown"
            }
        )
    )
    "#;

    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "windows".to_string());
    vars.insert("arch".to_string(), "x64".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::String(s) = &obj.fields[&Identifier("config".to_string())] {
                assert_eq!(s, "win64");
            } else {
                panic!("Expected resolved string value");
            }
        }
    }
    Ok(())
}

// Concatenation tests
#[test]
fn test_concat_arrays() -> Result<()> {
    let config_str = r#"
    test_rule(
        files = ["a", "b"] + ["c", "d"]
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &HashMap::new())?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(files) = &obj.fields[&Identifier("files".to_string())] {
                assert_eq!(files.len(), 4);
                assert_eq!(
                    files,
                    &[
                        Value::String("a".to_owned()),
                        Value::String("b".to_owned()),
                        Value::String("c".to_owned()),
                        Value::String("d".to_owned()),
                    ]
                );
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_concat_arrays_select() -> Result<()> {
    let config_str = r#"
    test_rule(
        files = ["a", "b"] + select(
            (platform) => {
                (windows) = ["c", "d"]
            }
        )
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;

    let mut vars = HashMap::<String, String>::new();
    vars.insert("platform".into(), "windows".into());
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(files) = &obj.fields[&Identifier("files".to_string())] {
                assert_eq!(files.len(), 4);
                assert_eq!(
                    files,
                    &[
                        Value::String("a".to_owned()),
                        Value::String("b".to_owned()),
                        Value::String("c".to_owned()),
                        Value::String("d".to_owned()),
                    ]
                );
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_concat_objects() -> Result<()> {
    let config_str = r#"
    test_rule(
        files = ["a", "b"]
    ) 
    +
    test_rule(
        files = ["c", "d"]
        name = "John"
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &HashMap::new())?;

    if let Value::Array(arr) = resolved {
        assert_eq!(arr.len(), 1);

        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(files) = &obj.fields[&Identifier("files".to_string())] {
                assert_eq!(files.len(), 4);
                assert_eq!(
                    files,
                    &[
                        Value::String("a".to_owned()),
                        Value::String("b".to_owned()),
                        Value::String("c".to_owned()),
                        Value::String("d".to_owned()),
                    ]
                );
            } else {
                panic!("Expected array value");
            }

            if let Value::String(name) = &obj.fields[&Identifier("name".to_string())] {
                assert_eq!(name, "John");
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_invalid_concat() {
    let config_str = r#"
    test_rule(
        invalid = "string" + ["array"]
    )
    "#;

    let value = read_papyrus_str(config_str, "test").unwrap();
    let result = resolve_value(value, &PathBuf::from("."), &HashMap::new());
    assert!(result.is_err());
}

// Complex object tests
#[test]
fn test_nested_objects() -> Result<()> {
    let config_str = r#"
    parent_rule(
        name = "parent",
        child = child_rule(
            name = "child",
            value = "nested"
        )
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    if let Value::Array(arr) = value {
        if let Value::Object(obj) = &arr[0] {
            assert_eq!(obj.typename, "parent_rule");
            if let Value::Object(child) = &obj.fields[&Identifier("child".to_string())] {
                assert_eq!(child.typename, "child_rule");
                if let Value::String(s) = &child.fields[&Identifier("value".to_string())] {
                    assert_eq!(s, "nested");
                } else {
                    panic!("Expected string value in nested object");
                }
            } else {
                panic!("Expected nested object");
            }
        }
    }
    Ok(())
}

// Resolution tests
#[test]
fn test_resolve_with_vars() -> Result<()> {
    let config_str = r#"
    test_rule(
        value = select(
            (env) => {
                (dev) = "development",
                (prod) = "production",
                default = "unknown"
            }
        )
    )
    "#;

    let mut vars = HashMap::new();
    vars.insert("env".to_string(), "dev".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::String(s) = &obj.fields[&Identifier("value".to_string())] {
                assert_eq!(s, "development");
            } else {
                panic!("Expected string value after resolution");
            }
        }
    }
    Ok(())
}

#[test]
fn test_resolve_missing_vars() -> Result<()> {
    let config_str = r#"
    test_rule(
        value = select(
            () => {
                default = "unknown"
            }
        )
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &HashMap::new())?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::String(s) = &obj.fields[&Identifier("value".to_string())] {
                assert_eq!(s, "unknown", "Should use default value when vars are missing");
            } else {
                panic!("Expected string value after resolution");
            }
        }
    }
    Ok(())
}

// Deserialization tests
#[derive(Debug, serde::Deserialize)]
struct TestRule {
    name: String,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_option_string")]
    value: Option<String>,
}

fn deserialize_option_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(String::deserialize(deserializer)?))
}

impl PapyrusObjectType for TestRule {
    fn name() -> &'static str {
        "test_rule"
    }
}

#[test]
fn test_deserialize_objects() -> Result<()> {
    let config_str = r#"
    test_rule(
        name = "test1"
    )
    test_rule(
        name = "test2",
        value = "value2"
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let objects: Vec<TestRule> = value.deserialize_objects()?;
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[0].name, "test1");
    assert!(objects[0].value.is_none());
    assert_eq!(objects[1].value.as_ref().unwrap(), "value2");
    Ok(())
}

#[test]
fn test_deserialize_named_object() -> Result<()> {
    let config_str = r#"
    test_rule(
        name = "specific"
    )
    test_rule(
        name = "other",
        value = "not_this_one"
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let object: TestRule = value.deserialize_named_object("specific")?;
    assert_eq!(object.name, "specific");
    assert!(object.value.is_none());
    Ok(())
}

// ============================================================================
// Unresolved value tests
// ============================================================================

#[test]
fn test_select_no_match_returns_unresolved() -> Result<()> {
    let config_str = r#"
    test_rule(
        config = select(
            (platform) => {
                (windows) = "win",
                (linux) = "lin"
            }
        )
    )
    "#;

    // Resolve with vars that don't match any filter
    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "macos".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    // The entire object should be unresolved because the config field is unresolved
    if let Value::Array(arr) = &resolved {
        assert!(arr[0].is_unresolved(), "Object should be marked as unresolved");
        let info = arr[0].as_unresolved().expect("Should have unresolved info");
        assert!(info.reason.contains("select()"));
        assert_eq!(info.select_inputs, vec!["platform"]);
        assert_eq!(info.select_values, vec!["macos"]);
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_select_with_default_still_resolves() -> Result<()> {
    let config_str = r#"
    test_rule(
        name = "test",
        config = select(
            (platform) => {
                (windows) = "win",
                default = "fallback"
            }
        )
    )
    "#;

    // Resolve with vars that don't match explicit filter but should use default
    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "macos".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    // Should resolve to default value
    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::String(s) = &obj.fields[&Identifier("config".to_string())] {
                assert_eq!(s, "fallback");
            } else {
                panic!("Expected string value");
            }
        } else {
            panic!("Expected object");
        }
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_unresolved_propagates_through_concat() -> Result<()> {
    let config_str = r#"
    test_rule(
        values = ["a", "b"] + select(
            (platform) => {
                (windows) = ["c"]
            }
        )
    )
    "#;

    // Resolve with vars that don't match any filter
    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "linux".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    // The object should be unresolved because the concat result is unresolved
    if let Value::Array(arr) = &resolved {
        assert!(arr[0].is_unresolved(), "Object should be unresolved due to concat with unresolved select");
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_multiple_rules_partial_resolution() -> Result<()> {
    let config_str = r#"
    test_rule(
        name = "windows_only",
        value = select(
            (platform) => {
                (windows) = "win_value"
            }
        )
    )
    test_rule(
        name = "all_platforms",
        value = "universal"
    )
    "#;

    // Resolve for linux platform
    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "linux".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    // First rule should be unresolved, second should be resolved
    if let Value::Array(arr) = &resolved {
        assert_eq!(arr.len(), 2);
        assert!(arr[0].is_unresolved(), "windows_only should be unresolved on linux");
        assert!(!arr[1].is_unresolved(), "all_platforms should be resolved");

        if let Value::Object(obj) = &arr[1] {
            assert_eq!(obj.fields.get("name"), Some(&Value::String("all_platforms".to_string())));
            assert_eq!(obj.fields.get("value"), Some(&Value::String("universal".to_string())));
        }
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_unresolved_info_contains_diagnostic_data() -> Result<()> {
    let config_str = r#"
    test_rule(
        platform_specific = select(
            (platform, arch) => {
                (windows, x64) = "win64",
                (linux, arm64) = "linarm"
            }
        )
    )
    "#;

    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "macos".to_string());
    vars.insert("arch".to_string(), "x64".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = &resolved {
        let info = arr[0].as_unresolved().expect("Should have unresolved info");

        // Check diagnostic info is complete
        assert_eq!(info.select_inputs, vec!["platform", "arch"]);
        assert_eq!(info.select_values, vec!["macos", "x64"]);
        assert!(info.available_filters.len() == 2, "Should list the 2 available filters");
        assert!(info.reason.contains("macos"));
        assert!(info.reason.contains("x64"));
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_deserialize_unresolved_fails_with_info() {
    let config_str = r#"
    test_rule(
        name = "unresolved_target",
        value = select(
            (platform) => {
                (windows) = "win_value"
            }
        )
    )
    "#;

    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "linux".to_string());

    let value = read_papyrus_str(config_str, "test").unwrap();
    let resolved = resolve_value(value, &PathBuf::from("."), &vars).unwrap();

    // Attempting to deserialize an unresolved object should fail with detailed error
    let result: Result<TestRule> = resolved.deserialize_named_object("unresolved_target");
    assert!(result.is_err(), "Deserializing unresolved value should fail");

    let err_msg = format!("{}", result.unwrap_err());
    // Error should contain diagnostic info
    assert!(err_msg.contains("unresolved"), "Error should mention unresolved");
}

// ============================================================================
// Relative target resolution tests
// ============================================================================

#[test]
fn test_relative_target_resolved_with_dir() -> Result<()> {
    let config_str = r#"
    test_rule(
        name = "my_target",
        deps = Targets([":relative_dep", "//absolute/path:target"])
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;

    // Resolve with a directory relative path
    let resolved = resolve_value_with_dir(
        value,
        &PathBuf::from("."),
        &HashMap::new(),
        Some("examples/myproject")
    )?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Targets(deps) = &obj.fields[&Identifier("deps".to_string())] {
                assert_eq!(deps.len(), 2);
                // First dep should be resolved to absolute path
                assert_eq!(deps[0], Value::Target("//examples/myproject:relative_dep".to_string()));
                // Second dep should remain unchanged (already absolute)
                assert_eq!(deps[1], Value::Target("//absolute/path:target".to_string()));
            } else {
                panic!("Expected deps to be an array");
            }
        } else {
            panic!("Expected object");
        }
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_relative_target_without_dir_unchanged() -> Result<()> {
    let config_str = r#"
    test_rule(
        name = "my_target",
        deps = [":relative_dep"]
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;

    // Resolve without a directory path (using the basic resolve_value)
    let resolved = resolve_value(value, &PathBuf::from("."), &HashMap::new())?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(deps) = &obj.fields[&Identifier("deps".to_string())] {
                assert_eq!(deps.len(), 1);
                // Relative dep should remain unchanged when no dir_relpath is provided
                assert_eq!(deps[0], Value::String(":relative_dep".to_string()));
            } else {
                panic!("Expected deps to be an array");
            }
        } else {
            panic!("Expected object");
        }
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_relative_target_in_nested_select() -> Result<()> {
    let config_str = r#"
    test_rule(
        name = "my_target",
        deps = select(
            (platform) => {
                (windows) = [Target(":win_dep")],
                (linux) = [Target(":linux_dep")],
                default = []
            }
        )
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;

    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "windows".to_string());

    // Resolve with a directory relative path
    let resolved = resolve_value_with_dir(
        value,
        &PathBuf::from("."),
        &vars,
        Some("libs/mylib")
    )?;

    if let Value::Array(ref arr) = &resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(deps) = &obj.fields[&Identifier("deps".to_string())] {
                assert_eq!(deps.len(), 1);
                // Relative dep should be resolved
                assert_eq!(deps[0], Value::Target("//libs/mylib:win_dep".to_string()));
            } else {
                panic!("Expected deps to be an array. found [{:?}]", &obj);
            }
        } else {
            panic!("Expected object. found [{:?}]", &resolved);
        }
    } else {
        panic!("Expected array. found [{:?}]", &resolved);
    }
    Ok(())
}

#[test]
fn test_relative_target_in_concat() -> Result<()> {
    let config_str = r#"
    test_rule(
        name = "my_target",
        deps = [Target(":dep1")] + [Target(":dep2"), Target("//other:dep3")]
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;

    // Resolve with a directory relative path
    let resolved = resolve_value_with_dir(
        value,
        &PathBuf::from("."),
        &HashMap::new(),
        Some("path/to/module")
    )?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(deps) = &obj.fields[&Identifier("deps".to_string())] {
                assert_eq!(deps.len(), 3);
                assert_eq!(deps[0], Value::Target("//path/to/module:dep1".to_string()));
                assert_eq!(deps[1], Value::Target("//path/to/module:dep2".to_string()));
                // Absolute path should remain unchanged
                assert_eq!(deps[2], Value::Target("//other:dep3".to_string()));
            } else {
                panic!("Expected deps to be an array");
            }
        } else {
            panic!("Expected object");
        }
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_relative_target_pattern_detection() {
    use crate::papyrus::{is_relative_target, resolve_relative_target};

    // Valid relative targets
    assert!(is_relative_target(":foo"));
    assert!(is_relative_target(":my_target"));
    assert!(is_relative_target(":a"));

    // Invalid - not relative targets
    assert!(!is_relative_target(""));              // empty
    assert!(!is_relative_target(":"));             // just colon
    assert!(!is_relative_target("//path:target")); // absolute target
    assert!(!is_relative_target("normal_string")); // regular string
    assert!(!is_relative_target(":/path"));        // has slash after colon
}

#[test]
fn test_resolve_relative_target_function() {
    use crate::papyrus::resolve_relative_target;

    assert_eq!(
        resolve_relative_target(":foo", "examples/bar"),
        "//examples/bar:foo"
    );
    assert_eq!(
        resolve_relative_target(":my_target", "libs/common"),
        "//libs/common:my_target"
    );
    assert_eq!(
        resolve_relative_target(":x", "a/b/c"),
        "//a/b/c:x"
    );
}

// ============================================================================
// Target/Targets parsing and resolution tests
// ============================================================================

#[test]
fn test_target_parsing() -> Result<()> {
    let config_str = r#"
    test_rule(
        single_dep = Target(":local_lib"),
        absolute_dep = Target("//path/to:other_lib")
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;

    if let Value::Array(arr) = value {
        if let Value::Object(obj) = &arr[0] {
            // Check single_dep
            if let Value::Target(target) = &obj.fields[&Identifier("single_dep".to_string())] {
                assert_eq!(target, ":local_lib");
            } else {
                panic!("Expected Target value for single_dep");
            }
            // Check absolute_dep
            if let Value::Target(target) = &obj.fields[&Identifier("absolute_dep".to_string())] {
                assert_eq!(target, "//path/to:other_lib");
            } else {
                panic!("Expected Target value for absolute_dep");
            }
        } else {
            panic!("Expected Object");
        }
    } else {
        panic!("Expected Array");
    }

    Ok(())
}

#[test]
fn test_targets_parsing() -> Result<()> {
    let config_str = r#"
    test_rule(
        deps = Targets([":lib1", ":lib2", "//other:lib3"])
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;

    if let Value::Array(arr) = value {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Targets(targets) = &obj.fields[&Identifier("deps".to_string())] {
                assert_eq!(targets.len(), 3);
                assert_eq!(targets[0], ":lib1");
                assert_eq!(targets[1], ":lib2");
                assert_eq!(targets[2], "//other:lib3");
            } else {
                panic!("Expected Targets value");
            }
        } else {
            panic!("Expected Object");
        }
    } else {
        panic!("Expected Array");
    }

    Ok(())
}

#[test]
fn test_target_resolution_with_dir() -> Result<()> {
    let config_str = r#"
    test_rule(
        dep = Target(":local_lib")
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value_with_dir(
        value,
        &PathBuf::from("/project"),
        &HashMap::new(),
        Some("examples/mylib"),
    )?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Target(target) = &obj.fields[&Identifier("dep".to_string())] {
                assert_eq!(target, "//examples/mylib:local_lib");
            } else {
                panic!("Expected Target value");
            }
        } else {
            panic!("Expected Object");
        }
    } else {
        panic!("Expected Array");
    }

    Ok(())
}

#[test]
fn test_targets_resolution_with_dir() -> Result<()> {
    let config_str = r#"
    test_rule(
        deps = Targets([":lib1", ":lib2", "//other:lib3"])
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value_with_dir(
        value,
        &PathBuf::from("/project"),
        &HashMap::new(),
        Some("examples/myapp"),
    )?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Targets(targets) = &obj.fields[&Identifier("deps".to_string())] {
                assert_eq!(targets.len(), 3);
                // Relative targets should be resolved
                assert_eq!(targets[0], "//examples/myapp:lib1");
                assert_eq!(targets[1], "//examples/myapp:lib2");
                // Absolute target should be unchanged
                assert_eq!(targets[2], "//other:lib3");
            } else {
                panic!("Expected Targets value");
            }
        } else {
            panic!("Expected Object");
        }
    } else {
        panic!("Expected Array");
    }

    Ok(())
}

#[test]
fn test_targets_concat() -> Result<()> {
    let config_str = r#"
    test_rule(
        deps = Targets([":lib1"]) + Targets([":lib2", ":lib3"])
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value_with_dir(
        value,
        &PathBuf::from("/project"),
        &HashMap::new(),
        Some("examples/myapp"),
    )?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Targets(targets) = &obj.fields[&Identifier("deps".to_string())] {
                assert_eq!(targets.len(), 3);
                assert_eq!(targets[0], "//examples/myapp:lib1");
                assert_eq!(targets[1], "//examples/myapp:lib2");
                assert_eq!(targets[2], "//examples/myapp:lib3");
            } else {
                panic!("Expected Targets value");
            }
        } else {
            panic!("Expected Object");
        }
    } else {
        panic!("Expected Array");
    }

    Ok(())
}

// ============================================================================
// String + Path concatenation tests
// ============================================================================

#[test]
fn test_concat_string_and_relpath() -> Result<()> {
    let config_str = r#"
    test_rule(
        flag = "-isysroot=" + RelPath("./empty_dir")
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("/project"), &HashMap::new())?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::String(flag) = &obj.fields[&Identifier("flag".to_string())] {
                // The path should be resolved relative to /project
                assert!(flag.starts_with("-isysroot="), "Flag should start with -isysroot=");
                assert!(flag.contains("empty_dir"), "Flag should contain the path");
            } else {
                panic!("Expected string value after concatenation");
            }
        } else {
            panic!("Expected object");
        }
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_concat_string_and_relpath_in_array() -> Result<()> {
    let config_str = r#"
    test_rule(
        flags = [
            "-isysroot=" + RelPath("./sysroot"),
            "-I" + RelPath("./include")
        ]
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("/project"), &HashMap::new())?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(flags) = &obj.fields[&Identifier("flags".to_string())] {
                assert_eq!(flags.len(), 2);

                if let Value::String(flag0) = &flags[0] {
                    assert!(flag0.starts_with("-isysroot="), "First flag should start with -isysroot=");
                    assert!(flag0.contains("sysroot"), "First flag should contain sysroot");
                } else {
                    panic!("Expected string for first flag");
                }

                if let Value::String(flag1) = &flags[1] {
                    assert!(flag1.starts_with("-I"), "Second flag should start with -I");
                    assert!(flag1.contains("include"), "Second flag should contain include");
                } else {
                    panic!("Expected string for second flag");
                }
            } else {
                panic!("Expected array value");
            }
        } else {
            panic!("Expected object");
        }
    } else {
        panic!("Expected array");
    }
    Ok(())
}

// ============================================================================
// Multi-select tests
// ============================================================================

#[test]
fn test_multi_select_parsing() -> Result<()> {
    let config_str = r#"
    test_rule(
        config = multi_select(
            (platform, arch) => {
                (windows, x64) = ["win64"],
                (linux, _) = ["linux"],
                default = ["fallback"]
            }
        )
    )
    "#;

    let value = read_papyrus_str(config_str, "test")?;
    if let Value::Array(arr) = value {
        if let Value::Object(obj) = &arr[0] {
            if let Value::MultiSelect(select) = &obj.fields[&Identifier("config".to_string())] {
                assert_eq!(select.inputs.len(), 2);
                assert_eq!(select.filters.len(), 3);
            } else {
                panic!("Expected multi_select value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_multi_select_single_match() -> Result<()> {
    let config_str = r#"
    test_rule(
        flags = multi_select(
            (platform) => {
                (windows) = ["-DWINDOWS"],
                (linux) = ["-DLINUX"],
                default = ["-DUNKNOWN"]
            }
        )
    )
    "#;

    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "windows".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(flags) = &obj.fields[&Identifier("flags".to_string())] {
                assert_eq!(flags.len(), 1);
                assert_eq!(flags[0], Value::String("-DWINDOWS".to_string()));
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_multi_select_multiple_matches() -> Result<()> {
    let config_str = r#"
    test_rule(
        flags = multi_select(
            (platform, arch) => {
                (windows, _) = ["-DWINDOWS"],
                (_, x64) = ["-DX64"],
                default = ["-DDEFAULT"]
            }
        )
    )
    "#;

    // Both (windows, _) and (_, x64) should match
    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "windows".to_string());
    vars.insert("arch".to_string(), "x64".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(flags) = &obj.fields[&Identifier("flags".to_string())] {
                // Should have both -DWINDOWS and -DX64 concatenated
                assert_eq!(flags.len(), 2, "Expected 2 flags from multi_select");
                assert_eq!(flags[0], Value::String("-DWINDOWS".to_string()));
                assert_eq!(flags[1], Value::String("-DX64".to_string()));
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_multi_select_default_only_when_no_match() -> Result<()> {
    let config_str = r#"
    test_rule(
        flags = multi_select(
            (platform) => {
                (windows) = ["-DWINDOWS"],
                (linux) = ["-DLINUX"],
                default = ["-DUNKNOWN"]
            }
        )
    )
    "#;

    // No explicit match, should use default
    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "macos".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(flags) = &obj.fields[&Identifier("flags".to_string())] {
                assert_eq!(flags.len(), 1);
                assert_eq!(flags[0], Value::String("-DUNKNOWN".to_string()));
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_multi_select_default_ignored_when_match_exists() -> Result<()> {
    let config_str = r#"
    test_rule(
        flags = multi_select(
            (platform) => {
                (windows) = ["-DWINDOWS"],
                default = ["-DDEFAULT"]
            }
        )
    )
    "#;

    // Explicit match exists, default should NOT be included
    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "windows".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(flags) = &obj.fields[&Identifier("flags".to_string())] {
                // Should only have -DWINDOWS, NOT the default
                assert_eq!(flags.len(), 1);
                assert_eq!(flags[0], Value::String("-DWINDOWS".to_string()));
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_multi_select_preserves_order() -> Result<()> {
    let config_str = r#"
    test_rule(
        flags = multi_select(
            (a, b) => {
                (_, yes) = ["third"],
                (yes, _) = ["first"],
                (yes, yes) = ["second"],
            }
        )
    )
    "#;

    let mut vars = HashMap::new();
    vars.insert("a".to_string(), "yes".to_string());
    vars.insert("b".to_string(), "yes".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(flags) = &obj.fields[&Identifier("flags".to_string())] {
                // All three should match, in config file order
                assert_eq!(flags.len(), 3);
                assert_eq!(flags[0], Value::String("third".to_string()));
                assert_eq!(flags[1], Value::String("first".to_string()));
                assert_eq!(flags[2], Value::String("second".to_string()));
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_multi_select_no_match_no_default_returns_unresolved() -> Result<()> {
    let config_str = r#"
    test_rule(
        flags = multi_select(
            (platform) => {
                (windows) = ["-DWINDOWS"],
                (linux) = ["-DLINUX"]
            }
        )
    )
    "#;

    // No match and no default
    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "macos".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = &resolved {
        assert!(arr[0].is_unresolved(), "Should be unresolved when no match and no default");
        let info = arr[0].as_unresolved().expect("Should have unresolved info");
        assert!(info.reason.contains("multi_select()"));
    } else {
        panic!("Expected array");
    }
    Ok(())
}

#[test]
fn test_multi_select_with_concat() -> Result<()> {
    let config_str = r#"
    test_rule(
        flags = ["-DBASE"] + multi_select(
            (platform) => {
                (windows) = ["-DWINDOWS"],
                (linux) = ["-DLINUX"],
                default = []
            }
        )
    )
    "#;

    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "windows".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(flags) = &obj.fields[&Identifier("flags".to_string())] {
                assert_eq!(flags.len(), 2);
                assert_eq!(flags[0], Value::String("-DBASE".to_string()));
                assert_eq!(flags[1], Value::String("-DWINDOWS".to_string()));
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_multi_select_with_or_patterns() -> Result<()> {
    let config_str = r#"
    test_rule(
        flags = multi_select(
            (platform) => {
                (windows | linux) = ["-DDESKTOP"],
                (linux | macos) = ["-DUNIX"],
            }
        )
    )
    "#;

    // linux matches both patterns
    let mut vars = HashMap::new();
    vars.insert("platform".to_string(), "linux".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(obj) = &arr[0] {
            if let Value::Array(flags) = &obj.fields[&Identifier("flags".to_string())] {
                assert_eq!(flags.len(), 2);
                assert_eq!(flags[0], Value::String("-DDESKTOP".to_string()));
                assert_eq!(flags[1], Value::String("-DUNIX".to_string()));
            } else {
                panic!("Expected array value");
            }
        }
    }
    Ok(())
}

#[test]
fn test_multi_select_with_objects() -> Result<()> {
    let config_str = r#"
    test_rule(
        config = multi_select(
            (mode) => {
                (debug) = inner(a = ["debug_a"]),
                (verbose) = inner(b = ["verbose_b"]),
            }
        )
    )
    "#;

    // When only one matches, should return that object
    let mut vars = HashMap::new();
    vars.insert("mode".to_string(), "debug".to_string());

    let value = read_papyrus_str(config_str, "test")?;
    let resolved = resolve_value(value, &PathBuf::from("."), &vars)?;

    if let Value::Array(arr) = resolved {
        if let Value::Object(outer_obj) = &arr[0] {
            if let Value::Object(inner_obj) = &outer_obj.fields[&Identifier("config".to_string())] {
                assert_eq!(inner_obj.typename, "inner");
                assert!(inner_obj.fields.contains_key("a"));
            } else {
                panic!("Expected object value");
            }
        }
    }
    Ok(())
}
