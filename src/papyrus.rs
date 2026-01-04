#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use anyhow::Context;
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

    #[token("RelPaths", priority = 100)]
    RelPaths,

    #[token("Target", priority = 100)]
    Target,

    #[token("Targets", priority = 100)]
    Targets,

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

    #[token("multi_select")]
    MultiSelect,

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
    RelPaths(Vec<String>),
    Path(PathBuf),
    Paths(Vec<PathBuf>),
    Select(Select),
    MultiSelect(Select),
    String(String),
    Target(String),
    Targets(Vec<String>),
    Unresolved(UnresolvedInfo),
}

/// Diagnostic information about why a value could not be resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct UnresolvedInfo {
    pub reason: String,
    pub select_inputs: Vec<String>,
    pub select_values: Vec<String>,
    pub available_filters: Vec<String>,
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

    pub fn as_unresolved(&self) -> Option<&UnresolvedInfo> {
        match self {
            Value::Unresolved(info) => Some(info),
            _ => None,
        }
    }

    pub fn is_unresolved(&self) -> bool {
        matches!(self, Value::Unresolved(_))
    }

    pub fn get_index(&self, index: usize) -> anyhow::Result<&Value> {
        match self {
            Value::Array(arr) => {
                arr.get(index).ok_or(anyhow_loc!("Index [{}] not within bounds [{}]", index, arr.len()))
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
                                return Some(T::deserialize(de).map_err(|e| anyhow_loc!("{}", e)));
                            }
                        }
                        None
                    })
                    .collect();
                results
            }
            v => bail_loc!("papyrus::extract_objects: Expected Array, got {:#?}", v),
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
            .ok_or_else(|| anyhow_loc!("Expected Array, got {:#?}", self))?
            .iter()
            .find(|value| {
                value
                    .as_object()
                    .filter(
                        |obj| matches!(obj.fields.get(&*NAME), Some(Value::String(s)) if s == object_name),
                    )
                    .is_some()
            })
            .ok_or_else(|| anyhow_loc!("Object '{}' not found", object_name))
    }

    pub fn deserialize_named_object<T>(&self, object_name: &str) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned + PapyrusObjectType,
    {
        let value = self.get_named_object(object_name)?;

        // Verify the object has the correct type
        if let Value::Object(obj) = value {
            if obj.typename != T::name() {
                bail_loc!(
                    "Object '{}' has type '{}', expected '{}'",
                    object_name,
                    obj.typename,
                    T::name()
                );
            }
        } else {
            bail_loc!("Expected Object, got {:#?}", value);
        }

        let de = crate::papyrus_serde::ValueDeserializer::new(value);
        T::deserialize(de).map_err(|e| anyhow_loc!("{}", e))
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

/// Checks if a string looks like a relative target (e.g., `:targetname`)
pub fn is_relative_target(s: &str) -> bool {
    s.starts_with(':') && s.len() > 1 && !s[1..].contains('/')
}

/// Resolves a relative target string to an absolute target string.
/// For example, `:foo` with dir_relpath "examples/bar" becomes "//examples/bar:foo"
pub fn resolve_relative_target(s: &str, dir_relpath: &str) -> String {
    debug_assert!(is_relative_target(s));
    let target_name = &s[1..]; // skip the leading ':'
    format!("//{dir_relpath}:{target_name}")
}

pub fn resolve_value(
    value: Value,
    value_root: &Path,
    vars: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    resolve_value_with_dir(value, value_root, vars, None)
}

pub fn resolve_value_with_dir(
    value: Value,
    value_root: &Path,
    vars: &HashMap<String, String>,
    dir_relpath: Option<&str>,
) -> anyhow::Result<Value> {
    match value {
        Value::Array(values) => {
            let new_values = values
                .into_iter()
                .map(|v| resolve_value_with_dir(v, value_root, vars, dir_relpath))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Value::Array(new_values))
        }
        Value::Object(obj) => {
            let new_fields = obj
                .fields
                .into_iter()
                .map(|(k, v)| resolve_value_with_dir(v, value_root, vars, dir_relpath).map(|new_value| (k, new_value)))
                .collect::<anyhow::Result<HashMap<Identifier, Value>>>()?;

            // Check if any field is unresolved - if so, the entire object is unresolved
            for (key, value) in &new_fields {
                if let Some(info) = value.as_unresolved() {
                    tracing::trace!(
                        "Object field '{}' is unresolved, marking entire object as unresolved",
                        key.0
                    );
                    return Ok(Value::Unresolved(info.clone()));
                }
            }

            Ok(Value::Object(Object {
                typename: obj.typename,
                fields: new_fields,
            }))
        }
        Value::Map(map) => {
            let new_map = map
                .into_iter()
                .map(|(k, v)| resolve_value_with_dir(v, value_root, vars, dir_relpath).map(|new_value| (k, new_value)))
                .collect::<anyhow::Result<HashMap<Identifier, Value>>>()?;
            Ok(Value::Map(new_map))
        }
        Value::Glob(glob) => {
            let mut paths: HashSet<String> = Default::default();

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
            let mut excludes: Vec<glob::Pattern> = Default::default();
            for exclude in &glob.excludes {
                let full_pattern = value_root.join(&exclude);
                let pattern_str = full_pattern
                    .to_str()
                    .ok_or_else(|| anyhow_loc!("Invalid UTF-8 in glob pattern: {:?}", full_pattern))?;
                let pattern = glob::Pattern::new(pattern_str)
                    .with_context(|| format!("Failed to parse glob pattern: {}", pattern_str))?;
                excludes.push(pattern);
            }

            // apply excludes
            let paths: Vec<PathBuf> = paths
                .into_iter()
                .filter(|p| !excludes.iter().any(|e| e.matches(p)))
                .map(|p| p.into())
                .collect();

            if paths.is_empty() {
                bail_loc!(
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
        Value::RelPaths(rel_paths) => {
            let mut abs_paths: Vec<PathBuf> = Default::default();
            for rel_path in rel_paths {
                let mut abs_path = PathBuf::from(value_root);
                abs_path.push(&rel_path);
                let abs_path = abs_path.slash_fix();
                tracing::trace!("Resolved RelPath [{:?}] -> [{:?}]", &rel_path, &abs_path);
                abs_paths.push(abs_path);
            }
            Ok(Value::Paths(abs_paths))
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
                        let resolved_v = resolve_value_with_dir(v, value_root, vars, dir_relpath)?;
                        return Ok(resolved_v);
                    }
                } else {
                    // This is the default case
                    let v = s.filters.swap_remove(i).1;
                    let resolved_v = resolve_value_with_dir(v, value_root, vars, dir_relpath)?;
                    return Ok(resolved_v);
                }
            }
            // No filters matched - return Unresolved instead of error
            // This allows the target to remain unresolved until actually accessed
            let available_filters: Vec<String> = s
                .filters
                .iter()
                .map(|(filter, _)| {
                    match filter {
                        Some(f) => format!(
                            "({})",
                            f.iter()
                                .map(|opt| match opt {
                                    Some(vals) => vals.join(" | "),
                                    None => "_".to_string(),
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        None => "default".to_string(),
                    }
                })
                .collect();

            tracing::trace!(
                "Select did not match any filter, marking as unresolved.\n  Inputs: {:?}\n  Values: {:?}\n  Filters: {:?}",
                &s.inputs,
                &resolved_input,
                &available_filters
            );

            Ok(Value::Unresolved(UnresolvedInfo {
                reason: format!(
                    "select() did not match any filter for inputs {:?} with values {:?}",
                    s.inputs,
                    resolved_input.iter().map(|s| s.as_str()).collect::<Vec<_>>()
                ),
                select_inputs: s.inputs.clone(),
                select_values: resolved_input.iter().map(|s| s.to_string()).collect(),
                available_filters,
            }))
        }
        Value::MultiSelect(s) => {
            let resolved_input: Vec<&String> = s
                .inputs
                .iter()
                .map(|i| {
                    vars.get(i).context(format!(
                        "resolve_value: Failed because multi_select could not find required var [{}]. Vars: {:?}",
                        i, &vars
                    ))
                })
                .collect::<anyhow::Result<Vec<&String>>>()?;

            // Collect all matching filters (in order), storing default separately
            let mut matched_values: Vec<Value> = Vec::new();
            let mut default_value: Option<Value> = None;

            for (filter, value) in &s.filters {
                if let Some(filter) = filter {
                    assert_eq!(s.inputs.len(), filter.len());
                    let passes = resolved_input.iter().enumerate().all(|(idx, input)| match &filter[idx] {
                        Some(valid_values) => valid_values.iter().any(|v| v == *input),
                        None => true,
                    });
                    if passes {
                        matched_values.push(value.clone());
                    }
                } else {
                    // This is the default case - save it but don't add yet
                    default_value = Some(value.clone());
                }
            }

            // Default only applies when NO other filters matched
            if matched_values.is_empty() {
                if let Some(default_val) = default_value {
                    matched_values.push(default_val);
                }
            }

            // If no matches at all (no explicit matches and no default), return unresolved
            if matched_values.is_empty() {
                let available_filters: Vec<String> = s
                    .filters
                    .iter()
                    .map(|(filter, _)| match filter {
                        Some(f) => format!(
                            "({})",
                            f.iter()
                                .map(|opt| match opt {
                                    Some(vals) => vals.join(" | "),
                                    None => "_".to_string(),
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        None => "default".to_string(),
                    })
                    .collect();

                tracing::trace!(
                    "MultiSelect did not match any filter, marking as unresolved.\n  Inputs: {:?}\n  Values: {:?}\n  Filters: {:?}",
                    &s.inputs,
                    &resolved_input,
                    &available_filters
                );

                return Ok(Value::Unresolved(UnresolvedInfo {
                    reason: format!(
                        "multi_select() did not match any filter for inputs {:?} with values {:?}",
                        s.inputs,
                        resolved_input.iter().map(|s| s.as_str()).collect::<Vec<_>>()
                    ),
                    select_inputs: s.inputs.clone(),
                    select_values: resolved_input.iter().map(|s| s.to_string()).collect(),
                    available_filters,
                }));
            }

            // Concatenate all matched values in order
            let mut result = resolve_value(matched_values.remove(0), value_root, vars)?;
            for next_value in matched_values {
                let resolved_next = resolve_value(next_value, value_root, vars)?;
                result = resolve_concat(result, resolved_next, value_root, vars)?;
            }

            Ok(result)
        }
        Value::Concat(pair) => {
            let left = resolve_value_with_dir(*pair.0, value_root, vars, dir_relpath)?;
            let right = resolve_value_with_dir(*pair.1, value_root, vars, dir_relpath)?;

            // If either side is unresolved, propagate the unresolved state
            if let Some(info) = left.as_unresolved() {
                return Ok(Value::Unresolved(info.clone()));
            }
            if let Some(info) = right.as_unresolved() {
                return Ok(Value::Unresolved(info.clone()));
            }

            resolve_concat_with_dir(left, right, value_root, vars, dir_relpath)
        }
        Value::Path(_) => Ok(value),
        Value::Paths(_) => Ok(value),
        Value::String(ref s) => {
            // Check if the string looks like a relative target and resolve it
            if let Some(dir) = dir_relpath {
                if is_relative_target(s) {
                    let resolved = resolve_relative_target(s, dir);
                    tracing::trace!("Resolved relative target [{:?}] -> [{:?}]", s, &resolved);
                    return Ok(Value::String(resolved));
                }
            }
            Ok(value)
        }
        Value::Target(ref s) => {
            // Resolve relative target if we have dir context
            if let Some(dir) = dir_relpath {
                if is_relative_target(s) {
                    let resolved = resolve_relative_target(s, dir);
                    tracing::trace!("Resolved relative Target [{:?}] -> [{:?}]", s, &resolved);
                    return Ok(Value::Target(resolved));
                }
            }
            Ok(value)
        }
        Value::Targets(ref targets) => {
            // Resolve any relative targets in the list
            if let Some(dir) = dir_relpath {
                let resolved: Vec<String> = targets
                    .iter()
                    .map(|s| {
                        if is_relative_target(s) {
                            let resolved = resolve_relative_target(s, dir);
                            tracing::trace!("Resolved relative Target [{:?}] -> [{:?}]", s, &resolved);
                            resolved
                        } else {
                            s.clone()
                        }
                    })
                    .collect();
                return Ok(Value::Targets(resolved));
            }
            Ok(value)
        }
        Value::Unresolved(_) => Ok(value), // Pass through unresolved values
    }
}

fn resolve_concat(
    left: Value,
    right: Value,
    value_root: &Path,
    vars: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    resolve_concat_with_dir(left, right, value_root, vars, None)
}

fn resolve_concat_with_dir(
    left: Value,
    right: Value,
    value_root: &Path,
    vars: &HashMap<String, String>,
    dir_relpath: Option<&str>,
) -> anyhow::Result<Value> {
    // Resolve left and right
    let mut left = resolve_value_with_dir(left, value_root, vars, dir_relpath)?;
    let mut right = resolve_value_with_dir(right, value_root, vars, dir_relpath)?;

    // If either side is unresolved, propagate the unresolved state
    if let Some(info) = left.as_unresolved() {
        return Ok(Value::Unresolved(info.clone()));
    }
    if let Some(info) = right.as_unresolved() {
        return Ok(Value::Unresolved(info.clone()));
    }

    // perform concatenation
    match (&mut left, right) {
        (Value::Array(left), Value::Array(mut right)) => {
            left.append(&mut right);
            return Ok(Value::Array(std::mem::take(left)));
        }
        (Value::Paths(left), Value::Paths(mut right)) => {
            left.append(&mut right);
            return Ok(Value::Paths(std::mem::take(left)));
        }
        (Value::Object(left), Value::Object(right)) => {
            if left.typename != right.typename {
                bail_loc!("resolve_value: Cannot concentate objects of different types.\n  Left: {:?}\n  Right: {:?}",
                &left,
                &right)
            }

            // concat each field in right into left
            for (key, r) in right.fields {
                match left.fields.get_mut(&key) {
                    Some(l) => {
                        // key is in both left and right, concat
                        *l = resolve_concat_with_dir(
                            std::mem::replace(l, Value::Array(Vec::new())),
                            r,
                            value_root,
                            vars,
                            dir_relpath,
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
        (Value::String(left), Value::Path(right)) => {
            let right_str = right.to_string_lossy();
            let result = format!("{}{}", left, right_str);
            return Ok(Value::String(result));
        }
        (Value::Targets(left), Value::Targets(mut right)) => {
            left.append(&mut right);
            return Ok(Value::Targets(std::mem::take(left)));
        }
        (left, right) => {
            bail_loc!(
                "resolve_value: Cannot concatenate values.\n    Left: {:?}\n    Right: {:?}",
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
                anyhow_loc!(
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
            bail_loc!(
                "parse_object: Expected identifier token for new rule. Found [{:?}]",
                token
            );
        }
    } else {
        bail_loc!("parse_object: Ran out of tokens");
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

pub fn parse_relpaths<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    let mut paths: Vec<String> = Default::default();

    expect_token(lexer, &Token::RelPaths)?;
    expect_token(lexer, &Token::ParenOpen)?;
    expect_token(lexer, &Token::BracketOpen)?;

    while lexer.peek() != &Some(Ok(Token::BracketClose)) {
        let s = expect_string(lexer)?;
        paths.push(s);
        consume_token(lexer, &Token::Comma);
    }

    expect_token(lexer, &Token::BracketClose)?;
    expect_token(lexer, &Token::ParenClose)?;
    consume_token(lexer, &Token::Comma);

    Ok(Value::RelPaths(paths))
}

pub fn parse_target<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::Target)?;
    expect_token(lexer, &Token::ParenOpen)?;
    let s = expect_string(lexer)?;
    expect_token(lexer, &Token::ParenClose)?;
    consume_token(lexer, &Token::Comma);
    Ok(Value::Target(s))
}

pub fn parse_targets<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    let mut targets: Vec<String> = Default::default();

    expect_token(lexer, &Token::Targets)?;
    expect_token(lexer, &Token::ParenOpen)?;
    expect_token(lexer, &Token::BracketOpen)?;

    while lexer.peek() != &Some(Ok(Token::BracketClose)) {
        let s = expect_string(lexer)?;
        targets.push(s);
        consume_token(lexer, &Token::Comma);
    }

    expect_token(lexer, &Token::BracketClose)?;
    expect_token(lexer, &Token::ParenClose)?;
    consume_token(lexer, &Token::Comma);

    Ok(Value::Targets(targets))
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
        Some(Ok(Token::MultiSelect)) => parse_multi_select(lexer),
        Some(Ok(Token::Identifier(_))) => parse_object(lexer),
        Some(Ok(Token::RelPath)) => parse_relpath(lexer),
        Some(Ok(Token::RelPaths)) => parse_relpaths(lexer),
        Some(Ok(Token::Target)) => parse_target(lexer),
        Some(Ok(Token::Targets)) => parse_targets(lexer),
        Some(Ok(t)) => bail_loc!("parse_value: Unexpected token [{:?}]", t),
        v => bail_loc!("parse_value: Unexpected lexer value [{:?}]", v),
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
            Some(Ok(t)) => bail_loc!("parse_map: Unexpected token [{:?}]", t),
            t => bail_loc!("parse_map: Unexpected token [{:?}]", t),
        }
        consume_token(lexer, &Token::Comma);
    }
    Ok(Value::Map(map))
}

pub fn parse_glob<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::Glob)?;
    expect_token(lexer, &Token::ParenOpen)?;

    let mut glob: Glob = Default::default();

    let parse_paths = |lexer: &mut PeekLexer<'src>| -> ParseResult<Vec<String>> {
        let mut paths = Vec::<String>::new();

        expect_token(lexer, &Token::BracketOpen)?;
        while !consume_token(lexer, &Token::BracketClose) {
            match lexer.next() {
                Some(Ok(Token::String(s))) => paths.push(s.into()),
                t => bail_loc!("parse_glob: Unexpected token [{:?}]", t),
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
    } else {
        glob.includes = parse_paths(lexer)?;
    }
    expect_token(lexer, &Token::ParenClose)?;

    Ok(Value::Glob(glob))
}

/// Parse the body of a select/multi_select (shared logic)
fn parse_select_body<'src>(lexer: &mut PeekLexer<'src>, keyword: &str) -> ParseResult<Select> {
    expect_token(lexer, &Token::ParenOpen)?;
    let mut inputs = Vec::<String>::default();
    expect_token(lexer, &Token::ParenOpen)?;
    loop {
        if consume_token(lexer, &Token::ParenClose) {
            break;
        }
        match lexer.next() {
            Some(Ok(Token::Identifier(i))) => inputs.push(i.into()),
            t => bail_loc!("{}: Unexpected token [{:?}]", keyword, t),
        }
        consume_token(lexer, &Token::Comma);
    }
    let mut seen = std::collections::HashSet::new();
    for input in &inputs {
        if !seen.insert(input) {
            bail_loc!("{}: duplicate input found: {}", keyword, input);
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
                                Some(Ok(t)) => bail_loc!("{}: Unexpected token [{:?}]", keyword, t),
                                v => bail_loc!("{}: Unexpected value [{:?}]", keyword, v),
                            }
                            consume_token(lexer, &Token::Comma);
                        }
                        select_filter.push(Some(values));
                    }
                    Some(Ok(t)) => bail_loc!("{}: Unexpected token [{:?}]", keyword, t),
                    v => bail_loc!("{}: Unexpected value [{:?}]", keyword, v),
                }
                consume_token(lexer, &Token::Comma);
            }
            if select_filter.len() != inputs.len() {
                bail_loc!("{}: Num inputs ({}) and num filters ({}) length must match.\nInputs: {:?}\nFilter: {:?}",
                    keyword,
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
    Ok(Select { inputs, filters })
}

pub fn parse_select<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::Select)?;
    let select = parse_select_body(lexer, "parse_select")?;
    Ok(Value::Select(select))
}

pub fn parse_multi_select<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::MultiSelect)?;
    let select = parse_select_body(lexer, "parse_multi_select")?;
    Ok(Value::MultiSelect(select))
}

pub fn expect_token<'src>(lexer: &mut PeekLexer<'src>, expected_token: &Token<'src>) -> ParseResult<()> {
    match lexer.next() {
        Some(Ok(token)) => {
            if &token == expected_token {
                Ok(())
            } else {
                bail_loc!(
                    "expect_token: Token [{:?}] did not match expected token [{:?}]",
                    token,
                    expected_token
                );
            }
        }
        e => bail_loc!(
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
        Some(Ok(t)) => bail_loc!("expect_identifier: Unexpected token [{:?}]", t),
        t => bail_loc!("expect_identifier: Unexpected result [{:?}]", t),
    }
}

pub fn expect_string<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<String> {
    let token = lexer.next();
    match token {
        Some(Ok(Token::String(s))) => Ok(s.to_owned()),
        Some(Ok(t)) => bail_loc!("expect_identifier: Unexpected token [{:?}]", t),
        t => bail_loc!("expect_identifier: Unexpected result [{:?}]", t),
    }
}

pub fn read_papyrus_file(path: &Path) -> anyhow::Result<Value> {
    if !std::fs::exists(path)? {
        bail_loc!("read_papyrus failed because file didn't exist: [{:?}]", path);
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
            bail_loc!("{}", err_msg)
        }
        Ok(v) => Ok(v),
    }
}

/// Helper to format select/multi_select values
fn format_select_value(
    keyword: &str,
    sel: &Select,
    indent: usize,
    indent_str: &str,
    next_indent: &str,
) -> String {
    let inputs = sel.inputs.join(", ");
    let filters: Vec<String> = sel
        .filters
        .iter()
        .map(|(filter, val)| {
            let filter_str = match filter {
                Some(f) => {
                    let parts: Vec<String> = f
                        .iter()
                        .map(|opt| match opt {
                            Some(vals) => vals.join(" | "),
                            None => "_".to_string(),
                        })
                        .collect();
                    format!("({})", parts.join(", "))
                }
                None => "default".to_string(),
            };
            format!("{}{} = {}", next_indent, filter_str, format_value(val, indent + 1))
        })
        .collect();
    format!(
        "{}(({}) => {{\n{}\n{}}})",
        keyword,
        inputs,
        filters.join(",\n"),
        indent_str
    )
}

/// Format a Value as a human-readable string with proper indentation
pub fn format_value(value: &Value, indent: usize) -> String {
    let indent_str = "  ".repeat(indent);
    let next_indent = "  ".repeat(indent + 1);

    match value {
        Value::String(s) => format!("\"{}\"", s),
        Value::Path(p) => format!("\"{}\"", p.display()),
        Value::Paths(paths) => {
            if paths.is_empty() {
                "[]".to_string()
            } else {
                let items: Vec<String> = paths.iter().map(|p| format!("{}\"{}\"", next_indent, p.display())).collect();
                format!("[\n{}\n{}]", items.join(",\n"), indent_str)
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                "[]".to_string()
            } else {
                let items: Vec<String> =
                    arr.iter().map(|v| format!("{}{}", next_indent, format_value(v, indent + 1))).collect();
                format!("[\n{}\n{}]", items.join(",\n"), indent_str)
            }
        }
        Value::Map(map) => {
            if map.is_empty() {
                "{}".to_string()
            } else {
                let items: Vec<String> = map
                    .iter()
                    .sorted_by_key(|(k, _)| &k.0)
                    .map(|(k, v)| format!("{}{} = {}", next_indent, k.0, format_value(v, indent + 1)))
                    .collect();
                format!("{{\n{}\n{}}}", items.join(",\n"), indent_str)
            }
        }
        Value::Object(obj) => {
            if obj.fields.is_empty() {
                format!("{}()", obj.typename)
            } else {
                let items: Vec<String> = obj
                    .fields
                    .iter()
                    .sorted_by_key(|(k, _)| &k.0)
                    .map(|(k, v)| format!("{}{} = {}", next_indent, k.0, format_value(v, indent + 1)))
                    .collect();
                format!("{}(\n{}\n{})", obj.typename, items.join(",\n"), indent_str)
            }
        }
        Value::RelPath(p) => format!("RelPath(\"{}\")", p),
        Value::RelPaths(paths) => {
            let items: Vec<String> = paths.iter().map(|p| format!("\"{}\"", p)).collect();
            format!("RelPaths([{}])", items.join(", "))
        }
        Value::Target(t) => format!("Target(\"{}\")", t),
        Value::Targets(targets) => {
            let items: Vec<String> = targets.iter().map(|t| format!("\"{}\"", t)).collect();
            format!("Targets([{}])", items.join(", "))
        }
        Value::Glob(glob) => {
            let includes: Vec<String> = glob.includes.iter().map(|s| format!("\"{}\"", s)).collect();
            if glob.excludes.is_empty() {
                format!("glob([{}])", includes.join(", "))
            } else {
                let excludes: Vec<String> = glob.excludes.iter().map(|s| format!("\"{}\"", s)).collect();
                format!(
                    "glob(includes = [{}], excludes = [{}])",
                    includes.join(", "),
                    excludes.join(", ")
                )
            }
        }
        Value::Select(sel) => format_select_value("select", sel, indent, &indent_str, &next_indent),
        Value::MultiSelect(sel) => format_select_value("multi_select", sel, indent, &indent_str, &next_indent),
        Value::Concat((left, right)) => {
            format!("{} + {}", format_value(left, indent), format_value(right, indent))
        }
        Value::Unresolved(info) => {
            format!("<unresolved: {}>", info.reason)
        }
    }
}
