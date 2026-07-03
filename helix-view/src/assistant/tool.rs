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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentJumpTarget {
    Existing {
        thread: super::thread::Id,
        message_start_index: Option<u64>,
        message_end_index: Option<u64>,
    },
    LoadRemote {
        message_start_index: Option<u64>,
        message_end_index: Option<u64>,
    },
    Unsupported,
}

#[must_use]
pub fn resolve_subagent_jump(
    info: &SubagentSessionInfo,
    known_sessions: impl IntoIterator<Item = (String, super::thread::Id)>,
    can_load_remote: bool,
) -> SubagentJumpTarget {
    if let Some((_, thread)) = known_sessions
        .into_iter()
        .find(|(session, _)| session == &info.session_id)
    {
        return SubagentJumpTarget::Existing {
            thread,
            message_start_index: info.message_start_index,
            message_end_index: info.message_end_index,
        };
    }
    if can_load_remote {
        SubagentJumpTarget::LoadRemote {
            message_start_index: info.message_start_index,
            message_end_index: info.message_end_index,
        }
    } else {
        SubagentJumpTarget::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    fn thread(value: u64) -> super::super::thread::Id {
        super::super::thread::Id::new(NonZeroU64::new(value).unwrap())
    }

    #[test]
    fn resolves_known_subagent_session_to_existing_thread() {
        let info = SubagentSessionInfo {
            session_id: "remote-1".to_string(),
            message_start_index: Some(2),
            message_end_index: Some(5),
        };

        assert_eq!(
            resolve_subagent_jump(&info, [("remote-1".to_string(), thread(7))], true),
            SubagentJumpTarget::Existing {
                thread: thread(7),
                message_start_index: Some(2),
                message_end_index: Some(5),
            }
        );
    }

    #[test]
    fn resolves_unknown_subagent_session_to_load_when_supported() {
        let info = SubagentSessionInfo {
            session_id: "remote-2".to_string(),
            message_start_index: None,
            message_end_index: None,
        };

        assert_eq!(
            resolve_subagent_jump(
                &info,
                Vec::<(String, super::super::thread::Id)>::new(),
                true
            ),
            SubagentJumpTarget::LoadRemote {
                message_start_index: None,
                message_end_index: None,
            }
        );
    }
}
