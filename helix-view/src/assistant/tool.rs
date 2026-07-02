use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Id(Arc<str>);

impl Id {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Pending,
    Running,
    Completed,
    Failed { message: Option<String> },
    Canceled,
    Unknown(Arc<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub id: Id,
    pub name: String,
    pub state: State,
    pub output: String,
    pub subagent: Option<SubagentSessionInfo>,
    pub sandbox: Option<SandboxAuthorization>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSessionInfo {
    pub session_id: String,
    pub message_start_index: Option<u64>,
    pub message_end_index: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxAuthorization {
    pub command: Option<String>,
    pub network_hosts: Vec<String>,
    pub allow_fs_write_all: bool,
    pub write_paths: Vec<PathBuf>,
    pub unsandboxed: bool,
    pub reason: Option<String>,
}
