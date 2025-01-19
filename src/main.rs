#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]

use logos::{Lexer, Logos, Span};
use toolshed::Arena;

use std::collections::HashMap;
use std::fs;

type Error = (String, Span);
type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Logos)]
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
}

#[derive(Clone, Debug)]
enum Value<'arena> {
    Array(Vec<Value<'arena>>),
    Rule(Rule<'arena>),
    Root(Vec<Rule<'arena>>),
    Glob(Vec<&'arena str>),
}

#[derive(Clone, Debug, Hash)]
struct Identifier<'arena>(&'arena str);

#[derive(Clone, Debug)]
struct Rule<'arena>(HashMap<Identifier<'arena>, Value<'arena>>);

#[derive(Clone, Debug)]
struct AnubisConfig<'arena> {
    rules: Vec<Rule<'arena>>,
}

fn parse_config<'arena>(lexer: &mut Lexer<'arena, Token<'arena>>) -> anyhow::Result<AnubisConfig<'arena>> {

    anyhow::bail!("oh no");
}

fn parse_rule<'arena>(lexer: &mut Lexer<'arena, Token<'arena>>) -> anyhow::Result<Rule<'arena>> {
    if let Some(token) = lexer.next() {
        if let Ok(Token::Identifier(ident)) = token {
        } else {
            anyhow::bail!("Unexpected token: [{:?}]", lexer.span());
        }
    } else {
       anyhow::bail!("Failed to parse rule because lexer was empty? {:?}", lexer.span());
    }

    anyhow::bail!("oh no");
}

/*
#[derive(Debug)]
enum Value {
    /// null.
    Null,
    /// true or false.
    Bool(bool),
    /// Any floating point number.
    Number(f64),
    /// Any quoted string.
    String(String),
    /// An array of values
    Array(Vec<Value>),
    /// An dictionary mapping keys and values.
    Object(HashMap<String, Value>),
}


fn parse_value<'source>(lexer: &mut Lexer<'source, Token<'source>>) -> Result<Value<'source>> {
    if let Some(token) = lexer.next() {
        match token {
            Ok(Token::Bool(b)) => Ok(Value::Bool(b)),
            Ok(Token::BraceOpen) => parse_object(lexer),
            Ok(Token::BracketOpen) => parse_array(lexer),
            Ok(Token::Null) => Ok(Value::Null),
            Ok(Token::Number(n)) => Ok(Value::Number(n)),
            Ok(Token::String(s)) => Ok(Value::String(s)),
            _ => Err((
                "unexpected token here (context: value)".to_owned(),
                lexer.span(),
            )),
        }
    } else {
        Err(("empty values are not allowed".to_owned(), lexer.span()))
    }
}

fn parse_array<'source>(lexer: &mut Lexer<'source, Token<'source>>) -> Result<Value<'source>> {
    let mut array = Vec::new();
    let span = lexer.span();
    let mut awaits_comma = false;
    let mut awaits_value = false;

    while let Some(token) = lexer.next() {
        match token {
            Ok(Token::Bool(b)) if !awaits_comma => {
                array.push(Value::Bool(b));
                awaits_value = false;
            }
            Ok(Token::BraceOpen) if !awaits_comma => {
                let object = parse_object(lexer)?;
                array.push(object);
                awaits_value = false;
            }
            Ok(Token::BracketOpen) if !awaits_comma => {
                let sub_array = parse_array(lexer)?;
                array.push(sub_array);
                awaits_value = false;
            }
            Ok(Token::BracketClose) if !awaits_value => return Ok(Value::Array(array)),
            Ok(Token::Comma) if awaits_comma => awaits_value = true,
            Ok(Token::Null) if !awaits_comma => {
                array.push(Value::Null);
                awaits_value = false
            }
            Ok(Token::Number(n)) if !awaits_comma => {
                array.push(Value::Number(n));
                awaits_value = false;
            }
            Ok(Token::String(s)) if !awaits_comma => {
                array.push(Value::String(s));
                awaits_value = false;
            }
            _ => {
                return Err((
                    "unexpected token here (context: array)".to_owned(),
                    lexer.span(),
                ))
            }
        }
        awaits_comma = !awaits_value;
    }
    Err(("unmatched opening bracket defined here".to_owned(), span))
}

fn parse_object<'source>(lexer: &mut Lexer<'source, Token<'source>>) -> Result<Value<'source>> {
    let mut map = HashMap::new();
    let span = lexer.span();
    let mut awaits_comma = false;
    let mut awaits_key = false;

    while let Some(token) = lexer.next() {
        match token {
            Ok(Token::BraceClose) if !awaits_key => return Ok(Value::Object(map)),
            Ok(Token::Comma) if awaits_comma => awaits_key = true,
            Ok(Token::String(key)) if !awaits_comma => {
                match lexer.next() {
                    Some(Ok(Token::Colon)) => (),
                    _ => {
                        return Err((
                            "unexpected token here, expecting ':'".to_owned(),
                            lexer.span(),
                        ))
                    }
                }
                let value = parse_value(lexer)?;
                map.insert(key, value);
                awaits_key = false;
            }
            _ => {
                return Err((
                    "unexpected token here (context: object)".to_owned(),
                    lexer.span(),
                ))
            }
        }
        awaits_comma = !awaits_key;
    }
    Err(("unmatched opening brace defined here".to_owned(), span))
}
    */

fn main() -> anyhow::Result<()> {
    let arena = Arena::new();

    let filename = "C:/source_control/anubis/examples/simple_cpp/ANUBIS";
    let src = fs::read_to_string(&filename).and_then(|s| Ok(arena.alloc_string(s)))?;

    let lexer = Token::lexer(src);

    for token in Token::lexer(src) {
        println!("{:?}", token);
    }

    // match parse_value(&mut lexer) {
    //     Ok(value) => println!("{:#?}", value),
    //     Err((msg, span)) => {
    //         use ariadne::{ColorGenerator, Label, Report, ReportKind, Source};

    //         let mut colors = ColorGenerator::new();

    //         let a = colors.next();

    //         Report::build(ReportKind::Error, &filename, 12)
    //             .with_message("Invalid JSON".to_string())
    //             .with_label(
    //                 Label::new((&filename, span))
    //                     .with_message(msg)
    //                     .with_color(a),
    //             )
    //             .finish()
    //             .eprint((&filename, Source::from(src)))
    //             .unwrap();
    //     }
    // }

    Ok(())
}
