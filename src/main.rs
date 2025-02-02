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

    #[regex(r#""([^"\\]|\\["\\bnfrt]|u[a-fA-F0-9]{4})*""#, |lex| lex.slice())]
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
    Glob(Vec<PathBuf>),
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

#[derive(Clone, Debug, Deserialize)]
struct CppBinary {
    name: String,
    srcs: Vec<String>,
    srcs2: Vec<PathBuf>,
}

fn resolve_value(value: Value, path_root: &Path) -> anyhow::Result<Value> {
    match value {
        Value::Array(values) => {
            let new_values = values
                .into_iter()
                .map(|v| resolve_value(v, path_root))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Value::Array(new_values))
        }
        Value::Rule(rule) => {
            let new_rule = rule
                .into_iter()
                .map(|(k, v)| resolve_value(v, path_root).map(|new_value| (k, new_value)))
                .collect::<anyhow::Result<HashMap<Identifier, Value>>>()?;
            Ok(Value::Rule(new_rule))
        }
        Value::Glob(glob) => {
            // TODO: actually resolve
            let paths = glob.into_iter().map(|path| path).collect();
            Ok(Value::Paths(paths))
        }
        Value::Select(s) => {
            // TODO: flatten
            Ok(Value::String("WAS_SELECT".to_owned()))
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

    let mut paths = Vec::<PathBuf>::default();

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
            let resolve_root = PathBuf::default();
            let resolve_vars: HashMap<String, String> = [("platform", "windows"), ("arch", "x64")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();

            let config = resolve_value(config, &resolve_root)?;

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

            println!("Rules: {:?}", rules);
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
