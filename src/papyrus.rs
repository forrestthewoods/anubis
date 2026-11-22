//! Papyrus, the language of `ANUBIS` files: lexer, parser and resolver.
//!
//! Parsing turns a file into a [`Value`] tree. Resolution evaluates it for one
//! mode: `select`/`multi_select` against the mode's variables, `RelPath` and
//! `glob` to absolute paths, `:name` targets to full labels, `+` to joined
//! values. See `docs/papyrus.md` for the language itself.

#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use anyhow::Context;
use itertools::Itertools;
use logos::{Lexer, Logos, Span};

use camino::{Utf8Path, Utf8PathBuf};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::hash::DefaultHasher;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::anubis::AnubisTarget;
use crate::papyrus_serde::ValueDeserializer;
use crate::util::SlashFix;
use crate::{anyhow_loc, bail_loc, bail_loc_if, function_name};

// ----------------------------------------------------------------------------
// type declarations
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Logos, PartialEq)]
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

    /// Whole numbers only. A literal that doesn't fit in an `i64` is a lex error.
    #[regex(r"-?[0-9]+", |lex| lex.slice().parse::<i64>().ok())]
    Integer(i64),

    #[token("(")]
    ParenOpen,

    #[token(")")]
    ParenClose,

    #[token("RelPath")]
    RelPath,

    #[token("RelPaths")]
    RelPaths,

    #[token("Target")]
    Target,

    #[token("Targets")]
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
    Path(Utf8PathBuf),
    Paths(Vec<Utf8PathBuf>),
    Select(Select),
    MultiSelect(Select),
    String(String),
    Integer(i64),
    Bool(bool),
    Target(AnubisTarget),
    Targets(Vec<AnubisTarget>),
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
    /// Shared values defined so far in this file (`name = value` at the top
    /// level). A bare `name` in a value is replaced by a copy of the
    /// definition, still unresolved, so a `select` or `glob` in it resolves
    /// where it is used.
    pub defs: HashMap<String, SharedValue>,
    /// Where the error being returned is, when that isn't the last token read.
    pub error_span: Option<Span>,
}

pub struct SharedValue {
    pub value: Value,
    /// The name in its definition.
    pub span: Span,
    pub used: bool,
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

        // TODO: error on duplicate names.

        let arr = self
            .as_array()
            .ok_or_else(|| anyhow_loc!("Expected Array, got {:#?}", self))?;

        arr.iter()
            .find(|value| {
                value
                    .as_object()
                    .filter(
                        |obj| matches!(obj.fields.get(&*NAME), Some(Value::String(s)) if s == object_name),
                    )
                    .is_some()
            })
            .ok_or_else(|| {
                let available_names: Vec<&str> = arr
                    .iter()
                    .filter_map(|value| {
                        value.as_object().and_then(|obj| {
                            if let Some(Value::String(s)) = obj.fields.get(&*NAME) {
                                Some(s.as_str())
                            } else {
                                None
                            }
                        })
                    })
                    .collect();

                // Objects whose select() matched nothing for this mode are
                // dropped as Unresolved and lose their name; say why, or the
                // error just claims the object does not exist.
                let unresolved: Vec<&str> = arr
                    .iter()
                    .filter_map(|v| v.as_unresolved().map(|info| info.reason.as_str()))
                    .collect();
                if !unresolved.is_empty() {
                    anyhow_loc!(
                        "Object '{}' not found. {} object(s) in this file could not be resolved for this mode, which may include it:\n  {}",
                        object_name,
                        unresolved.len(),
                        unresolved.join("\n  ")
                    )
                } else if available_names.is_empty() {
                    anyhow_loc!("Object '{}' not found (no named objects available)", object_name)
                } else {
                    anyhow_loc!(
                        "Object '{}' not found. Available targets: [{}]",
                        object_name,
                        available_names.join(", ")
                    )
                }
            })
    }

    pub fn deserialize_named_object<T>(&self, object_name: &str) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned + PapyrusObjectType,
    {
        let value = self.get_named_object(object_name)?;

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
pub fn resolve_value(
    value: Value,
    value_root: &Path,
    vars: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    resolve_value_with_dir(value, value_root, vars, None)
}

/// Paths in an ANUBIS file must stay under that file's directory: no `..`
/// and no absolute paths. Everything a config references lives beneath it,
/// which keeps path handling (and cache keys) simple.
fn check_relpath(path: &str, what: &str) -> anyhow::Result<()> {
    let p = std::path::Path::new(path);
    let escapes = p.is_absolute()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.as_bytes().get(1) == Some(&b':')
        || path.split(['/', '\\']).any(|seg| seg == "..");
    if escapes {
        bail_loc!(
            "{} [{}] must be relative to its ANUBIS file's directory and stay inside it (no '..', no absolute paths)",
            what,
            path
        );
    }
    Ok(())
}

/// `check_relpath` only sees the pattern text, but a wildcard can match `..`
/// (`.[.]`, `.?`, `[.][.]`): the glob crate yields it as a literal component
/// (`pkg/../../etc/x`). So check each match too: it must sit under the ANUBIS
/// file's directory with no `..` after it.
fn check_glob_match(path: &Path, value_root: &Path, pattern: &str) -> anyhow::Result<()> {
    let inside = path
        .strip_prefix(value_root)
        .map(|rest| !rest.components().any(|c| matches!(c, std::path::Component::ParentDir)))
        .unwrap_or(false);
    if !inside {
        bail_loc!(
            "glob [{}] must stay inside its ANUBIS file's directory, but matched [{}]",
            pattern,
            path.display()
        );
    }
    Ok(())
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
                .map(|(k, v)| {
                    resolve_value_with_dir(v, value_root, vars, dir_relpath).map(|new_value| (k, new_value))
                })
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
                .map(|(k, v)| {
                    resolve_value_with_dir(v, value_root, vars, dir_relpath).map(|new_value| (k, new_value))
                })
                .collect::<anyhow::Result<HashMap<Identifier, Value>>>()?;
            Ok(Value::Map(new_map))
        }
        Value::Glob(glob) => {
            // Sorted: glob results feed archive/link command lines, which are
            // build-cache keys. A HashSet order changed every run, so every
            // glob-built library re-archived and everything above it relinked.
            let mut paths: std::collections::BTreeSet<String> = Default::default();

            for pattern in &glob.includes {
                check_relpath(pattern, "glob")?;
                let full_pattern = value_root.join(&pattern);
                let pattern_str = full_pattern
                    .to_str()
                    .ok_or_else(|| anyhow_loc!("Invalid UTF-8 in glob pattern: {:?}", full_pattern))?;
                for entry in glob::glob(pattern_str)
                    .with_context(|| format!("Failed to parse glob pattern: {}", pattern_str))?
                {
                    let path = match entry {
                        Ok(path) => path,
                        Err(e) => bail_loc!("Error matching glob pattern {}: {:?}", pattern_str, e),
                    };
                    check_glob_match(&path, value_root, pattern)?;
                    // Files only, like Bazel: `**/*` also matches directories,
                    // which no rule can use. Follows symlinks.
                    if !path.is_file() {
                        continue;
                    }
                    paths.insert(path.to_string_lossy().replace("\\", "/"));
                }
            }

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

            let paths: Vec<Utf8PathBuf> = paths
                .into_iter()
                .filter(|p| !excludes.iter().any(|e| e.matches(p)))
                .map(|p| Utf8PathBuf::from(p))
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
            check_relpath(&rel_path, "RelPath")?;
            let value_root_str = value_root.to_string_lossy();
            let mut abs_path = Utf8PathBuf::from(value_root_str.as_ref());
            abs_path.push(&rel_path);
            let abs_path = abs_path.slash_fix();
            tracing::trace!("Resolved RelPath [{:?}] -> [{:?}]", &rel_path, &abs_path);
            Ok(Value::Path(abs_path))
        }
        Value::RelPaths(rel_paths) => {
            let value_root_str = value_root.to_string_lossy();
            let mut abs_paths: Vec<Utf8PathBuf> = Default::default();
            for rel_path in rel_paths {
                check_relpath(&rel_path, "RelPaths")?;
                let mut abs_path = Utf8PathBuf::from(value_root_str.as_ref());
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
            // A specific filter wins over `default` wherever `default` is listed.
            let specific = s.filters.iter().position(|(filter, _)| match filter {
                Some(filter) => {
                    assert_eq!(s.inputs.len(), filter.len());
                    resolved_input.iter().enumerate().all(|(idx, input)| match &filter[idx] {
                        Some(valid_values) => valid_values.iter().any(|v| v == *input),
                        None => true,
                    })
                }
                None => false,
            });
            if let Some(i) = specific.or_else(|| s.filters.iter().position(|(filter, _)| filter.is_none())) {
                let v = s.filters.swap_remove(i).1;
                return resolve_value_with_dir(v, value_root, vars, dir_relpath);
            }
            // No filters matched - return Unresolved instead of error
            // This allows the target to remain unresolved until actually accessed
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
        Value::String(_) | Value::Integer(_) | Value::Bool(_) => Ok(value),
        Value::Target(ref t) => Ok(dir_relpath.map(|dir| Value::Target(t.resolve(dir))).unwrap_or(value)),
        Value::Targets(mut targets) => {
            if let Some(dir) = dir_relpath {
                let resolved: Vec<AnubisTarget> =
                    targets.into_iter().map(|t| dir_relpath.map(|dir| t.resolve(dir)).unwrap_or(t)).collect();
                return Ok(Value::Targets(resolved));
            } else {
                Ok(Value::Targets(targets))
            }
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
    let mut left = resolve_value_with_dir(left, value_root, vars, dir_relpath)?;
    let mut right = resolve_value_with_dir(right, value_root, vars, dir_relpath)?;

    // If either side is unresolved, propagate the unresolved state
    if let Some(info) = left.as_unresolved() {
        return Ok(Value::Unresolved(info.clone()));
    }
    if let Some(info) = right.as_unresolved() {
        return Ok(Value::Unresolved(info.clone()));
    }

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

            for (key, r) in right.fields {
                match left.fields.get_mut(&key) {
                    Some(l) => {
                        *l = resolve_concat_with_dir(
                            std::mem::replace(l, Value::Array(Vec::new())),
                            r,
                            value_root,
                            vars,
                            dir_relpath,
                        )?;
                    }
                    None => {
                        left.fields.insert(key, r);
                    }
                }
            }

            return Ok(Value::Object(std::mem::take(left)));
        }
        (Value::String(left), Value::Path(right)) => {
            let result = format!("{}{}", left, right);
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
    let mut lexer = PeekLexer {
        lexer,
        peeked: None,
        defs: Default::default(),
        error_span: None,
    };
    while lexer.peek() != &None {
        match parse_top_level(&mut lexer) {
            Ok(Some(object)) => objects.push(object),
            Ok(None) => {}
            Err(e) => {
                return Err(SpannedError {
                    error: e,
                    span: lexer.error_span.take().unwrap_or_else(|| lexer.lexer.span()),
                });
            }
        }
    }
    // A shared value nothing uses is most likely a mistake: a field left
    // outside its rule's parentheses, or a rule missing its type.
    if let Some((name, def)) = lexer.defs.iter().filter(|(_, d)| !d.used).min_by_key(|(_, d)| d.span.start) {
        return Err(SpannedError {
            error: anyhow_loc!(
                "Shared value [{}] is never used. If it is meant to be a field, it is outside its rule's parentheses",
                name
            ),
            span: def.span.clone(),
        });
    }
    Ok(Value::Array(objects))
}

/// One top-level item: an object (`rule_type(...)`, returned) or a shared
/// value (`name = value`, stored in `lexer.defs`).
fn parse_top_level<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Option<Value>> {
    let name = match lexer.next() {
        Some(Ok(Token::Identifier(name))) => name,
        Some(token) => bail_loc!(
            "Expected the name of a rule (`name(...)`) or of a shared value (`name = ...`). Found [{:?}]",
            token
        ),
        None => bail_loc!("parse_object: Ran out of tokens"),
    };
    let span = lexer.lexer.span();
    if !consume_token(lexer, &Token::Equals) {
        return parse_object_named(lexer, name).map(Some);
    }
    if lexer.defs.contains_key(name) {
        lexer.error_span = Some(span);
        bail_loc!("Shared value [{}] is defined twice in this file", name);
    }
    let value = parse_value(lexer)?;
    lexer.defs.insert(name.to_owned(), SharedValue { value, span, used: false });
    Ok(None)
}

pub fn parse_object<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    match lexer.next() {
        Some(Ok(Token::Identifier(obj_type))) => parse_object_named(lexer, obj_type),
        Some(token) => bail_loc!(
            "parse_object: Expected identifier token for new rule. Found [{:?}]",
            token
        ),
        None => bail_loc!("parse_object: Ran out of tokens"),
    }
}

/// The rest of an object whose type name has been read: `(field = value, ...)`.
fn parse_object_named<'src>(lexer: &mut PeekLexer<'src>, obj_type: &str) -> ParseResult<Value> {
    let mut fields: HashMap<Identifier, Value> = Default::default();
    expect_token(lexer, &Token::ParenOpen).map_err(|e| {
        anyhow_loc!(
            "parse_object: {}\n Error while parsing object [{:?}]",
            e,
            obj_type
        )
    })?;
    let obj = loop {
        if consume_token(lexer, &Token::ParenClose) {
            break Value::Object(Object {
                typename: obj_type.to_owned(),
                fields,
            });
        }
        let ident = expect_identifier(lexer)?;
        expect_token(lexer, &Token::Equals)?;
        let value = parse_value(lexer)?;
        fields.insert(ident, value);
        consume_token(lexer, &Token::Comma);
    };

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
    consume_token(lexer, &Token::Comma); // allow a trailing comma
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
    consume_token(lexer, &Token::Comma); // allow a trailing comma
    expect_token(lexer, &Token::ParenClose)?;
    consume_token(lexer, &Token::Comma);

    Ok(Value::RelPaths(paths))
}

pub fn parse_target<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    expect_token(lexer, &Token::Target)?;
    expect_token(lexer, &Token::ParenOpen)?;
    let s = expect_string(lexer)?;
    let t = AnubisTarget::new(&s)?;
    consume_token(lexer, &Token::Comma); // allow a trailing comma
    expect_token(lexer, &Token::ParenClose)?;
    consume_token(lexer, &Token::Comma);
    Ok(Value::Target(t))
}

pub fn parse_targets<'src>(lexer: &mut PeekLexer<'src>) -> ParseResult<Value> {
    let mut targets: Vec<AnubisTarget> = Default::default();

    expect_token(lexer, &Token::Targets)?;
    expect_token(lexer, &Token::ParenOpen)?;
    expect_token(lexer, &Token::BracketOpen)?;

    while lexer.peek() != &Some(Ok(Token::BracketClose)) {
        let s = expect_string(lexer)?;
        let t = AnubisTarget::new(&s)?;
        targets.push(t);
        consume_token(lexer, &Token::Comma);
    }

    expect_token(lexer, &Token::BracketClose)?;
    consume_token(lexer, &Token::Comma); // allow a trailing comma
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
        Some(Ok(Token::Integer(n))) => {
            let n = *n;
            lexer.next();
            Ok(Value::Integer(n))
        }
        Some(Ok(Token::Bool(b))) => {
            let b = *b;
            lexer.next();
            Ok(Value::Bool(b))
        }
        Some(Ok(Token::Glob)) => parse_glob(lexer),
        Some(Ok(Token::BraceOpen)) => parse_map(lexer),
        Some(Ok(Token::BracketOpen)) => parse_array(lexer),
        Some(Ok(Token::Select)) => parse_select(lexer),
        Some(Ok(Token::MultiSelect)) => parse_multi_select(lexer),
        Some(Ok(Token::Identifier(name))) => {
            // `name(` starts an object; a bare name is a shared value.
            let name: &'src str = name;
            let span = lexer.lexer.span();
            lexer.next();
            let paren = lexer.peek() == &Some(Ok(Token::ParenOpen));
            match lexer.defs.get_mut(name) {
                // Commas are optional, so `(a) = name` followed by `(b) = ...`
                // in a select would otherwise read as the object `name(b)`.
                Some(_) if paren => {
                    lexer.error_span = Some(span);
                    bail_loc!("Shared value [{}] is followed by `(`. Put a comma after it", name)
                }
                Some(def) => {
                    def.used = true;
                    Ok(def.value.clone())
                }
                None if paren => return parse_object_named(lexer, name),
                None => {
                    let mut known: Vec<&str> = lexer.defs.keys().map(|k| k.as_str()).collect();
                    known.sort();
                    lexer.error_span = Some(span);
                    bail_loc!(
                        "[{}] is not a shared value defined above this line (`{} = ...` at the top level of the file). Defined so far: [{}]",
                        name,
                        name,
                        known.join(", ")
                    )
                }
            }
        }
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
    consume_token(lexer, &Token::Comma); // allow a trailing comma
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
                bail_loc!(
                    "{}: Num inputs ({}) and num filters ({}) length must match.\nInputs: {:?}\nFilter: {:?}",
                    keyword,
                    inputs.len(),
                    select_filter.len(),
                    inputs,
                    select_filter
                )
            }
            maybe_select_filter = Some(select_filter);
        }
        expect_token(lexer, &Token::Equals)?;
        let value = parse_value(lexer)?;
        filters.push((maybe_select_filter, value));
        consume_token(lexer, &Token::Comma);
    }
    consume_token(lexer, &Token::Comma); // allow a trailing comma
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

/// A resolved value as JSON, for `anubis resolve`: objects are maps with
/// `"type"` = their typename, paths are relative to `root` with `/`, targets
/// are `//dir:name`. Map keys come out sorted; arrays keep their order.
/// Anything not fully resolved is an error.
pub fn to_json(value: &Value, root: &Utf8Path) -> anyhow::Result<serde_json::Value> {
    let root = format!("{}/", root.as_str().replace('\\', "/").trim_end_matches('/'));
    to_json_under(value, &root)
}

fn to_json_under(value: &Value, root: &str) -> anyhow::Result<serde_json::Value> {
    use serde_json::Value as J;
    let fields = |fields: &HashMap<Identifier, Value>| -> anyhow::Result<BTreeMap<String, J>> {
        fields.iter().map(|(k, v)| Ok((k.0.clone(), to_json_under(v, root)?))).collect()
    };
    let path = |p: &Utf8PathBuf| -> anyhow::Result<J> {
        let p = p.as_str().replace('\\', "/");
        match p.strip_prefix(root) {
            Some(rel) => Ok(J::String(rel.to_owned())),
            None => bail_loc!("path [{}] is not under the workspace root [{}]", p, root),
        }
    };
    Ok(match value {
        Value::String(s) => J::String(s.clone()),
        Value::Integer(n) => J::from(*n),
        Value::Bool(b) => J::Bool(*b),
        Value::Path(p) => path(p)?,
        Value::Paths(ps) => J::Array(ps.iter().map(path).collect::<anyhow::Result<_>>()?),
        Value::Target(t) => J::String(t.target_path().to_owned()),
        Value::Targets(ts) => J::Array(ts.iter().map(|t| J::String(t.target_path().to_owned())).collect()),
        Value::Array(vs) => {
            J::Array(vs.iter().map(|v| to_json_under(v, root)).collect::<anyhow::Result<_>>()?)
        }
        Value::Map(map) => J::Object(fields(map)?.into_iter().collect()),
        Value::Object(obj) => {
            let mut map = fields(&obj.fields)?;
            bail_loc_if!(
                map.contains_key("type"),
                "[{}] has a field named `type`, which `anubis resolve` uses for the typename",
                obj.typename
            );
            map.insert("type".to_owned(), J::String(obj.typename.clone()));
            J::Object(map.into_iter().collect())
        }
        Value::Unresolved(info) => bail_loc!("unresolved: {}", info.reason),
        Value::Concat(_)
        | Value::Glob(_)
        | Value::RelPath(_)
        | Value::RelPaths(_)
        | Value::Select(_)
        | Value::MultiSelect(_) => bail_loc!("not resolved: {:?}", value),
    })
}
