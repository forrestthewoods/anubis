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

use crate::serde_impl::ValueDeserializer;

#[derive(Debug, Logos, PartialEq)]
#[logos(skip r"[ \t\r\n\f]+")]
pub enum Token<'source> {
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

    #[token("+")]
    Plus,

    #[regex(r"-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?", |lex| {
        lex.slice().parse::<f64>().unwrap()
    })]
    Number(f64),

    #[token("(")]
    ParenOpen,

    #[token(")")]
    ParenClose,

    #[regex(r#"[a-zA-Z_][a-zA-Z0-9_\-\.]*"#, |lex| lex.slice())]
    Identifier(&'source str),

    #[regex(r#""([^"\\]|\\["\\bnfrt]|u[a-fA-F0-9]{4})*""#, |lex| {
        // Trim the surrounding quotes.
        let s = lex.slice();
        &s[1..s.len()-1]
    })]
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

pub type ParseResult<T> = anyhow::Result<T>;

#[derive(Debug)]
pub struct SpannedError {
    pub error: anyhow::Error,
    pub span: Span,
}

pub struct PeekLexer<'source> {
    pub lexer: &'source mut Lexer<'source, Token<'source>>,
    pub peeked: Option<Option<Result<Token<'source>, ()>>>,
}

impl<'source> PeekLexer<'source> {
    pub fn peek(&mut self) -> &Option<Result<Token<'source>, ()>> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lexer.next());
        }
        self.peeked.as_ref().unwrap()
    }

    pub fn consume(&mut self) {
        self.peeked = None;
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
pub enum Value {
    Array(Vec<Value>),
    Rule(HashMap<Identifier, Value>),
    Glob(Vec<String>),
    Path(PathBuf),
    Paths(Vec<PathBuf>),
    String(String),
    Select(Select),
    Concat((Box<Value>, Box<Value>)),
}

#[derive(Clone, Debug)]
pub struct Select {
    pub inputs: Vec<String>,
    pub filters: Vec<(Option<SelectFilter>, Value)>,
}

pub type SelectFilter = Vec<Option<Vec<String>>>;

#[derive(Clone, Debug, Deserialize, Hash, Eq, PartialEq)]
pub struct Identifier(pub String);

#[derive(Clone, Debug, Deserialize)]
pub struct CppBinary {
    pub name: String,
    pub srcs: Vec<String>,
    pub srcs2: Vec<PathBuf>,
    pub srcs3: Vec<String>,
    pub srcs4: Vec<String>,
}

pub fn resolve_value(
    value: Value,
    path_root: &Path,
    vars: &HashMap<String, String>,
) -> anyhow::Result<Value> {
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
            let mut paths = Vec::new();
            for pattern in glob_patterns {
                let full_pattern = path_root.join(&pattern);
                let pattern_str = full_pattern
                    .to_str()
                    .ok_or_else(|| anyhow!("Invalid UTF-8 in glob pattern: {:?}", full_pattern))?;
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
                        "resolve_value: Failed because select could not find required var [{}]. Vars: {:?}",
                        i, &vars
                    ))
                })
                .collect::<anyhow::Result<Vec<&String>>>()?;
            for i in 0..s.filters.len() {
                if let Some(filter) = &s.filters[i].0 {
                    assert_eq!(s.inputs.len(), filter.len());
                    let passes = resolved_input
                        .iter()
                        .enumerate()
                        .all(|(idx, input)| match &filter[idx] {
                            Some(valid_values) => valid_values.iter().any(|v| v == *input),
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
                "resolve_value: failed to resolve select. No filters matched.\n  Select: {:?}\n  Vars: {:?}",
                s,
                vars
            );
        }
        Value::Concat(pair) => {
            let mut left = resolve_value(*pair.0, path_root, vars)?;
            let mut right = resolve_value(*pair.1, path_root, vars)?;
            match (&mut left, &mut right) {
                (Value::Array(l), Value::Array(r)) => {
                    l.append(r);
                    return Ok(Value::Array(std::mem::take(l)));
                }
                _ => {
                    bail!(
                        "resolve_value: Cannot concatenate non-arrays.\n  Left: {:?}\n  Right: {:?}",
                        left,
                        right
                    )
                }
            }
        }
        _ => Ok(value),
    }
}

pub fn parse_config<'src>(lexer: &'src mut Lexer<'src, Token<'src>>) -> anyhow::Result<Value, SpannedError> {
    let mut rules: Vec<Value> = Default::default();
    let mut lexer = PeekLexer { lexer, peeked: None };
    while lexer.peek() != &None {
        match parse_rule(&mut lexer) {
            Ok(Some(rule)) => rules.push(rule),
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

pub fn parse_rule<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Option<Value>> {
    if let Some(token) = lexer.next() {
        if let Ok(Token::Identifier(rule_type)) = token {
            let mut rule: HashMap<Identifier, Value> = Default::default();
            rule.insert(
                Identifier("rule_type".to_owned()),
                Value::String(rule_type.to_owned()),
            );
            expect_token(lexer, &Token::ParenOpen)
                .map_err(|e| anyhow!("parse_rule: {}\n Error while parsing rule [{:?}]", e, rule))?;
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
        Ok(None)
    }
}

pub fn parse_value<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    let v = match lexer.next() {
        Some(Ok(Token::String(s))) => Ok(Value::String(s.to_owned())),
        Some(Ok(Token::Glob)) => parse_glob(lexer),
        Some(Ok(Token::BracketOpen)) => parse_array(lexer),
        Some(Ok(Token::Select)) => parse_select(lexer),
        Some(Ok(t)) => bail!("parse_value: Unexpected token [{:?}]", t),
        v => bail!("parse_value: Unexpected lexer value [{:?}]", v),
    }?;
    if consume_token(lexer, &Token::Plus) {
        let left = Box::new(v);
        let right = Box::new(parse_value(lexer)?);
        Ok(Value::Concat((left, right)))
    } else {
        Ok(v)
    }
}

pub fn parse_array<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
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

pub fn parse_glob<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
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

pub fn parse_select<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::ParenOpen)?;
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
    let mut seen = std::collections::HashSet::new();
    for input in &inputs {
        if !seen.insert(input) {
            bail!("parse_select: duplicate input found: {}", input);
        }
    }
    expect_token(lexer, &Token::Arrow)?;
    let mut filters = Vec::<(Option<SelectFilter>, Value)>::default();
    expect_token(lexer, &Token::BraceOpen)?;
    loop {
        if consume_token(lexer, &Token::BraceClose) {
            break;
        }
        let mut maybe_select_filter: Option<Vec<Option<Vec<String>>>> = None;
        if consume_token(lexer, &Token::Default) {
            // no filter to parse
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
                bail!("parse_select: Num inputs ({}) and num filters ({}) length must match.\nInputs: {:?}\nFilter: {:?}", 
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
    expect_token(lexer, &Token::ParenClose)?;
    consume_token(lexer, &Token::Comma);
    Ok(Value::Select(Select { inputs, filters }))
}

pub fn expect_token<'src>(lexer: &mut PeekLexer<'src>, expected_token: &Token<'src>) -> ParseResult<()> {
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

pub fn consume_token<'src>(lexer: &mut PeekLexer<'src>, token: &Token<'src>) -> bool {
    if let Some(Ok(t)) = lexer.peek() {
        if t == token {
            lexer.consume();
            return true;
        }
    }
    false
}

pub fn expect_identifier<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Identifier> {
    let token = lexer.next();
    match token {
        Some(Ok(Token::Identifier(i))) => Ok(Identifier(i.to_owned())),
        Some(Ok(t)) => bail!("expect_identifier: Unexpected token [{:?}]", t),
        t => bail!("expect_identifier: Unexpected result [{:?}]", t),
    }
}
