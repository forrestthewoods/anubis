#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]

use logos::{Lexer, Logos, Span};
use toolshed::Arena;

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
type ArenaList<'a, T> = toolshed::list::List<'a, T>;
type ArenaMap<'a, K,V> = toolshed::map::Map<'a, K,V>;


#[derive(Copy, Debug)]
enum Value<'arena> {
    Array(ArenaList<'arena, Value<'arena>>),
    Rule(Rule<'arena>),
    Root(ArenaList<'arena, Rule<'arena>>),
    Glob(ArenaList<'arena, &'arena str>),
}

#[derive(Debug, Hash)]
struct Identifier<'arena>(&'arena str);

#[derive(Debug)]
struct Rule<'arena>(ArenaMap<'arena, Identifier<'arena>, Value<'arena>>);

#[derive(Debug)]
struct AnubisConfig<'arena> {
    rules: ArenaList<'arena, &'arena Rule<'arena>>,
}

impl<'arena> AnubisConfig<'arena> {
    fn new() -> Self {
        AnubisConfig {
            rules: toolshed::list::List::empty()
        }
    }
}


fn parse_config<'arena>(
    arena: &'arena Arena,
    lexer: &mut Lexer<'arena, Token<'arena>>,
) -> ParseResult<AnubisConfig<'arena>> {
    let config = AnubisConfig::new();

    while !lexer.remainder().is_empty() {
        let rule = parse_rule(arena, lexer)?;
        config.rules.prepend(arena, rule);
    }

    Ok(config)
}

fn parse_rule<'arena>(
    arena: &Arena,
    lexer: &mut Lexer<'arena, Token<'arena>>,
) -> ParseResult<&'arena Rule<'arena>> {

    if let Some(token) = lexer.next() {
        if let Ok(Token::Identifier(ident)) = token {
            expect_token(lexer, Token::ParenOpen)?;
        } else {
            return Err((format!("Unexpected token [{:?}]", token), lexer.span()));
        }
    } else {
        return Err(("Unexpected end of stream".to_owned(), lexer.span()));
    }

    Err(("oh no".to_owned(), lexer.span()))
}

fn expect_token<'arena>(
    lexer: &mut Lexer<'arena, Token<'arena>>,
    expected_token: Token<'arena>,
) -> ParseResult<()> {
    if let Some(Ok(token)) = lexer.next() {
        if token == expected_token {
            Ok(())
        } else {
            return Err((format!("Token [{:?}] did not match expected token [{:?}]", token, expected_token), lexer.span()));
        }
    } else {
        return Err(("Unexpected end of stream".to_owned(), lexer.span()));
    }
}

fn main() -> anyhow::Result<()> {
    let arena = Arena::new();

    let filename = "C:/source_control/anubis/examples/simple_cpp/ANUBIS";
    let src = fs::read_to_string(&filename).and_then(|s| Ok(arena.alloc_string(s)))?;

    for token in Token::lexer(src) {
        println!("{:?}", token);
    }

    match parse_config(&arena, &mut Token::lexer(src)) {
        Ok(value) => println!("{:#?}", value),
        Err((msg, span)) => {
            use ariadne::{ColorGenerator, Label, Report, ReportKind, Source};

            let mut colors = ColorGenerator::new();

            let a = colors.next();

            Report::build(ReportKind::Error, &filename, 12)
                .with_message("Invalid ANUBIS".to_string())
                .with_label(
                    Label::new((&filename, span))
                        .with_message(msg)
                        .with_color(a),
                )
                .finish()
                .eprint((&filename, Source::from(src)))
                .unwrap();
        }
    }

    Ok(())
}
