//! Cancellable asynchronous host operations.

use serde::{Deserialize, Serialize};

use super::{DocumentHandle, DynamicValue};
use crate::requests::{OpenDocumentRequest, RunCommandRequest};
use crate::snapshots::Position;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntaxQueryRequest {
    pub document: DocumentHandle,
    pub query: String,
    pub start: Option<Position>,
    pub end: Option<Position>,
    pub max_captures: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntaxCapture {
    pub name: String,
    pub kind: String,
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspCallRequest {
    pub document: DocumentHandle,
    pub server: Option<String>,
    pub method: String,
    pub params: DynamicValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginTaskRequest {
    OpenDocument(OpenDocumentRequest),
    RunCommand(RunCommandRequest),
    SyntaxQuery(SyntaxQueryRequest),
    LspCall(LspCallRequest),
    SetTheme(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginTaskResult {
    Unit,
    Document(DocumentHandle),
    SyntaxCaptures(Vec<SyntaxCapture>),
    Value(DynamicValue),
}
