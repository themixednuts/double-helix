use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::permission;
use crate::Editor;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Caps {
    pub fs: FsCaps,
    pub terminal: Option<TerminalCaps>,
    pub permission: PermissionCaps,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsCaps {
    pub read_text: bool,
    pub write_text: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalCaps;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PermissionCaps;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub path: PathBuf,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTerminal {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<Env>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalId(Arc<str>);

impl TerminalId {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for TerminalId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),
    Other,
}

#[derive(Debug, Clone)]
pub enum Fs {
    Local,
}

#[derive(Clone)]
pub enum Terminal {
    Local {
        inner: Arc<helix_acp::TerminalManager>,
    },
}

#[derive(Debug, Clone)]
pub enum Permission {
    Local,
}

#[derive(Debug, Clone)]
pub struct Set {
    pub fs: Fs,
    pub terminal: Option<Terminal>,
    pub permission: Permission,
}

pub fn local_set(editor: &Editor) -> Set {
    Set {
        fs: Fs::Local,
        terminal: Some(Terminal::Local {
            inner: editor.assistant_terminals(),
        }),
        permission: Permission::Local,
    }
}

impl fmt::Debug for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local { .. } => f.debug_struct("Terminal::Local").finish(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Fs {
    pub async fn read_text(&self, path: &Path) -> Result<String, Error> {
        match self {
            Self::Local => tokio::fs::read_to_string(path)
                .await
                .map_err(|err| Error::Other(err.into())),
        }
    }

    pub async fn write_text(&self, req: Write) -> Result<(), Error> {
        match self {
            Self::Local => {
                if let Some(parent) = req.path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|err| Error::Other(err.into()))?;
                }
                tokio::fs::write(&req.path, req.text)
                    .await
                    .map_err(|err| Error::Other(err.into()))
            }
        }
    }
}

impl Terminal {
    pub async fn create(&self, req: CreateTerminal) -> Result<TerminalId, Error> {
        match self {
            Self::Local { inner } => {
                let response = inner
                    .create(&helix_acp::types::CreateTerminalRequest {
                        command: req.command.display().to_string(),
                        args: Some(req.args),
                        env: Some(
                            req.env
                                .into_iter()
                                .map(|env| helix_acp::types::EnvVariable {
                                    name: env.key,
                                    value: env.value,
                                })
                                .collect(),
                        ),
                        cwd: req.cwd.map(|cwd| cwd.display().to_string()),
                        output_byte_limit: None,
                        session_id: "assistant".to_string(),
                    })
                    .await
                    .map_err(Error::Other)?;
                Ok(TerminalId::new(response.terminal_id))
            }
        }
    }

    pub async fn output(&self, id: &TerminalId) -> Result<String, Error> {
        match self {
            Self::Local { inner } => inner
                .output(&helix_acp::types::TerminalOutputRequest {
                    session_id: "assistant".to_string(),
                    terminal_id: id.to_string(),
                })
                .await
                .map(|out| out.output)
                .map_err(Error::Other),
        }
    }

    pub async fn wait(&self, id: &TerminalId) -> Result<ExitStatus, Error> {
        match self {
            Self::Local { inner } => {
                let out = inner
                    .wait_for_exit(&helix_acp::types::WaitForTerminalExitRequest {
                        session_id: "assistant".to_string(),
                        terminal_id: id.to_string(),
                    })
                    .await
                    .map_err(Error::Other)?;
                Ok(match out.exit_code {
                    Some(code) => ExitStatus::Code(code),
                    None => ExitStatus::Other,
                })
            }
        }
    }

    pub async fn kill(&self, id: &TerminalId) -> Result<(), Error> {
        match self {
            Self::Local { inner } => inner
                .kill(&helix_acp::types::KillTerminalRequest {
                    session_id: "assistant".to_string(),
                    terminal_id: id.to_string(),
                })
                .await
                .map(|_| ())
                .map_err(Error::Other),
        }
    }

    pub async fn release(&self, id: &TerminalId) -> Result<(), Error> {
        match self {
            Self::Local { inner } => inner
                .release(&helix_acp::types::ReleaseTerminalRequest {
                    session_id: "assistant".to_string(),
                    terminal_id: id.to_string(),
                })
                .await
                .map(|_| ())
                .map_err(Error::Other),
        }
    }
}

impl Permission {
    pub async fn request(&self, _req: permission::Request) -> Result<permission::Decision, Error> {
        match self {
            Self::Local => Ok(permission::Decision::Dismiss),
        }
    }
}
