#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]

use logos::{Lexer, Logos, Span};

use std::collections::HashMap;
use std::fs;

type Error = (String, Span);
type Result<T> = std::result::Result<T, Error>;

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

    #[token("null")]
    Null,

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
    // TODO:
    // comment
}

type ParseError = (String, Span);
type ParseResult<T> = std::result::Result<T, ParseError>;
type LexerPeekIter<'arena> = std::iter::Peekable<&'arena mut Lexer<'arena, Token<'arena>>>;

#[derive(Debug)]
enum Value {
    Array(Vec<Value>),
    Rule(Rule),
    Root(Vec<Rule>),
    Glob(Vec<String>),
    String(String),
}

#[derive(Debug, Hash)]
struct Identifier(String);

#[derive(Debug)]
struct Rule(HashMap<Identifier, Value>);

#[derive(Default)]
struct AnubisConfig {
    rules: Vec<Rule>,
}

impl AnubisConfig {
    fn new() -> Self {
        AnubisConfig {
            rules: Default::default(),
        }
    }
}

fn parse_config<'arena>(lexer: &mut Lexer<'arena, Token<'arena>>) -> ParseResult<AnubisConfig> {
    let mut config = AnubisConfig::new();

    // Parse each rule in the config
    loop {
        match parse_rule(lexer) {
            Ok(Some(rule)) => config.rules.push(rule),
            Ok(None) => break,
            Err(e) => return Err(e),
        }
    }

    Ok(config)
}

fn parse_rule<'arena>(lexer: &mut Lexer<'arena, Token<'arena>>) -> ParseResult<Option<Rule>> {
    if let Some(token) = lexer.next() {
        if let Ok(Token::Identifier(ident)) = token {
            expect_token(lexer, Token::ParenOpen)?;
            let ident = expect_identifier(lexer)?;
            expect_token(lexer, Token::Equals)?;
            let value = parse_value(lexer)?;
        } else {
            return Err((format!("Unexpected token [{:?}]", token), lexer.span()));
        }
    } else {
        return Ok(None);
    }

    Err(("Unexpected error parsing Rule".to_owned(), lexer.span()))
}

fn parse_value<'arena>(lexer: &mut Lexer<'arena, Token<'arena>>) -> ParseResult<Value> {

    match lexer.next() {
        Some(Ok(Token::String(s))) => Ok(Value::String(s.to_owned())),
        t => Err((format!("Unexpected token [{:?}]", t), lexer.span()))
    }
}


fn expect_token<'arena>(
    lexer: &mut Lexer<'arena, Token<'arena>>,
    expected_token: Token<'arena>,
) -> ParseResult<()> {
    match lexer.next() {
        Some(Ok(token)) => {
            if token == expected_token {
                Ok(())
            } else {
                Err((
                    format!(
                        "Token [{:?}] did not match expected token [{:?}]",
                        token, expected_token
                    ),
                    lexer.span(),
                ))
            }
        }
        _ => Err(("unepxected error".to_owned(), lexer.span())),
    }
}

fn expect_identifier<'arena>(
    lexer: &mut Lexer<'arena, Token<'arena>>
) -> ParseResult<Token<'arena>>  {

    let token = lexer.next();
    match token {
        Some(Ok(Token::Identifier(i))) => Ok(Token::Identifier(i)),
        Some(Ok(t)) => Err((format!("Unexpected token [{:?}]", t), lexer.span())),
        _ => Err((format!("Unexpected end of stream"), lexer.span()))
    }
}

fn main() -> anyhow::Result<()> {
    let filename = "C:/source_control/anubis/examples/simple_cpp/ANUBIS";
    let src = fs::read_to_string(&filename)?;

    for token in Token::lexer(&src) {
        println!("{:?}", token);
    }

    match parse_config(&mut Token::lexer(&src)) {
        Ok(value) => {}
        Err((msg, span)) => {
            use ariadne::{ColorGenerator, Label, Report, ReportKind, Source};

            let mut colors = ColorGenerator::new();

            let a = colors.next();

            Report::build(ReportKind::Error, &filename, 12)
                .with_message("Invalid ANUBIS".to_string())
                .with_label(Label::new((&filename, span)).with_message(msg).with_color(a))
                .finish()
                .eprint((&filename, Source::from(src)))
                .unwrap();
        }
    }

    Ok(())
}
