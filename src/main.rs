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
    // TODO:
    // comment
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

#[derive(Debug)]
enum Value {
    Array(Vec<Value>),
    Rule(HashMap<Identifier, Value>),
    Glob(Vec<PathBuf>),
    //Path(PathBuf), // TODO:
    String(String),
}

#[derive(Debug, Deserialize, Hash, Eq, PartialEq)]
struct Identifier(String);

#[derive(Clone, Debug, Deserialize)]
struct CppBinary {
    name: String,
    srcs: Vec<String>,
}

fn resolve_value(value: &mut Value, path_root: &Path) {}

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
        Some(Ok(Token::BracketOpen)) => {
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
        t => bail!("parse_value: Unexpected token [{:?}]", t),
    }
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

    for token in Token::lexer(&src) {
        println!("{:?}", token);
    }

    let mut lexer = Token::lexer(&src);
    let result = { parse_config(&mut lexer) };

    match result {
        Ok(config) => {
            println!("{:?}", config);

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

// ---------------------------------------------------
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::forward_to_deserialize_any;
use serde::Deserialize;
use std::fmt;

// First, implement an error type
#[derive(Debug)]
enum DeserializeError {
    ExpectedArray,
    ExpectedMap,
    ExpectedString,
    Unresolved(String),
    Custom(String),
}

impl de::Error for DeserializeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DeserializeError::Custom(msg.to_string())
    }
}

impl fmt::Display for DeserializeError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DeserializeError::ExpectedArray => write!(f, "expected array"),
            DeserializeError::ExpectedMap => write!(f, "expected map"),
            DeserializeError::ExpectedString => write!(f, "expected string"),
            DeserializeError::Unresolved(msg) => write!(f, "unresolved value: {}", msg),
            DeserializeError::Custom(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for DeserializeError {}

// Create a deserializer wrapper
struct ValueDeserializer {
    value: Value,
}

impl ValueDeserializer {
    fn new(value: Value) -> Self {
        ValueDeserializer { value }
    }
}

impl<'de> Deserializer<'de> for ValueDeserializer {
    type Error = DeserializeError;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.value {
            Value::String(s) => visitor.visit_string(s),
            Value::Array(arr) => visitor.visit_seq(ArrayDeserializer {
                iter: arr.into_iter(),
            }),
            Value::Rule(rule) => visitor.visit_map(RuleDeserializer::new(rule)),
            Value::Glob(g) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize blobs. Must resolve before deserialize: glob{:?}",
                g
            ))),
        }
    }

    // Implement specific type deserializers
    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.value {
            Value::String(s) => visitor.visit_string(s),
            _ => Err(DeserializeError::ExpectedString),
        }
    }

    // Add other deserialize_* methods, forwarding to deserialize_any where appropriate

    // We'll need to implement at least:
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

// Helper struct for deserializing arrays
struct ArrayDeserializer {
    iter: std::vec::IntoIter<Value>,
}

impl<'de> SeqAccess<'de> for ArrayDeserializer {
    type Error = DeserializeError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        match self.iter.next() {
            Some(value) => {
                let deserializer = ValueDeserializer::new(value);
                seed.deserialize(deserializer).map(Some)
            }
            None => Ok(None),
        }
    }
}

// Helper struct for deserializing rules
struct RuleDeserializer {
    iter: std::collections::hash_map::IntoIter<Identifier, Value>,
    next_value: Option<Value>,
}

impl RuleDeserializer {
    fn new(map: HashMap<Identifier, Value>) -> Self {
        RuleDeserializer {
            iter: map.into_iter(),
            next_value: None,
        }
    }
}

impl<'de> MapAccess<'de> for RuleDeserializer {
    type Error = DeserializeError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: de::DeserializeSeed<'de>,
    {
        match self.iter.next() {
            Some((key, value)) => {
                self.next_value = Some(value);
                let key_deserializer = ValueDeserializer::new(Value::String(key.0));
                seed.deserialize(key_deserializer).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        match self.next_value.take() {
            Some(value) => {
                let value_deserializer = ValueDeserializer::new(value);
                seed.deserialize(value_deserializer)
            }
            None => Err(DeserializeError::Custom("value missing".to_string())),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = Value;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string, array, or rule")
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(Value::String(value))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    values.push(value);
                }
                Ok(Value::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut rule: HashMap<Identifier, Value> = Default::default();
                while let Some((key, value)) = map.next_entry()? {
                    rule.insert(key, value);
                }
                Ok(Value::Rule(rule))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}
