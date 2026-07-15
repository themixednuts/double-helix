//! A dynamically-typed value for plugin communication.
//!
//! [`DynamicValue`] replaces `serde_json::Value` in the callback/response path.
//! It preserves the JSON data model without coupling the contract to a
//! particular language host or wire codec.

use serde::{Deserialize, Serialize};

/// A dynamically-typed value for plugin UI responses and lightweight data.
///
/// Used as the response type for prompt (String), confirm (Bool), and picker
/// (String) results. Also serves as the wire format for simple values in
/// future RPC transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DynamicValue {
    /// No value (e.g. cancelled prompt).
    Nil,
    /// Boolean (e.g. confirm response).
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Floating-point.
    Float(f64),
    /// String (e.g. prompt input, picker selection).
    String(String),
    Array(Vec<DynamicValue>),
    Object(std::collections::BTreeMap<String, DynamicValue>),
}

impl DynamicValue {
    /// Returns `true` if this is `Nil`.
    pub const fn is_nil(&self) -> bool {
        matches!(self, Self::Nil)
    }

    /// Try to extract a string reference.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Try to extract a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Try to extract an integer.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            _ => None,
        }
    }
}

impl From<String> for DynamicValue {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&str> for DynamicValue {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}

impl From<bool> for DynamicValue {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

impl From<i64> for DynamicValue {
    fn from(n: i64) -> Self {
        Self::Int(n)
    }
}

impl From<()> for DynamicValue {
    fn from((): ()) -> Self {
        Self::Nil
    }
}

impl TryFrom<serde_json::Value> for DynamicValue {
    type Error = String;

    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        Ok(match value {
            serde_json::Value::Null => Self::Nil,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(value) if value.is_i64() => Self::Int(
                value
                    .as_i64()
                    .ok_or_else(|| "JSON integer is not representable".to_string())?,
            ),
            serde_json::Value::Number(value) => Self::Float(
                value
                    .as_f64()
                    .ok_or_else(|| "JSON number is not representable".to_string())?,
            ),
            serde_json::Value::String(value) => Self::String(value),
            serde_json::Value::Array(values) => Self::Array(
                values
                    .into_iter()
                    .map(Self::try_from)
                    .collect::<Result<_, _>>()?,
            ),
            serde_json::Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| Ok((key, Self::try_from(value)?)))
                    .collect::<Result<_, String>>()?,
            ),
        })
    }
}

impl TryFrom<DynamicValue> for serde_json::Value {
    type Error = String;

    fn try_from(value: DynamicValue) -> Result<Self, Self::Error> {
        Ok(match value {
            DynamicValue::Nil => Self::Null,
            DynamicValue::Bool(value) => Self::Bool(value),
            DynamicValue::Int(value) => value.into(),
            DynamicValue::Float(value) => Self::Number(
                serde_json::Number::from_f64(value)
                    .ok_or_else(|| "floating-point value must be finite".to_string())?,
            ),
            DynamicValue::String(value) => Self::String(value),
            DynamicValue::Array(values) => Self::Array(
                values
                    .into_iter()
                    .map(Self::try_from)
                    .collect::<Result<_, _>>()?,
            ),
            DynamicValue::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| Ok((key, Self::try_from(value)?)))
                    .collect::<Result<_, String>>()?,
            ),
        })
    }
}

/// Convert from `Option<String>` — `None` becomes `Nil`, `Some` becomes `String`.
impl From<Option<String>> for DynamicValue {
    fn from(opt: Option<String>) -> Self {
        match opt {
            Some(s) => Self::String(s),
            None => Self::Nil,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_values_are_rejected_instead_of_becoming_null() {
        let error = serde_json::Value::try_from(DynamicValue::Float(f64::NAN)).unwrap_err();
        assert!(error.contains("finite"));
    }

    #[test]
    fn nested_values_round_trip_through_json() {
        let value = DynamicValue::Object(std::collections::BTreeMap::from([(
            "items".into(),
            DynamicValue::Array(vec![DynamicValue::Int(7), DynamicValue::Bool(true)]),
        )]));
        let json = serde_json::Value::try_from(value.clone()).unwrap();
        assert_eq!(DynamicValue::try_from(json).unwrap(), value);
    }
}
