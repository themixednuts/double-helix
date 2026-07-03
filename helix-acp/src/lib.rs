pub mod client;
pub mod jsonrpc;
pub mod registry;
pub mod terminal;
pub mod transport;
pub mod types;

pub use client::AcpAgent;
pub use registry::Registry;
pub use terminal::TerminalManager;
pub use types::*;

use slotmap::new_key_type;

new_key_type! {
    pub struct AgentId;
}

/// ACP protocol version
pub const PROTOCOL_VERSION: u32 = 1;

/// JSON-RPC method names used by the Agent Client Protocol.
pub mod methods {
    // Client -> Agent requests
    pub const INITIALIZE: &str = "initialize";
    pub const AUTHENTICATE: &str = "authenticate";
    pub const LOGOUT: &str = "logout";
    pub const SESSION_LIST: &str = "session/list";
    pub const SESSION_RESUME: &str = "session/resume";
    pub const SESSION_DELETE: &str = "session/delete";
    pub const ELICITATION_CREATE: &str = "elicitation/create";
    pub const ELICITATION_COMPLETE: &str = "elicitation/complete";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_LOAD: &str = "session/load";
    pub const SESSION_FORK: &str = "session/fork";
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_SET_MODE: &str = "session/set_mode";
    pub const SESSION_SET_CONFIG: &str = "session/set_config_option";

    // Client -> Agent notifications
    pub const SESSION_CANCEL: &str = "session/cancel";

    // Agent -> Client requests
    pub const FS_READ_TEXT_FILE: &str = "fs/read_text_file";
    pub const FS_WRITE_TEXT_FILE: &str = "fs/write_text_file";
    pub const TERMINAL_CREATE: &str = "terminal/create";
    pub const TERMINAL_OUTPUT: &str = "terminal/output";
    pub const TERMINAL_WAIT_FOR_EXIT: &str = "terminal/wait_for_exit";
    pub const TERMINAL_KILL: &str = "terminal/kill";
    pub const TERMINAL_RELEASE: &str = "terminal/release";
    pub const REQUEST_PERMISSION: &str = "session/request_permission";

    // Agent -> Client notifications
    pub const SESSION_UPDATE: &str = "session/update";
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("stream closed")]
    StreamClosed,
    #[error("request {0} timed out")]
    Timeout(jsonrpc::Id),
    #[error("io error: {0}")]
    IO(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("agent error: {0}")]
    AgentError(jsonrpc::Error),
    #[error("unhandled method: {0}")]
    Unhandled(String),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = core::result::Result<T, Error>;
