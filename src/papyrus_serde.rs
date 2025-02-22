#![allow(dead_code)]

use heck::ToUpperCamelCase;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use std::fmt;

use crate::{Identifier, Value};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug)]
pub enum DeserializeError {
    ExpectedArray,
    ExpectedMap(Value),
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
            DeserializeError::ExpectedMap(v) => write!(f, "expected map. found [{:?}]", v),
            DeserializeError::ExpectedString => write!(f, "expected string"),
            DeserializeError::Unresolved(msg) => write!(f, "unresolved value: {}", msg),
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
            Value::String(s) => visitor.visit_string(s.clone()),
            Value::Array(arr) => visitor.visit_seq(ArrayDeserializer {
                iter: arr.clone().into_iter(),
            }),
            Value::Object(obj) => {
                visitor.visit_map(ObjectDeserializer::new(obj.typename.clone(), obj.fields.clone()))
            }
            Value::Map(map) => visitor.visit_map(MapDeserializer::new(map.clone())),
            Value::Path(path) => {
                let path_str = path
                    .to_str()
                    .ok_or_else(|| DeserializeError::Custom("Invalid UTF-8 in path".to_string()))?;
                visitor.visit_string(path_str.to_owned())
            }
            Value::Paths(paths) => visitor.visit_seq(PathsSeqDeserializer {
                iter: paths.clone().into_iter(),
            }),
            Value::Glob(g) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize unresolved glob: {:?}",
                g
            ))),
            Value::Select(s) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize unresolved select: {:?}",
                s
            ))),
            Value::Concat(c) => Err(DeserializeError::Unresolved(format!(
                "Can't deserialize unresolved concat: {:?}",
                c
            ))),
        }
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.value {
            Value::String(s) => visitor.visit_string(s.clone()),
            _ => Err(DeserializeError::ExpectedString),
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

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
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
