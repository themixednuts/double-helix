//! ACP protocol types.
//!
//! All types use camelCase serialization to match the ACP wire format.
//! Enum discriminator tags use snake_case as specified by the protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub type SessionId = String;
pub type ToolCallId = String;
pub type TerminalId = String;
pub type Meta = serde_json::Map<String, Value>;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<ClientSessionCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<ElicitationCapabilities>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<AgentSessionCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSessionCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<SessionConfigOptionsCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOptionsCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boolean: Option<BooleanConfigOptionCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BooleanConfigOptionCapabilities {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form: Option<ElicitationFormCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<ElicitationUrlCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationFormCapabilities {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationUrlCapabilities {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<SessionListCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<SessionDeleteCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<SessionResumeCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListCapabilities {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeleteCapabilities {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResumeCapabilities {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCaps {
    pub load_session: bool,
    pub list_sessions: bool,
    pub resume_session: bool,
    pub delete_session: bool,
    pub mcp: bool,
    pub auth: bool,
    pub config_options: bool,
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
    pub auth_methods: Option<Vec<AuthMethod>>,
}

impl InitializeResponse {
    #[must_use]
    pub fn caps(&self) -> AgentCaps {
        AgentCaps {
            load_session: self.agent_capabilities.load_session.unwrap_or(false),
            list_sessions: self
                .agent_capabilities
                .session
                .as_ref()
                .and_then(|session| session.list.as_ref())
                .is_some(),
            resume_session: self
                .agent_capabilities
                .session
                .as_ref()
                .and_then(|session| session.resume.as_ref())
                .is_some(),
            delete_session: self
                .agent_capabilities
                .session
                .as_ref()
                .and_then(|session| session.delete.as_ref())
                .is_some(),
            mcp: self.agent_capabilities.mcp.is_some(),
            auth: self.auth_methods.as_ref().is_some_and(|items| !items.is_empty()),
            config_options: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<PathBuf>,
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
    pub available_commands: Option<Vec<AvailableCommand>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub session_id: SessionId,
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<PathBuf>,
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
pub struct ResumeSessionRequest {
    pub session_id: SessionId,
    pub cwd: PathBuf,
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionResponse {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteSessionRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeleteSessionResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<EnvVariable>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ConfigOptionKind>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
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
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ConfigOptionKind {
    Select {
        current_value: ConfigOptionValueId,
        options: Vec<ConfigOptionValue>,
    },
    Boolean {
        current_value: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigOptionValueId {
    String(String),
    Object { value: String },
}

impl ConfigOptionValueId {
    #[must_use]
    pub fn into_string(self) -> String {
        match self {
            Self::String(value) | Self::Object { value } => value,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdateData {
    pub config_options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageUpdateData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandsUpdateData {
    pub available_commands: Vec<AvailableCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommand {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<AvailableCommandInput>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AvailableCommandInput {
    Unstructured {
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    Structured {
        #[serde(default)]
        arguments: Vec<AvailableCommandArgument>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
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
    #[serde(rename = "agent_thought_chunk")]
    AgentThoughtChunk(ContentChunk),
    #[serde(rename = "tool_call")]
    ToolCall(ToolCallInfo),
    #[serde(rename = "tool_call_update")]
    ToolCallUpdate(ToolCallUpdate),
    #[serde(rename = "config_option_update")]
    ConfigOptionUpdate(ConfigOptionUpdateData),
    #[serde(rename = "current_mode_update")]
    CurrentModeUpdate(CurrentModeUpdateData),
    #[serde(rename = "available_commands_update")]
    AvailableCommandsUpdate(AvailableCommandsUpdateData),
    #[serde(rename = "usage_update")]
    UsageUpdate(UsageUpdateData),
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
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: ToolCallId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    #[serde(default)]
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, alias = "options")]
    pub permissions: Vec<PermissionOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCallUpdate>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    #[serde(default, alias = "optionId")]
    pub id: String,
    #[serde(default, alias = "name")]
    pub title: String,
    #[serde(default)]
    pub kind: PermissionOptionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    AllowThread,
    RejectOnce,
    RejectAlways,
    #[default]
    Other,
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
    pub value: ConfigValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigValue {
    ValueId { value: String },
    Boolean { value: bool },
    Legacy(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionResponse {
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateRequest {
    pub method_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateResponse {
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LogoutRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LogoutResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthMethod {
    Agent(AuthMethodData),
    Terminal(AuthMethodData),
    EnvVar(AuthMethodData),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethodData {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<EnvVariable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variable: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateElicitationRequest {
    pub elicitation_id: String,
    pub session_id: Option<SessionId>,
    pub mode: ElicitationMode,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ElicitationMode {
    Form(ElicitationFormMode),
    Url(ElicitationUrlMode),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationFormMode {
    pub message: String,
    pub schema: ElicitationSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationUrlMode {
    pub message: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationSchema {
    #[serde(default)]
    pub fields: Vec<ElicitationField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationField {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: ElicitationFieldType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default)]
    pub options: Vec<ElicitationOption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationFieldType {
    Text,
    Select,
    Bool,
    Textarea,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateElicitationResponse {
    pub action: ElicitationAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ElicitationAction {
    Accept { content: Value },
    Decline,
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteElicitationNotification {
    pub elicitation_id: String,
}

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
    CreateElicitation(CreateElicitationRequest),
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
            crate::methods::ELICITATION_CREATE => Ok(AgentMethodCall::CreateElicitation(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            _ => Err(crate::Error::Unhandled(method.to_string())),
        }
    }
}
