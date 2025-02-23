use crate::cpp_rules::CppBinary;
use crate::papyrus::*;
use anyhow::Result;
use logos::{Lexer, Logos, Span};

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
    cpp_binary(
        name = "test_binary",
        srcs = [ "main.cpp" ],
    )
    "#;

    let value = crate::papyrus::read_papyrus_str(&config_str, &"test")?;
    let binary: CppBinary = value.deserialize_named_object("test_binary")?;
    assert_eq!(binary.name, "test_binary");
    Ok(())
}

#[test]
fn test_deserialize_missing_object() -> Result<()> {
    let config_str = r#"
    cpp_binary(
        name = "test_binary",
        srcs = [ "main.cpp" ],
    )
    "#;

    let value = crate::papyrus::read_papyrus_str(&config_str, &"test")?;
    let result: Result<CppBinary> = value.deserialize_named_object("non_existent");
    assert!(result.is_err());
    Ok(())
} 
