#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use anyhow::bail;
use logos::{Lexer, Logos, Span};

use std::collections::HashMap;
use std::fs;

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

type ParseResult<T> = anyhow::Result<T>;

struct PeekLexer<'source> {
    lexer: Lexer<'source, Token<'source>>,
    peeked: Option<Option<Result<Token<'source>, ()>>>,
}

impl<'source> PeekLexer<'source> {
    fn new(source: &'source str) -> Self {
        Self {
            lexer: Token::lexer(source),
            peeked: None,
        }
    }

    fn peek(&mut self) -> &Option<Result<Token<'source>, ()>> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lexer.next());
        }
        self.peeked.as_ref().unwrap()
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
    Rule(Rule),
    Root(Vec<Rule>),
    Glob(Vec<String>),
    String(String),
}

#[derive(Debug, Hash, Eq, PartialEq)]
struct Identifier(String);

#[derive(Debug, Default)]
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
    let mut comma_ok = false;
    loop {
        // match lexer.next() {
        //     Some(Ok(Token::ParenClose)) => break,
        //     Some(Ok(t)) => {
        //         bail!("Unexpected token [{:?}]", t);
        //     }
        //     Some(Err(e)) => { bail!("Unexpected error [{:?}]", e); }
        //     None => {
        //         bail!("Unexpected end of config");
        //     }
        // }

        match parse_rule(lexer) {
            Ok(Some(rule)) => {
                config.rules.push(rule);
            }
            Ok(None) => break,
            Err(e) => return Err(e),
        }
    }

    Ok(config)
}

fn parse_rule<'arena>(lexer: &mut Lexer<'arena, Token<'arena>>) -> ParseResult<Option<Rule>> {
    if let Some(token) = lexer.next() {
        if let Ok(Token::Identifier(rule_type)) = token {
            // New rule
            let mut rule: Rule = Default::default();
            rule.0.insert(
                Identifier("rule_type".to_owned()),
                Value::String(rule_type.to_owned()),
            );
            expect_token(lexer, Token::ParenOpen)?;

            // Loop over rule key/values
            let mut comma_ok = false;
            loop  {

                let ident = expect_identifier(lexer)?;
                expect_token(lexer, Token::Equals)?;
                let value = parse_value(lexer)?;
                rule.0.insert(ident, value);
            }
        } else {
            bail!("Expected identifier token for new rule. Found [{:?}]", token);
        }
    } else {
        // End of file is fine
        return Ok(None);
    }
}

fn parse_value<'arena>(lexer: &mut Lexer<'arena, Token<'arena>>) -> ParseResult<Value> {
    match lexer.next() {
        Some(Ok(Token::String(s))) => Ok(Value::String(s.to_owned())),
        t => bail!("Unexpected token [{:?}]", t),
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
                bail!(
                    "Token [{:?}] did not match expected token [{:?}]",
                    token,
                    expected_token
                );
            }
        }
        _ => bail!("unepxected error"),
    }
}

fn expect_identifier<'arena>(lexer: &mut Lexer<'arena, Token<'arena>>) -> ParseResult<Identifier> {
    let token = lexer.next();
    match token {
        Some(Ok(Token::Identifier(i))) => Ok(Identifier(i.to_owned())),
        Some(Ok(t)) => bail!("Unexpected token [{:?}]", t),
        _ => bail!("Unexpected end of stream"),
    }
}

fn main() -> anyhow::Result<()> {
    let filename = "C:/source_control/anubis/examples/simple_cpp/ANUBIS";
    let src = fs::read_to_string(&filename)?;

    for token in Token::lexer(&src) {
        println!("{:?}", token);
    }

    let mut lexer = Token::lexer(&src);
    match parse_config(&mut lexer) {
        Ok(value) => {}
        Err(e) => {
            use ariadne::{ColorGenerator, Label, Report, ReportKind, Source};

            let mut colors = ColorGenerator::new();

            let a = colors.next();

            Report::build(ReportKind::Error, &filename, 12)
                .with_message("Invalid ANUBIS".to_string())
                .with_label(
                    Label::new((&filename, lexer.span()))
                        .with_message(e)
                        .with_color(a),
                )
                .finish()
                .eprint((&filename, Source::from(src)))
                .unwrap();
        }
    }

    Ok(())
}
