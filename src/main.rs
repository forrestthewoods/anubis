#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use anyhow::{anyhow, bail, Context};
use logos::{Lexer, Logos, Span};

use std::collections::HashMap;
use std::fs;
use std::hash::DefaultHasher;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

mod serde_impl;
use crate::serde_impl::ValueDeserializer;

#[derive(Debug, Logos, PartialEq)]
#[logos(skip r"[ \t\r\n\f]+")]
enum Token<'source> {
    #[token("false", |_| false)]
    #[token("true", |_| true)]
    Bool(bool),

    #[token("{")]
    BraceOpen,

    #[token("}")]
    BraceClose,

    #[token("[")]
    BracketOpen,

    #[token("]")]
    BracketClose,

    #[token(":")]
    Colon,

    #[token(",")]
    Comma,

    #[token("=")]
    Equals,

    #[token("=>")]
    Arrow,

    #[token("|")]
    Pipe,

    #[token("_", priority = 100)]
    Underscore,

    // do we need null? null sucks
    //#[token("null")]
    //Null,
    #[regex(r"-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?", |lex| lex.slice().parse::<f64>().unwrap())]
    Number(f64),

    #[token("(")]
    ParenOpen,

    #[token(")")]
    ParenClose,

    #[regex(r#"[a-zA-Z_][a-zA-Z0-9_\-\.]*"#, |lex| lex.slice())]
    Identifier(&'source str),

    #[regex(r#""([^"\\]|\\["\\bnfrt]|u[a-fA-F0-9]{4})*""#, |lex| {
        // Trim quotes
        let s = lex.slice();
        &s[1..s.len()-1]})]
    String(&'source str),

    #[token("glob")]
    Glob,

    #[token("select")]
    Select,

    #[token("default")]
    Default,

    #[regex(r#"#[^\r\n]*"#, logos::skip)]
    Comment,
}

type ParseResult<T> = anyhow::Result<T>;

#[derive(Debug)]
struct SpannedError {
    error: anyhow::Error,
    span: Span, // Or whatever your span type is
}

struct PeekLexer<'source> {
    lexer: &'source mut Lexer<'source, Token<'source>>,
    peeked: Option<Option<Result<Token<'source>, ()>>>,
}

impl<'source> PeekLexer<'source> {
    fn peek(&mut self) -> &Option<Result<Token<'source>, ()>> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lexer.next());
        }
        self.peeked.as_ref().unwrap()
    }

    fn consume(&mut self) {
        self.peeked = None
    }
}

impl<'source> Iterator for PeekLexer<'source> {
    type Item = core::result::Result<Token<'source>, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(peeked) = self.peeked.take() {
            peeked
        } else {
            self.lexer.next()
        }
    }
}

#[derive(Clone, Debug)]
enum Value {
    Array(Vec<Value>),
    Rule(HashMap<Identifier, Value>),
    Glob(Vec<String>),
    Path(PathBuf),
    Paths(Vec<PathBuf>),
    String(String),
    Select(Select),
}

#[derive(Clone, Debug)]
struct Select {
    inputs: Vec<String>,
    filters: Vec<(Option<SelectFilter>, Value)>,
}

type SelectFilter = Vec<Option<Vec<String>>>;

#[derive(Clone, Debug, Deserialize, Hash, Eq, PartialEq)]
struct Identifier(String);

impl std::fmt::Display for Identifier {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // Simply display the inner string.
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct CppBinary {
    name: String,
    srcs: Vec<String>,
    srcs2: Vec<PathBuf>,
    srcs3: Vec<String>,
}

impl std::fmt::Display for CppBinary {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // Print a nicely indented CppBinary struct.
        writeln!(f, "CppBinary {{")?;
        writeln!(f, "  name: {},", self.name)?;
        writeln!(f, "  srcs: [{}],", self.srcs.join(", "))?;
        let srcs2: Vec<String> = self.srcs2.iter().map(|p| p.display().to_string()).collect();
        writeln!(f, "  srcs2: [{}],", srcs2.join(", "))?;
        writeln!(f, "  srcs3: [{}]", self.srcs3.join(", "))?;
        write!(f, "}}")
    }
}

impl std::fmt::Display for Select {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // Print the "select" block with its inputs first.
        write!(f, "select(")?;
        write!(f, "(")?;
        for (i, input) in self.inputs.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", input)?;
        }
        write!(f, ")")?;
        
        // Now print the filters and associated value(s):
        write!(f, " => {{ ")?;
        for (i, (filter, value)) in self.filters.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            match filter {
                Some(options) => {
                    write!(f, "(")?;
                    for (j, opt) in options.iter().enumerate() {
                        if j > 0 {
                            write!(f, ", ")?;
                        }
                        match opt {
                            Some(vals) => write!(f, "{}", vals.join(" | "))?,
                            None => write!(f, "_")?,
                        }
                    }
                    write!(f, ")")?;
                }
                None => {
                    write!(f, "default")?;
                }
            }
            write!(f, " = {}", value)?;
        }
        write!(f, " }}")?;
        write!(f, ")")
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // Start pretty-printing with indent level 0.
        self.fmt_pretty(f, 0)
    }
}

impl Value {
    fn fmt_pretty(&self, f: &mut std::fmt::Formatter, indent: usize) -> std::fmt::Result {
        // Use 4 spaces per indent level.
        let indent_str = "    ".repeat(indent);
        let indent_str_next = "    ".repeat(indent + 1);
        match self {
            Value::String(s) => write!(f, "\"{}\"", s),
            Value::Array(arr) => {
                if arr.is_empty() {
                    write!(f, "[]")
                } else {
                    writeln!(f, "[")?;
                    for (i, elem) in arr.iter().enumerate() {
                        write!(f, "{}" , indent_str_next)?;
                        elem.fmt_pretty(f, indent + 1)?;
                        if i < arr.len() - 1 {
                            writeln!(f, ",")?;
                        } else {
                            writeln!(f)?;
                        }
                    }
                    write!(f, "{}]", indent_str)
                }
            }
            Value::Rule(rule) => {
                if rule.is_empty() {
                    write!(f, "{{}}")
                } else {
                    writeln!(f, "{{")?;
                    // Collect entries to iterate with index.
                    let entries: Vec<_> = rule.iter().collect();
                    for (i, (key, value)) in entries.iter().enumerate() {
                        write!(f, "{}{}: ", indent_str_next, key)?;
                        value.fmt_pretty(f, indent + 1)?;
                        if i < entries.len() - 1 {
                            writeln!(f, ",")?;
                        } else {
                            writeln!(f)?;
                        }
                    }
                    write!(f, "{}}}", indent_str)
                }
            }
            Value::Path(path) => write!(f, "{}", path.display()),
            Value::Paths(paths) => {
                if paths.is_empty() {
                    write!(f, "[]")
                } else {
                    writeln!(f, "[")?;
                    for (i, path) in paths.iter().enumerate() {
                        write!(f, "{}{}", indent_str_next, path.display())?;
                        if i < paths.len() - 1 {
                            writeln!(f, ",")?;
                        } else {
                            writeln!(f)?;
                        }
                    }
                    write!(f, "{}]", indent_str)
                }
            }
            Value::Glob(patterns) => {
                if patterns.is_empty() {
                    write!(f, "glob()")
                } else {
                    writeln!(f, "glob(")?;
                    for (i, pat) in patterns.iter().enumerate() {
                        write!(f, "{}{}", indent_str_next, pat)?;
                        if i < patterns.len() - 1 {
                            writeln!(f, ",")?;
                        } else {
                            writeln!(f)?;
                        }
                    }
                    write!(f, "{})", indent_str)
                }
            }
            Value::Select(select) => {
                // Manually pretty-print the select block.
                writeln!(f, "select(")?;
                // Print inputs on a new, indented line.
                write!(f, "{}(", indent_str_next)?;
                for (i, input) in select.inputs.iter().enumerate() {
                    write!(f, "{}", input)?;
                    if i < select.inputs.len() - 1 {
                        write!(f, ", ")?;
                    }
                }
                writeln!(f, ")")?;
                // Print filters
                writeln!(f, "{}=> {{", indent_str_next)?;
                for (i, (filter, value)) in select.filters.iter().enumerate() {
                    write!(f, "{}  ", indent_str_next)?;
                    if let Some(filt) = filter {
                        write!(f, "(")?;
                        for (j, opt) in filt.iter().enumerate() {
                            match opt {
                                Some(vals) => write!(f, "{}", vals.join(" | "))?,
                                None => write!(f, "_")?,
                            }
                            if j < filt.len() - 1 {
                                write!(f, ", ")?;
                            }
                        }
                        write!(f, ")")?;
                    } else {
                        write!(f, "default")?;
                    }
                    write!(f, " = ")?;
                    value.fmt_pretty(f, indent + 2)?;
                    if i < select.filters.len() - 1 {
                        writeln!(f, ",")?;
                    } else {
                        writeln!(f)?;
                    }
                }
                write!(f, "{} }}", indent_str_next)?;
                writeln!(f, ")")
            }
        }
    }
}

fn resolve_value(value: Value, path_root: &Path, vars: &HashMap<String, String>) -> anyhow::Result<Value> {
    match value {
        Value::Array(values) => {
            let new_values = values
                .into_iter()
                .map(|v| resolve_value(v, path_root, vars))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Value::Array(new_values))
        }
        Value::Rule(rule) => {
            let new_rule = rule
                .into_iter()
                .map(|(k, v)| resolve_value(v, path_root, vars).map(|new_value| (k, new_value)))
                .collect::<anyhow::Result<HashMap<Identifier, Value>>>()?;
            Ok(Value::Rule(new_rule))
        }
        Value::Glob(glob_patterns) => {
            // Resolving each glob pattern relative to the given path_root.
            let mut paths = Vec::new();
            for pattern in glob_patterns {
                // Create the full pattern by joining the path root with the provided pattern.
                let full_pattern = path_root.join(&pattern);
                // Convert to a string: this will fail if the path contains non-UTF8 sequences.
                let pattern_str = full_pattern.to_str().ok_or_else(|| {
                    anyhow!("Invalid UTF-8 in glob pattern: {:?}", full_pattern)
                })?;
                // Use the glob crate to resolve the pattern.
                for entry in glob::glob(pattern_str)
                    .with_context(|| format!("Failed to parse glob pattern: {}", pattern_str))?
                {
                    match entry {
                        Ok(path) => paths.push(path),
                        Err(e) => bail!("Error matching glob pattern {}: {:?}", pattern_str, e),
                    }
                }
            }
            Ok(Value::Paths(paths))
        }
        Value::Select(mut s) => {
            let resolved_input: Vec<&String> = s
                .inputs
                .iter()
                .map(|i| {
                    vars.get(i).context(format!(
                        "resolve_value: Failed because select could not find required var [{}]. Vars: [{:?}]",
                        i, &vars
                    ))
                })
                .collect::<anyhow::Result<Vec<&String>>>()?;

            for i in 0..s.filters.len() {
                if let Some(filter) = &s.filters[i].0 {
                    assert_eq!(s.inputs.len(), filter.len());

                    // inputs = (platform, aarch, etc)
                    // filter = (_, foo | bar, baz)
                    // vars = { platform = "windows", aarch = "x64" }
                    // pass if every input is in filter or _
                    let passes = resolved_input
                        .iter()
                        .enumerate()
                        .all(|(idx, input)| match &filter[idx] {
                            Some(valid_values) => valid_values.iter().any(|v| &v == input),
                            None => true,
                        });

                    if passes {
                        let v = s.filters.swap_remove(i).1;
                        return Ok(v);
                    }
                } else {
                    let v = s.filters.swap_remove(i).1;
                    return Ok(v);
                }
            }

            bail!(
                "resolve_value: failed to resolve select. No filters matched.\n  Select: {}\n  Vars: {:?}",
                s,
                vars
            )
        }
        _ => Ok(value),
    }
}

fn parse_config<'src>(lexer: &'src mut Lexer<'src, Token<'src>>) -> anyhow::Result<Value, SpannedError> {
    let mut rules: Vec<Value> = Default::default();

    let mut lexer = PeekLexer {
        lexer: lexer,
        peeked: None,
    };

    // Parse each rule in the config
    let mut comma_ok = false;
    while lexer.peek() != &None {
        match parse_rule(&mut lexer) {
            Ok(Some(rule)) => {
                rules.push(rule);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(SpannedError {
                    error: e,
                    span: lexer.lexer.span(),
                });
            }
        }
    }

    Ok(Value::Array(rules))
}

fn parse_rule<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Option<Value>> {
    if let Some(token) = lexer.next() {
        if let Ok(Token::Identifier(rule_type)) = token {
            // New rule
            let mut rule: HashMap<Identifier, Value> = Default::default();
            rule.insert(
                Identifier("rule_type".to_owned()),
                Value::String(rule_type.to_owned()),
            );
            expect_token(lexer, &Token::ParenOpen)
                .map_err(|e| anyhow!("parse_rule: {}\n Error while parsing rule [{:?}]", e, rule))?;

            // Loop over rule key/values
            loop {
                if consume_token(lexer, &Token::ParenClose) {
                    return Ok(Some(Value::Rule(rule)));
                }

                let ident = expect_identifier(lexer)?;
                expect_token(lexer, &Token::Equals)?;
                let value = parse_value(lexer)?;
                rule.insert(ident, value);
                consume_token(lexer, &Token::Comma);
            }
        } else {
            bail!(
                "parse_rule: Expected identifier token for new rule. Found [{:?}]",
                token
            );
        }
    } else {
        // End of file is fine
        return Ok(None);
    }
}

fn parse_value<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    match lexer.next() {
        Some(Ok(Token::String(s))) => Ok(Value::String(s.to_owned())),
        Some(Ok(Token::Glob)) => parse_glob(lexer),
        Some(Ok(Token::BracketOpen)) => parse_array(lexer),
        Some(Ok(Token::Select)) => parse_select(lexer),
        Some(Ok(t)) => bail!("parse_value: Unexpected token [{:?}]", t),
        v => bail!("parse_value: Unexpected lexer value [{:?}]", v),
    }
}

fn parse_array<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    // assuming that open bracket has been consumed
    let mut values: Vec<Value> = Default::default();

    loop {
        if consume_token(lexer, &Token::BracketClose) {
            break;
        }

        let v = parse_value(lexer)?;
        values.push(v);
        consume_token(lexer, &Token::Comma);
    }

    Ok(Value::Array(values))
}

fn parse_glob<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    // assuming that glob token has been consumed
    expect_token(lexer, &Token::ParenOpen)?;
    expect_token(lexer, &Token::BracketOpen)?;

    let mut paths = Vec::<String>::default();

    loop {
        if lexer.peek() == &Some(Ok(Token::BracketClose)) {
            break;
        }

        match lexer.next() {
            Some(Ok(Token::String(s))) => paths.push(s.into()),
            t => bail!("parse_glob: Unexpected token [{:?}]", t),
        }

        consume_token(lexer, &Token::Comma);
    }

    expect_token(lexer, &Token::BracketClose)?;
    expect_token(lexer, &Token::ParenClose)?;

    Ok(Value::Glob(paths))
}

fn parse_select<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::ParenOpen)?;

    // Read inputs
    let mut inputs = Vec::<String>::default();
    expect_token(lexer, &Token::ParenOpen)?;
    loop {
        if consume_token(lexer, &Token::ParenClose) {
            break;
        }

        match lexer.next() {
            Some(Ok(Token::Identifier(i))) => inputs.push(i.into()),
            t => bail!("parse_select: Unexpected token [{:?}]", t),
        }
        consume_token(lexer, &Token::Comma);
    }

    // verify inputs are unique
    let mut seen = std::collections::HashSet::new();
    for input in &inputs {
        if !seen.insert(input) {
            bail!("parse_select: duplicate input found: {}", input);
        }
    }

    expect_token(lexer, &Token::Arrow)?;

    // Read filters
    let mut filters = Vec::<(Option<SelectFilter>, Value)>::default();
    expect_token(lexer, &Token::BraceOpen)?;
    loop {
        if consume_token(lexer, &Token::BraceClose) {
            break;
        }

        // read filter
        let mut maybe_select_filter: Option<Vec<Option<Vec<String>>>> = None;

        if consume_token(lexer, &Token::Default) {
            // no filter to parse, but there will be a value parsed below
        } else {
            expect_token(lexer, &Token::ParenOpen)?;

            let mut select_filter: Vec<Option<Vec<String>>> = SelectFilter::default();
            loop {
                if consume_token(lexer, &Token::ParenClose) {
                    break;
                }

                match lexer.next() {
                    Some(Ok(Token::Underscore)) => select_filter.push(None),
                    Some(Ok(Token::Identifier(i))) => {
                        let mut values: Vec<String> = Default::default();
                        values.push(i.to_owned());

                        while consume_token(lexer, &Token::Pipe) {
                            match lexer.next() {
                                Some(Ok(Token::Identifier(i))) => values.push(i.to_owned()),
                                Some(Ok(t)) => bail!("parse_select: Unexpected token [{:?}]", t),
                                v => bail!("parse_select: Unexpected value [{:?}]", v),
                            }
                            consume_token(lexer, &Token::Comma);
                        }

                        select_filter.push(Some(values));
                    }
                    Some(Ok(t)) => bail!("parse_select: Unexpected token [{:?}]", t),
                    v => bail!("parse_select: Unexpected value [{:?}]", v),
                }
                consume_token(lexer, &Token::Comma);
            }

            if select_filter.len() != inputs.len() {
                bail!("parse_select: Num inputs ({}) and num filters ({}) length must match. \nInputs: {:?}  \nFilter: {:?}", 
                    inputs.len(),
                    select_filter.len(),
                    inputs,
                    select_filter)
            }

            maybe_select_filter = Some(select_filter);
        }

        expect_token(lexer, &Token::Equals)?;

        let value = parse_value(lexer)?;
        filters.push((maybe_select_filter, value));

        consume_token(lexer, &Token::Comma);
    }

    // close it out
    expect_token(lexer, &Token::ParenClose)?;
    consume_token(lexer, &Token::Comma);

    Ok(Value::Select(Select { inputs, filters }))
}

fn expect_token<'src>(lexer: &mut PeekLexer<'src>, expected_token: &Token<'src>) -> ParseResult<()> {
    match lexer.next() {
        Some(Ok(token)) => {
            if &token == expected_token {
                Ok(())
            } else {
                bail!(
                    "expect_token: Token [{:?}] did not match expected token [{:?}]",
                    token,
                    expected_token
                );
            }
        }
        e => bail!(
            "expect_token: Expected token [{:?}] but found [{:?}]",
            expected_token,
            e
        ),
    }
}

fn consume_token<'src>(lexer: &mut PeekLexer<'src>, token: &Token<'src>) -> bool {
    if let Some(Ok(t)) = lexer.peek() {
        if t == token {
            lexer.consume();
            return true;
        }
    }

    false
}

fn expect_identifier<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Identifier> {
    let token = lexer.next();
    match token {
        Some(Ok(Token::Identifier(i))) => Ok(Identifier(i.to_owned())),
        Some(Ok(t)) => bail!("expect_identifier: Unexpected token [{:?}]", t),
        _ => bail!("expect_identifier: Unexpected end of stream"),
    }
}

fn main() -> anyhow::Result<()> {
    let filename = "C:/source_control/anubis/examples/simple_cpp/ANUBIS";
    let src = fs::read_to_string(&filename)?;

    let mut lexer = Token::lexer(&src);
    let result = parse_config(&mut lexer);

    match result {
        Ok(config) => {
            println!("{:?}", config);

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
                        let de = ValueDeserializer::new(v);
                        CppBinary::deserialize(de)
                    })
                    .collect::<Result<Vec<CppBinary>, _>>()?,
                _ => bail!("Expected config root to be an array"),
            };

            for rule in &rules {
                println!("{}", rule);
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
