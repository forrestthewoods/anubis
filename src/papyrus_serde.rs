#![allow(dead_code)]

use heck::ToUpperCamelCase;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use std::fmt;

use crate::anubis::AnubisTarget;
use crate::{Identifier, UnresolvedInfo, Value};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug)]
pub enum DeserializeError {
    ExpectedArray,
    ExpectedMap(Value),
    ExpectedString(Value),
    /// Error for when a String value was used where a Target value was expected.
    /// This helps catch typos like `deps = ["//lib:foo"]` instead of `deps = [Target("//lib:foo")]`
    ExpectedTarget(Value),
    /// Error for values that could not be resolved (e.g., select() with no matching filter)
    Unresolved(String),
    /// Error for values explicitly marked as unresolved with diagnostic info
    UnresolvedValue(UnresolvedInfo),
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
            DeserializeError::ExpectedMap(v) => write!(f, "expected map. found [{:?}]", v),
            DeserializeError::ExpectedString(v) => write!(f, "expected string. found [{:?}]", v),
            DeserializeError::ExpectedTarget(v) => {
                write!(
                    f,
                    "expected Target value (use Target(\"...\") syntax), but found String. \
                    Use Target(\"//path:name\") instead of \"//path:name\". found [{:?}]",
                    v
                )
            }
            DeserializeError::Unresolved(msg) => write!(f, "unresolved value: {}", msg),
            DeserializeError::UnresolvedValue(info) => {
                write!(
                    f,
                    "unresolved value: {}\n  Select inputs: {:?}\n  Actual values: {:?}\n  Available filters: {:?}",
                    info.reason, info.select_inputs, info.select_values, info.available_filters
                )
            }
            DeserializeError::Custom(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for DeserializeError {}

pub struct ValueDeserializer<'a> {
    value: &'a Value,
}

impl<'a> ValueDeserializer<'a> {
    pub fn new(value: &'a Value) -> Self {
        ValueDeserializer { value }
    }
}

impl<'de, 'a> Deserializer<'de> for ValueDeserializer<'a> {
    type Error = DeserializeError;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.value {
            Value::Path(p) => visitor.visit_string(p.to_string_lossy().to_string()),
            Value::String(s) => visitor.visit_str(s),
            Value::Array(arr) => visitor.visit_seq(ArrayDeserializer {
                iter: arr.clone().into_iter(),
            }),
            Value::Object(obj) => {
                visitor.visit_map(ObjectDeserializer::new(obj.typename.clone(), obj.fields.clone()))
            }
            Value::Map(map) => visitor.visit_map(MapDeserializer::new(map.clone())),
            Value::Paths(paths) => visitor.visit_seq(PathsSeqDeserializer {
                iter: paths.clone().into_iter(),
            }),
            Value::RelPath(p) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize unresolved RelPath: {:?}",
                p
            ))),
            Value::RelPaths(p) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize unresolved RelPaths: {:?}",
                p
            ))),
            Value::Glob(g) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize unresolved glob: {:?}",
                g
            ))),
            Value::Select(s) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize unresolved select: {:?}",
                s
            ))),
            Value::MultiSelect(s) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize unresolved multi_select: {:?}",
                s
            ))),
            Value::Concat(c) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize unresolved concat: {:?}",
                c
            ))),
            Value::Target(target) => visitor.visit_str(target.target_path()),
            Value::Targets(targets) => visitor.visit_seq(TargetsSeqDeserializer {
                iter: targets.clone().into_iter(),
            }),
            Value::Unresolved(info) => Err(DeserializeError::UnresolvedValue(info.clone())),
        }
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.value {
            Value::Path(p) => visitor.visit_string(p.to_string_lossy().to_string()),
            Value::String(s) => visitor.visit_string(s.clone()),
            _ => Err(DeserializeError::ExpectedString(self.value.clone())),
        }
    }

    fn deserialize_struct<V>(
        self,
        name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.value {
            Value::Object(obj) => {
                if obj.typename.to_upper_camel_case() != name {
                    Err(DeserializeError::Custom(format!(
                        "Type mismatch: expected typename `{}`, got `{}`",
                        name, obj.typename
                    )))
                } else {
                    visitor.visit_map(ObjectDeserializer::new(obj.typename.clone(), obj.fields.clone()))
                }
            }
            Value::Map(map) => visitor.visit_map(MapDeserializer::new(map.clone())),
            v => Err(DeserializeError::ExpectedMap(v.clone())),
        }
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        // If we're here, the value exists, so it's Some(...)
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V>(self, name: &'static str, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        // Special handling for AnubisTarget: only accept Value::Target, not Value::String
        if name == "AnubisTarget" {
            match self.value {
                Value::Target(target) => visitor.visit_str(target.target_path()),
                // Reject strings that look like targets - they should use Target("...") syntax
                Value::String(_) => Err(DeserializeError::ExpectedTarget(self.value.clone())),
                _ => Err(DeserializeError::ExpectedTarget(self.value.clone())),
            }
        } else {
            // For other newtype structs, forward to deserialize_any
            self.deserialize_any(visitor)
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str
        bytes byte_buf unit unit_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}

pub struct ArrayDeserializer {
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
                let deserializer = ValueDeserializer::new(&value);
                seed.deserialize(deserializer).map(Some)
            }
            None => Ok(None),
        }
    }
}

pub struct ObjectDeserializer {
    typename: String,
    iter: std::collections::hash_map::IntoIter<Identifier, Value>,
    next_value: Option<Value>,
}

impl ObjectDeserializer {
    pub fn new(typename: String, map: HashMap<Identifier, Value>) -> Self {
        ObjectDeserializer {
            typename,
            iter: map.into_iter(),
            next_value: None,
        }
    }
}

impl<'de> MapAccess<'de> for ObjectDeserializer {
    type Error = DeserializeError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: de::DeserializeSeed<'de>,
    {
        match self.iter.next() {
            Some((key, value)) => {
                self.next_value = Some(value);
                let key_string = Value::String(key.0.clone());
                let key_deserializer = ValueDeserializer::new(&key_string);
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
                let value_deserializer = ValueDeserializer::new(&value);
                seed.deserialize(value_deserializer)
            }
            None => Err(DeserializeError::Custom("value missing".to_string())),
        }
    }
}

pub struct MapDeserializer {
    iter: std::collections::hash_map::IntoIter<Identifier, Value>,
    next_value: Option<Value>,
}

impl MapDeserializer {
    pub fn new(map: HashMap<Identifier, Value>) -> Self {
        MapDeserializer {
            iter: map.into_iter(),
            next_value: None,
        }
    }
}

impl<'de> MapAccess<'de> for MapDeserializer {
    type Error = DeserializeError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: de::DeserializeSeed<'de>,
    {
        match self.iter.next() {
            Some((key, value)) => {
                self.next_value = Some(value);
                let key_string = Value::String(key.0.clone());
                let key_deserializer = ValueDeserializer::new(&key_string);
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
                let value_deserializer = ValueDeserializer::new(&value);
                seed.deserialize(value_deserializer)
            }
            None => Err(DeserializeError::Custom("value missing".to_string())),
        }
    }
}

pub struct PathsSeqDeserializer {
    iter: std::vec::IntoIter<PathBuf>,
}

impl<'de> SeqAccess<'de> for PathsSeqDeserializer {
    type Error = DeserializeError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        match self.iter.next() {
            Some(path) => {
                let s = path
                    .to_str()
                    .ok_or_else(|| DeserializeError::Custom("Invalid UTF-8 in path".to_owned()))?;
                let path_string = Value::String(s.to_owned());
                let deserializer = ValueDeserializer::new(&path_string);
                seed.deserialize(deserializer).map(Some)
            }
            None => Ok(None),
        }
    }
}

pub struct TargetsSeqDeserializer {
    iter: std::vec::IntoIter<AnubisTarget>,
}

impl<'de> SeqAccess<'de> for TargetsSeqDeserializer {
    type Error = DeserializeError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        match self.iter.next() {
            Some(target) => {
                let target_value = Value::Target(target);
                let deserializer = ValueDeserializer::new(&target_value);
                seed.deserialize(deserializer).map(Some)
            }
            None => Ok(None),
        }
    }
}
