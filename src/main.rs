mod papyrus;
mod papyrus_serde;

use anyhow::{anyhow, bail};
use logos::Logos;
use papyrus::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Debug, Deserialize)]
pub struct CppBinary {
    pub name: String,
    pub srcs: Vec<String>,
    pub srcs2: Vec<PathBuf>,
    pub srcs3: Vec<String>,
    pub srcs4: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct CppToolchain {
    compiler: PathBuf,
    system_include_dirs : Vec<PathBuf>,
    library_dirs: Vec<PathBuf>,
    libraries: Vec<PathBuf>,
    flags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct Toolchain {
    cpp: CppToolchain
}

fn read_papyrus(path: &Path) -> anyhow::Result<papyrus::Value> {
    let src = fs::read_to_string(path)?;

    let mut lexer = Token::lexer(&src);
    let result = parse_config(&mut lexer);

    match result {
        Ok(value) => {
            //println!("{:#?}", config);

            let resolve_root = PathBuf::from_str("c:/source_control/anubis/examples/simple_cpp")?;
            let resolve_vars: HashMap<String, String> = [("platform", "windows"), ("arch", "x64")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();

            let value = resolve_value(value, &resolve_root, &resolve_vars)?;

            Ok(value)
        }
        Err(e) => {
            use ariadne::{ColorGenerator, Label, Report, ReportKind, Source};

            let mut colors = ColorGenerator::new();
            let a = colors.next();
            
            let s = path.to_string_lossy().to_string();
            Report::build(ReportKind::Error, path, 12)
                .with_message("Invalid ANUBIS".to_string())
                .with_label(
                    Label::new((path, e.span))
                        .with_message(e.error)
                        .with_color(a),
                )
                .finish()
                .eprint((s.as_str(), Source::from(src)))
                .unwrap();

            bail!("oh no")
        }
    }
}


fn main() -> anyhow::Result<()> {
    let config_path = "C:/source_control/anubis/examples/simple_cpp/ANUBIS";
    let config = read_papyrus(&Path::new(config_path))?;
    
    let rules: Vec<CppBinary> = match config {
        Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| {
                match &v {
                    Value::Object(obj) => if obj.typename == "cpp_binary" { Some(v) } else { None },
                    _ => None
                }
            })
            .map(|v| {
                let de = crate::papyrus_serde::ValueDeserializer::new(v);
                CppBinary::deserialize(de).map_err(|e| anyhow!("{}", e))
            })
            .collect::<Result<Vec<CppBinary>, anyhow::Error>>()?,
        _ => bail!("Expected config root to be an array"),
    };

    for rule in &rules {
        println!("{:#?}", rule);
    }

    Ok(())
}
