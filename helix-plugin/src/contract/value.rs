//! A dynamically-typed value for plugin communication.
//!
//! [`DynamicValue`] replaces `serde_json::Value` in the callback/response path.
//! It is deliberately minimal — only the types that actually flow through
//! the plugin boundary. Unlike `serde_json::Value`, it does not carry
//! arrays or nested objects; those are handled by typed contract types.

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

/// Convert from `Option<String>` — `None` becomes `Nil`, `Some` becomes `String`.
impl From<Option<String>> for DynamicValue {
    fn from(opt: Option<String>) -> Self {
        match opt {
            Some(s) => Self::String(s),
            None => Self::Nil,
        }
    }
}
