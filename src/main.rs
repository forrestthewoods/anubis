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

#[derive(Debug)]
struct SpannedError {
    error: anyhow::Error,
    span: Span,  // Or whatever your span type is
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
    Rule(Rule),
    Root(Vec<Rule>),
    Glob(Vec<String>),
    String(String),
}

#[derive(Debug, Hash, Eq, PartialEq)]
struct Identifier(String);

#[derive(Debug, Default)]
struct Rule(HashMap<Identifier, Value>);

#[derive(Debug, Default)]
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

fn parse_config<'src>(lexer: &'src mut Lexer<'src, Token<'src>>) -> anyhow::Result<AnubisConfig, SpannedError> {
    let mut config = AnubisConfig::new();

    let mut lexer = PeekLexer {
        lexer: lexer,
        peeked: None
    };

    // Parse each rule in the config
    let mut comma_ok = false;
    while lexer.peek() != &None {
        match parse_rule(&mut lexer) {
            Ok(Some(rule)) => {
                config.rules.push(rule);
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

    Ok(config)
}

fn parse_rule<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Option<Rule>> {
    if let Some(token) = lexer.next() {
        if let Ok(Token::Identifier(rule_type)) = token {
            // New rule
            let mut rule: Rule = Default::default();
            rule.0.insert(
                Identifier("rule_type".to_owned()),
                Value::String(rule_type.to_owned()),
            );
            expect_token(lexer, &Token::ParenOpen)?;

            // Loop over rule key/values
            loop  {
                if consume_token(lexer, &Token::ParenClose) {
                    println!("Returning a rule");
                    return Ok(Some(rule));
                }

                let ident = expect_identifier(lexer)?;
                expect_token(lexer, &Token::Equals)?;
                let value = parse_value(lexer)?;
                rule.0.insert(ident, value);
                consume_token(lexer, &Token::Comma);
            }
        } else {
            bail!("Expected identifier token for new rule. Found [{:?}]", token);
        }
    } else {
        // End of file is fine
        return Ok(None);
    }
}

fn parse_value<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    match lexer.next() {
        Some(Ok(Token::String(s))) => Ok(Value::String(s.to_owned())),
        Some(Ok(Token::BracketOpen)) => {
            let mut values : Vec<Value> = Default::default();

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
        t => bail!("Unexpected token [{:?}]", t),
    }
}

fn expect_token<'src>(
    lexer: &mut PeekLexer<'src>,
    expected_token: &Token<'src>,
) -> ParseResult<()> {
    match lexer.next() {
        Some(Ok(token)) => {
            if &token == expected_token {
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
    let result = { 
        parse_config(&mut lexer)
    };

    match result {
        Ok(config) => {
            println!("{:?}", config);
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
