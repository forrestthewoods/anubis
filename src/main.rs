mod papyrus;
mod papyrus_serde;

use anyhow::{anyhow, bail};
use logos::Logos;
use papyrus::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;


struct CppToolchain {
    compiler: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let filename = "C:/source_control/anubis/examples/simple_cpp/ANUBIS";
    let src = fs::read_to_string(&filename)?;

    let mut lexer = Token::lexer(&src);
    let result = parse_config(&mut lexer);

    match result {
        Ok(config) => {
            //println!("{:#?}", config);

            let resolve_root = PathBuf::from_str("c:/source_control/anubis/examples/simple_cpp")?;
            let resolve_vars: HashMap<String, String> = [("platform", "windows"), ("arch", "x64")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();

            let config = resolve_value(config, &resolve_root, &resolve_vars)?;

            let rules: Vec<CppBinary> = match config {
                Value::Array(arr) => arr
                    .into_iter()
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
        }
        Err(e) => {
            use ariadne::{ColorGenerator, Label, Report, ReportKind, Source};

            let mut colors = ColorGenerator::new();
            let a = colors.next();

            Report::build(ReportKind::Error, &filename, 12)
                .with_message("Invalid ANUBIS".to_string())
                .with_label(
                    Label::new((&filename, e.span))
                        .with_message(e.error)
                        .with_color(a),
                )
                .finish()
                .eprint((&filename, Source::from(src)))
                .unwrap();
        }
    }

    Ok(())
}
