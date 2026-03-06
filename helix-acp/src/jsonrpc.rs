//! JSON-RPC 2.0 types for the Agent Client Protocol.
//!
//! Adapted from helix-lsp's jsonrpc module. The wire format is identical
//! (standard JSON-RPC 2.0), only the transport framing differs:
//! ACP uses newline-delimited messages instead of Content-Length headers.

use serde::de::{self, DeserializeOwned, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    ServerError(i64),
}

impl ErrorCode {
    pub fn code(&self) -> i64 {
        match *self {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::InternalError => -32603,
            ErrorCode::ServerError(code) => code,
        }
    }
}

impl From<i64> for ErrorCode {
    fn from(code: i64) -> Self {
        match code {
            -32700 => ErrorCode::ParseError,
            -32600 => ErrorCode::InvalidRequest,
            -32601 => ErrorCode::MethodNotFound,
            -32602 => ErrorCode::InvalidParams,
            -32603 => ErrorCode::InternalError,
            code => ErrorCode::ServerError(code),
        }
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let code: i64 = Deserialize::deserialize(deserializer)?;
        Ok(ErrorCode::from(code))
    }
}

impl Serialize for ErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i64(self.code())
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Error {
            code: ErrorCode::InvalidParams,
            message: message.into(),
            data: None,
        }
    }

    pub fn method_not_found(method: impl Into<String>) -> Self {
        Error {
            code: ErrorCode::MethodNotFound,
            message: method.into(),
            data: None,
        }
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Error {
            code: ErrorCode::InternalError,
            message: message.into(),
            data: None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// JSON-RPC request ID.
#[derive(Debug, PartialEq, Eq, Clone, Hash, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Id {
    Null,
    Num(u64),
    Str(String),
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Id::Null => f.write_str("null"),
            Id::Num(num) => write!(f, "{}", num),
            Id::Str(string) => f.write_str(string),
        }
    }
}

/// JSON-RPC protocol version.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Version {
    V2,
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match *self {
            Version::V2 => serializer.serialize_str("2.0"),
        }
    }
}

struct VersionVisitor;

impl Visitor<'_> for VersionVisitor {
    type Value = Version;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a string")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        match value {
            "2.0" => Ok(Version::V2),
            _ => Err(de::Error::custom("invalid version")),
        }
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_identifier(VersionVisitor)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Params {
    None,
    Array(Vec<Value>),
    Map(serde_json::Map<String, Value>),
}

impl Params {
    pub fn parse<D: DeserializeOwned>(self) -> std::result::Result<D, Error> {
        let value: Value = self.into();
        serde_json::from_value(value)
            .map_err(|err| Error::invalid_params(format!("Invalid params: {}.", err)))
    }

    pub fn is_none(&self) -> bool {
        self == &Params::None
    }
}

impl From<Params> for Value {
    fn from(params: Params) -> Value {
        match params {
            Params::Array(vec) => Value::Array(vec),
            Params::Map(map) => Value::Object(map),
            Params::None => Value::Null,
        }
    }
}

fn default_params() -> Params {
    Params::None
}

fn default_id() -> Id {
    Id::Null
}

/// A JSON-RPC request (has id, expects response).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MethodCall {
    pub jsonrpc: Option<Version>,
    pub method: String,
    #[serde(default = "default_params", skip_serializing_if = "Params::is_none")]
    pub params: Params,
    pub id: Id,
}

/// A JSON-RPC notification (no id, no response expected).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Notification {
    pub jsonrpc: Option<Version>,
    pub method: String,
    #[serde(default = "default_params", skip_serializing_if = "Params::is_none")]
    pub params: Params,
}

/// An incoming call from the agent: either a request or notification.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Call {
    MethodCall(MethodCall),
    Notification(Notification),
    Invalid {
        #[serde(default = "default_id")]
        id: Id,
    },
}

/// A successful JSON-RPC response.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct Success {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub result: Value,
    pub id: Id,
}

/// A failed JSON-RPC response.
#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Serialize)]
pub struct Failure {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub error: Error,
    pub id: Id,
}

/// A JSON-RPC response (success or failure).
#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Output {
    Failure(Failure),
    Success(Success),
}

impl From<Output> for std::result::Result<Value, Error> {
    fn from(output: Output) -> Self {
        match output {
            Output::Success(success) => Ok(success.result),
            Output::Failure(failure) => Err(failure.error),
        }
    }
}
