#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use anyhow::{anyhow, bail, Context};
use itertools::Itertools;
use logos::{Lexer, Logos, Span};

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::DefaultHasher;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::papyrus_serde::ValueDeserializer;
use crate::util::SlashFix;
use crate::{anyhow_loc, bail_loc, function_name};

// ----------------------------------------------------------------------------
// type declarations
// ----------------------------------------------------------------------------
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

    #[token("RelPath", priority = 100)]
    RelPath,

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

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Array(Vec<Value>),
    Concat((Box<Value>, Box<Value>)),
    Object(Object),
    Glob(Glob),
    Map(HashMap<Identifier, Value>),
    RelPath(String),
    Path(PathBuf),
    Paths(Vec<PathBuf>),
    Select(Select),
    String(String),
}

pub type ParseResult<T> = anyhow::Result<T>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Glob {
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
}

#[derive(Debug)]
pub struct SpannedError {
    pub error: anyhow::Error,
    pub span: Span,
}

pub struct PeekLexer<'source> {
    pub lexer: &'source mut Lexer<'source, Token<'source>>,
    pub peeked: Option<Option<Result<Token<'source>, ()>>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object {
    pub typename: String,
    pub fields: HashMap<Identifier, Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub inputs: Vec<String>,
    pub filters: Vec<(Option<SelectFilter>, Value)>,
}

pub type SelectFilter = Vec<Option<Vec<String>>>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Identifier(pub String);

impl std::borrow::Borrow<str> for Identifier {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl std::hash::Hash for Identifier {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

pub trait PapyrusObjectType {
    fn name() -> &'static str;
}

// ----------------------------------------------------------------------------
// implementations
// ----------------------------------------------------------------------------
impl Value {
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(arr) => Some(arr),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Value::Object(obj) => Some(obj),
            _ => None,
        }
    }

    pub fn get_index(&self, index: usize) -> anyhow::Result<&Value> {
        match self {
            Value::Array(arr) => {
                arr.get(index).ok_or(anyhow!("Index [{}] not within bounds [{}]", index, arr.len()))
            }
            v => bail_loc!("Can't access non-array Papyrus::Value by index. [{:#?}]", v),
        }
    }

    pub fn get_key(&self, key: &str) -> anyhow::Result<&Value> {
        //let key = Identifier(key.to_owned());
        match self {
            Value::Object(obj) => obj
                .fields
                .get(key)
                .ok_or_else(|| anyhow_loc!("Key [{}] not found in Object [{:#?}]", key, obj)),
            Value::Map(map) => {
                map.get(key).ok_or_else(|| anyhow_loc!("Key [{}] not found in Object [{:#?}]", key, map))
            }
            _ => bail_loc!("Can't access Value by key. [{:#?}]", self),
        }
    }

    pub fn deserialize_objects<T>(&self) -> anyhow::Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned + PapyrusObjectType,
    {
        match self {
            Value::Array(arr) => {
                let results: Result<Vec<T>, _> = arr
                    .into_iter()
                    .filter_map(|v| {
                        if let Value::Object(ref obj) = v {
                            if obj.typename == T::name() {
                                let de = crate::papyrus_serde::ValueDeserializer::new(&v);
                                return Some(T::deserialize(de).map_err(|e| anyhow::anyhow!("{}", e)));
                            }
                        }
                        None
                    })
                    .collect();
                results
            }
            v => bail!("papyrus::extract_objects: Expected Array, got {:#?}", v),
        }
    }

    pub fn deserialize_single_object<T>(&self) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned + PapyrusObjectType,
    {
        let mut objects = self.deserialize_objects::<T>()?;
        if objects.len() != 1 {
            bail_loc!(
                "deserialize_single_object: Expected 1 object, found {}. {:#?}",
                objects.len(),
                &self
            );
        }

        Ok(objects.remove(0))
    }

    pub fn get_named_object(&self, object_name: &str) -> anyhow::Result<&Value> {
        static NAME: LazyLock<Identifier> = LazyLock::new(|| Identifier("name".to_owned()));

        // TODO:
        //#error detect duplicates and error

        self.as_array()
            .ok_or_else(|| anyhow::anyhow!("Expected Array, got {:#?}", self))?
            .iter()
            .find(|value| {
                value
                    .as_object()
                    .filter(
                        |obj| matches!(obj.fields.get(&*NAME), Some(Value::String(s)) if s == object_name),
                    )
                    .is_some()
            })
            .ok_or_else(|| anyhow::anyhow!("Object '{}' not found", object_name))
    }

    pub fn deserialize_named_object<T>(&self, object_name: &str) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned + PapyrusObjectType,
    {
        let value = self.get_named_object(object_name)?;

        // Verify the object has the correct type
        if let Value::Object(obj) = value {
            if obj.typename != T::name() {
                bail!(
                    "Object '{}' has type '{}', expected '{}'",
                    object_name,
                    obj.typename,
                    T::name()
                );
            }
        } else {
            bail!("Expected Object, got {:#?}", value);
        }

        let de = crate::papyrus_serde::ValueDeserializer::new(value);
        T::deserialize(de).map_err(|e| anyhow::anyhow!("{}", e))
    }
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

// ----------------------------------------------------------------------------
// free standing functions
// ----------------------------------------------------------------------------
pub fn resolve_value(
    value: Value,
    value_root: &Path,
    vars: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    match value {
        Value::Array(values) => {
            let new_values = values
                .into_iter()
                .map(|v| resolve_value(v, value_root, vars))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Value::Array(new_values))
        }
        Value::Object(obj) => {
            let new_fields = obj
                .fields
                .into_iter()
                .map(|(k, v)| resolve_value(v, value_root, vars).map(|new_value| (k, new_value)))
                .collect::<anyhow::Result<HashMap<Identifier, Value>>>()?;
            Ok(Value::Object(Object {
                typename: obj.typename,
                fields: new_fields,
            }))
        }
        Value::Map(map) => {
            let new_map = map
                .into_iter()
                .map(|(k, v)| resolve_value(v, value_root, vars).map(|new_value| (k, new_value)))
                .collect::<anyhow::Result<HashMap<Identifier, Value>>>()?;
            Ok(Value::Map(new_map))
        }
        Value::Glob(glob) => {
            let mut paths : HashSet<String> = Default::default();

            // find includes
            for pattern in &glob.includes {
                let full_pattern = value_root.join(&pattern);
                let pattern_str = full_pattern
                    .to_str()
                    .ok_or_else(|| anyhow_loc!("Invalid UTF-8 in glob pattern: {:?}", full_pattern))?;
                for entry in glob::glob(pattern_str)
                    .with_context(|| format!("Failed to parse glob pattern: {}", pattern_str))?
                {
                    match entry {
                        Ok(path) => paths.insert(path.to_string_lossy().replace("\\", "/")),
                        Err(e) => bail_loc!("Error matching glob pattern {}: {:?}", pattern_str, e),
                    };
                }
            }

            // build exclude pattern strings
            let mut excludes : Vec<glob::Pattern> = Default::default();
            for exclude in &glob.excludes {
                let full_pattern = value_root.join(&exclude);
                let pattern_str = full_pattern
                    .to_str()
                    .ok_or_else(|| anyhow_loc!("Invalid UTF-8 in glob pattern: {:?}", full_pattern))?;
                let pattern = glob::Pattern::new(pattern_str).with_context(|| format!("Failed to parse glob pattern: {}", pattern_str))?;
                excludes.push(pattern);
            }
            
            // apply excludes
            let paths : Vec<PathBuf> = paths.into_iter().filter(|p| {
                !excludes.iter().any(|e| e.matches(p))
            }).map(|p| p.into()).collect();

            

            if paths.is_empty() {
                bail!(
                    "Glob [{:?}] failed to match anything. Root: [{:?}]",
                    &glob,
                    value_root
                );
            } else {
                tracing::trace!("Glob [{:?}] resolved: [{:?}]", &glob, &paths);
                Ok(Value::Paths(paths))
            }
        }
        Value::RelPath(rel_path) => {
            let mut abs_path = PathBuf::from(value_root);
            abs_path.push(&rel_path);
            let abs_path = abs_path.slash_fix();
            tracing::trace!("Resolved RelPath [{:?}] -> [{:?}]", &rel_path, &abs_path);
            Ok(Value::Path(abs_path))
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
                    let passes = resolved_input.iter().enumerate().all(|(idx, input)| match &filter[idx] {
                        Some(valid_values) => valid_values.iter().any(|v| v == *input),
                        None => true,
                    });
                    if passes {
                        let v = s.filters.swap_remove(i).1;
                        let resolved_v = resolve_value(v, value_root, vars)?;
                        return Ok(resolved_v);
                    }
                } else {
                    let v = s.filters.swap_remove(i).1;
                    let resolved_v = resolve_value(v, value_root, vars)?;
                    return Ok(resolved_v);
                }
            }
            bail!(
                "resolve_value: failed to resolve select. No filters matched.\n  Select: {:?}\n  Vars: {:?}",
                s,
                vars
            );
        }
        Value::Concat(pair) => {
            let mut left = resolve_value(*pair.0, value_root, vars)?;
            let mut right = resolve_value(*pair.1, value_root, vars)?;
            resolve_concat(left, right, value_root, vars)
        }
        _ => Ok(value),
    }
}

fn resolve_concat(
    left: Value,
    right: Value,
    value_root: &Path,
    vars: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    // Resolve left and right
    let mut left = resolve_value(left, value_root, vars)?;
    let mut right = resolve_value(right, value_root, vars)?;

    // perform concatenation
    match (&mut left, right) {
        (Value::Array(left), Value::Array(mut right)) => {
            left.append(&mut right);
            return Ok(Value::Array(std::mem::take(left)));
        }
        (Value::Object(left), Value::Object(right)) => {
            if left.typename != right.typename {
                bail!("resolve_value: Cannot concentate objects of different types.\n  Left: {:?}\n  Right: {:?}",
                &left,
                &right)
            }

            // concat each field in right into left
            for (key, r) in right.fields {
                match left.fields.get_mut(&key) {
                    Some(l) => {
                        // key is in both left and right, concat
                        *l = resolve_concat(
                            std::mem::replace(l, Value::Array(Vec::new())),
                            r,
                            value_root,
                            vars,
                        )?;
                    }
                    None => {
                        // key missing in left, just add it
                        left.fields.insert(key, r);
                    }
                }
            }

            return Ok(Value::Object(std::mem::take(left)));
        }
        (left, right) => {
            bail!(
                "resolve_value: Cannot concatenate values.\n  Left: {:?}\n  Right: {:?}",
                &left,
                &right
            )
        }
    }
}

pub fn parse_config<'src>(lexer: &'src mut Lexer<'src, Token<'src>>) -> anyhow::Result<Value, SpannedError> {
    let mut objects: Vec<Value> = Default::default();
    let mut lexer = PeekLexer { lexer, peeked: None };
    while lexer.peek() != &None {
        match parse_object(&mut lexer) {
            Ok(object) => objects.push(object),
            Err(e) => {
                return Err(SpannedError {
                    error: e,
                    span: lexer.lexer.span(),
                });
            }
        }
    }
    Ok(Value::Array(objects))
}

pub fn parse_object<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    let obj = if let Some(token) = lexer.next() {
        if let Ok(Token::Identifier(obj_type)) = token {
            let mut fields: HashMap<Identifier, Value> = Default::default();
            expect_token(lexer, &Token::ParenOpen).map_err(|e| {
                anyhow!(
                    "parse_object: {}\n Error while parsing object [{:?}]",
                    e,
                    obj_type
                )
            })?;
            loop {
                if consume_token(lexer, &Token::ParenClose) {
                    break Ok::<Value, anyhow::Error>(Value::Object(Object {
                        typename: obj_type.to_owned(),
                        fields,
                    }));
                }
                let ident = expect_identifier(lexer)?;
                expect_token(lexer, &Token::Equals)?;
                let value = parse_value(lexer)?;
                fields.insert(ident, value);
                consume_token(lexer, &Token::Comma);
            }
        } else {
            bail!(
                "parse_object: Expected identifier token for new rule. Found [{:?}]",
                token
            );
        }
    } else {
        bail!("parse_object: Ran out of tokens");
    }?;

    if consume_token(lexer, &Token::Plus) {
        let left = Box::new(obj);
        let right = Box::new(parse_value(lexer)?);
        Ok(Value::Concat((left, right)))
    } else {
        Ok(obj)
    }
}

pub fn parse_relpath<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::RelPath)?;
    expect_token(lexer, &Token::ParenOpen)?;
    let s = expect_string(lexer)?;
    expect_token(lexer, &Token::ParenClose)?;
    consume_token(lexer, &Token::Comma);
    Ok(Value::RelPath(s))
}

pub fn parse_value<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    let v = match lexer.peek() {
        Some(Ok(Token::String(s))) => {
            let s = s.to_string();
            lexer.next();
            Ok(Value::String(s))
        }
        Some(Ok(Token::Glob)) => parse_glob(lexer),
        Some(Ok(Token::BraceOpen)) => parse_map(lexer),
        Some(Ok(Token::BracketOpen)) => parse_array(lexer),
        Some(Ok(Token::Select)) => parse_select(lexer),
        Some(Ok(Token::Identifier(_))) => parse_object(lexer),
        Some(Ok(Token::RelPath)) => parse_relpath(lexer),
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
    expect_token(lexer, &Token::BracketOpen)?;
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

pub fn parse_map<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::BraceOpen)?;
    let mut map: HashMap<Identifier, Value> = Default::default();
    loop {
        if consume_token(lexer, &Token::BraceClose) {
            break;
        }

        match lexer.next() {
            Some(Ok(Token::Identifier(key))) => {
                expect_token(lexer, &Token::Equals)?;
                let value = parse_value(lexer)?;
                map.insert(Identifier(key.to_owned()), value);
            }
            Some(Ok(t)) => bail!("parse_map: Unexpected token [{:?}]", t),
            t => bail!("parse_map: Unexpected token [{:?}]", t),
        }
        consume_token(lexer, &Token::Comma);
    }
    Ok(Value::Map(map))
}

pub fn parse_glob<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::Glob)?;
    expect_token(lexer, &Token::ParenOpen)?;

    let mut glob : Glob = Default::default();

    let parse_paths = |lexer: &mut PeekLexer<'src>| -> ParseResult<Vec<String>> {
        let mut paths = Vec::<String>::new();

        expect_token(lexer, &Token::BracketOpen)?;
        while !consume_token(lexer, &Token::BracketClose) {
            match lexer.next() {
                Some(Ok(Token::String(s))) => paths.push(s.into()),
                t => bail!("parse_glob: Unexpected token [{:?}]", t),
            }
            consume_token(lexer, &Token::Comma);
        }
        consume_token(lexer, &Token::Comma);

        Ok(paths)
    };

    if consume_token(lexer, &Token::Identifier("includes")) {
        expect_token(lexer, &Token::Equals)?;
        glob.includes = parse_paths(lexer)?;

        if consume_token(lexer, &Token::Identifier("excludes")) {
            expect_token(lexer, &Token::Equals)?;
            glob.excludes = parse_paths(lexer)?;
        }
        consume_token(lexer, &Token::Comma);
    }
    else {
        glob.includes = parse_paths(lexer)?;
    }
    expect_token(lexer, &Token::ParenClose)?;

    Ok(Value::Glob(glob))
}

pub fn parse_select<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::Select)?;
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

pub fn expect_string<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<String> {
    let token = lexer.next();
    match token {
        Some(Ok(Token::String(s))) => Ok(s.to_owned()),
        Some(Ok(t)) => bail!("expect_identifier: Unexpected token [{:?}]", t),
        t => bail!("expect_identifier: Unexpected result [{:?}]", t),
    }
}

pub fn read_papyrus_file(path: &Path) -> anyhow::Result<Value> {
    if !std::fs::exists(path)? {
        bail!("read_papyrus failed because file didn't exist: [{:?}]", path);
    }

    let src = fs::read_to_string(path)?;
    read_papyrus_str(&src, &path.to_string_lossy())
}

pub fn read_papyrus_str(str: &str, str_src: &str) -> anyhow::Result<Value> {
    let mut lexer = Token::lexer(str);
    let result = parse_config(&mut lexer);

    match result {
        Err(e) => {
            use ariadne::{ColorGenerator, Label, Report, ReportKind, Source};

            let mut colors = ColorGenerator::new();
            let a = colors.next();

            let mut buf: Vec<u8> = Default::default();
            Report::build(ReportKind::Error, str_src, 12)
                .with_message(format!("Invalid Papyrus: {}", e.error))
                .with_label(Label::new((str_src, e.span)).with_color(a))
                .finish()
                .write_for_stdout((str_src, Source::from(str)), &mut buf)
                .unwrap();

            let err_msg = String::from_utf8(buf)?;
            bail!("{}", err_msg)
        }
        Ok(v) => Ok(v),
    }
}
