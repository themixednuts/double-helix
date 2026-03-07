//! ACP protocol types.
//!
//! All types use camelCase serialization to match the ACP wire format.
//! Enum discriminator tags use snake_case as specified by the protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub type SessionId = String;
pub type ToolCallId = String;
pub type TerminalId = String;

// ---------------------------------------------------------------------------
// Common types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs: Option<FileSystemCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_text_file: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_text_file: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_capabilities: Option<PromptCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedded_context: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sse: Option<bool>,
}

// ---------------------------------------------------------------------------
// Initialize
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Implementation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u32,
    pub agent_capabilities: AgentCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<Implementation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_methods: Option<Vec<Value>>,
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_modes: Option<Vec<SessionMode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_commands: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionResponse {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_modes: Option<Vec<SessionMode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMode {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(rename = "type", default)]
    pub option_type: String,
    #[serde(default)]
    pub current_value: String,
    #[serde(default)]
    pub options: Vec<ConfigOptionValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionValue {
    pub value: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdateData {
    pub config_options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModeUpdateData {
    pub mode_id: String,
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: SessionId,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    pub session_id: SessionId,
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text(TextContent),
    #[serde(rename = "image")]
    Image(ImageContent),
    #[serde(rename = "audio")]
    Audio(AudioContent),
    #[serde(rename = "resource_link")]
    ResourceLink(ResourceLink),
    #[serde(rename = "resource")]
    Resource(EmbeddedResource),
}

impl From<String> for ContentBlock {
    fn from(text: String) -> Self {
        ContentBlock::Text(TextContent { text })
    }
}

impl From<&str> for ContentBlock {
    fn from(text: &str) -> Self {
        ContentBlock::Text(TextContent {
            text: text.to_string(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioContent {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLink {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedResource {
    pub uri: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

// ---------------------------------------------------------------------------
// Session updates (Agent -> Client notifications)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: SessionId,
    pub update: SessionUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate")]
pub enum SessionUpdate {
    #[serde(rename = "plan")]
    Plan(Plan),
    #[serde(rename = "agent_message_chunk")]
    AgentMessageChunk(ContentChunk),
    #[serde(rename = "tool_call")]
    ToolCall(ToolCallInfo),
    #[serde(rename = "tool_call_update")]
    ToolCallUpdate(ToolCallUpdate),
    #[serde(rename = "config_option_update")]
    ConfigOptionUpdate(ConfigOptionUpdateData),
    #[serde(rename = "current_mode_update")]
    CurrentModeUpdate(CurrentModeUpdateData),
    #[serde(rename = "available_commands_update")]
    AvailableCommandsUpdate(Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentChunk {
    pub content: ContentBlock,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<PlanEntryPriority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<PlanEntryStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

// ---------------------------------------------------------------------------
// Tool calls
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallInfo {
    pub tool_call_id: ToolCallId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    pub status: ToolCallStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: ToolCallId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Write,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

// ---------------------------------------------------------------------------
// Agent -> Client requests: File system
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadTextFileRequest {
    pub session_id: SessionId,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadTextFileResponse {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteTextFileRequest {
    pub session_id: SessionId,
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteTextFileResponse {}

// ---------------------------------------------------------------------------
// Agent -> Client requests: Terminal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTerminalRequest {
    pub session_id: SessionId,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<EnvVariable>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_byte_limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTerminalResponse {
    pub terminal_id: TerminalId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVariable {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutputRequest {
    pub session_id: SessionId,
    pub terminal_id: TerminalId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutputResponse {
    pub output: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<TerminalExitStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalExitStatus {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitForTerminalExitRequest {
    pub session_id: SessionId,
    pub terminal_id: TerminalId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitForTerminalExitResponse {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KillTerminalRequest {
    pub session_id: SessionId,
    pub terminal_id: TerminalId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillTerminalResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseTerminalRequest {
    pub session_id: SessionId,
    pub terminal_id: TerminalId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseTerminalResponse {}

// ---------------------------------------------------------------------------
// Agent -> Client requests: Permissions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: SessionId,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub permissions: Vec<PermissionOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionResponse {
    pub outcome: RequestPermissionOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RequestPermissionOutcome {
    #[serde(rename = "selected")]
    Selected { id: String },
    #[serde(rename = "dismissed")]
    Dismissed,
}

// ---------------------------------------------------------------------------
// Session mode / config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModeRequest {
    pub session_id: SessionId,
    pub mode_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetSessionModeResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionRequest {
    pub session_id: SessionId,
    pub config_id: String,
    #[serde(rename = "value")]
    pub value_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetSessionConfigOptionResponse {}

// ---------------------------------------------------------------------------
// Parsed incoming messages from the agent
// ---------------------------------------------------------------------------

/// Parsed notification from the agent.
pub enum AgentNotification {
    SessionUpdate(SessionNotification),
}

impl AgentNotification {
    pub fn parse(
        method: &str,
        params: crate::jsonrpc::Params,
    ) -> std::result::Result<Self, crate::Error> {
        match method {
            crate::methods::SESSION_UPDATE => {
                let notif: SessionNotification =
                    params.parse().map_err(crate::Error::AgentError)?;
                Ok(AgentNotification::SessionUpdate(notif))
            }
            _ => Err(crate::Error::Unhandled(method.to_string())),
        }
    }
}

/// Parsed request from the agent (expects a response).
pub enum AgentMethodCall {
    ReadTextFile(ReadTextFileRequest),
    WriteTextFile(WriteTextFileRequest),
    CreateTerminal(CreateTerminalRequest),
    TerminalOutput(TerminalOutputRequest),
    WaitForTerminalExit(WaitForTerminalExitRequest),
    KillTerminal(KillTerminalRequest),
    ReleaseTerminal(ReleaseTerminalRequest),
    RequestPermission(RequestPermissionRequest),
}

impl AgentMethodCall {
    pub fn parse(
        method: &str,
        params: crate::jsonrpc::Params,
    ) -> std::result::Result<Self, crate::Error> {
        match method {
            crate::methods::FS_READ_TEXT_FILE => Ok(AgentMethodCall::ReadTextFile(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::FS_WRITE_TEXT_FILE => Ok(AgentMethodCall::WriteTextFile(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_CREATE => Ok(AgentMethodCall::CreateTerminal(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_OUTPUT => Ok(AgentMethodCall::TerminalOutput(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_WAIT_FOR_EXIT => Ok(AgentMethodCall::WaitForTerminalExit(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_KILL => Ok(AgentMethodCall::KillTerminal(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_RELEASE => Ok(AgentMethodCall::ReleaseTerminal(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::REQUEST_PERMISSION => Ok(AgentMethodCall::RequestPermission(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            _ => Err(crate::Error::Unhandled(method.to_string())),
        }
    }
}
