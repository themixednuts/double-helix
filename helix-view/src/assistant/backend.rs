use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use url::Url;

use super::{auth, config, history, host, mode, permission, prompt, review, terminal, thread};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Kind(Cow<'static, str>);

impl Kind {
    #[must_use]
    pub const fn core(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    #[must_use]
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }
}

pub mod kind {
    use super::Kind;

    pub const ACP: Kind = Kind::core("acp");
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Remote(Arc<str>);

impl Remote {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Remote {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextServer {
    pub id: Arc<str>,
    pub transport: ContextTransport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextTransport {
    Http(Url),
    Sse(Url),
    Stdio {
        command: std::path::PathBuf,
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub scope: thread::Scope,
    pub context_servers: Vec<ContextServer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Caps {
    pub load_thread: bool,
    pub close_thread: bool,
    pub history: Option<history::Caps>,
    pub mode: Option<mode::Caps>,
    pub config: Option<config::Caps>,
    pub prompt: prompt::Caps,
    pub host: host::Caps,
}

pub trait Driver: Send + Sync {
    fn kind(&self) -> Kind;

    fn spawn(
        &self,
        runtime: &helix_runtime::Runtime,
        host: host::Set,
        tx: helix_runtime::Sender<Update>,
        connect: Connect,
    ) -> Result<Handle, Error>;
}

#[derive(Clone)]
pub struct Handle {
    pub id: Id,
    tx: helix_runtime::Sender<Command>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").field("id", &self.id).finish()
    }
}

impl Handle {
    #[must_use]
    pub fn new(id: Id, tx: helix_runtime::Sender<Command>) -> Self {
        Self { id, tx }
    }

    pub async fn send(&self, cmd: Command) -> Result<(), helix_runtime::Closed<Command>> {
        self.tx.send(cmd).await
    }

    pub fn try_send(&self, cmd: Command) -> Result<(), helix_runtime::TrySend<Command>> {
        self.tx.try_send(cmd)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    NewThread {
        thread: thread::Id,
        scope: thread::Scope,
    },
    ListThreads {
        scope: thread::Scope,
        cursor: Option<history::Cursor>,
    },
    LoadThread {
        thread: thread::Id,
        remote: Remote,
    },
    CloseThread {
        thread: thread::Id,
    },
    Submit {
        thread: thread::Id,
        prompt: prompt::Request,
    },
    ForkSubmit {
        thread: thread::Id,
        prompt: prompt::Request,
    },
    Cancel {
        thread: thread::Id,
    },
    SetMode {
        thread: thread::Id,
        mode: mode::Id,
    },
    SetConfig {
        thread: thread::Id,
        option: config::Id,
        value: config::ValueId,
    },
    CompleteElicitation {
        thread: thread::Id,
        id: String,
        response: thread::ElicitationResponse,
    },
    Authenticate {
        thread: thread::Id,
        method: String,
    },
    ResolvePermission {
        thread: thread::Id,
        request: permission::RequestId,
        decision: permission::Decision,
    },
    Review {
        thread: thread::Id,
        command: review::Command,
    },
    DeleteThread {
        remote: Remote,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Ready { caps: Caps },
    Bound { thread: thread::Id, remote: Remote },
    Stopped,
}

#[derive(Debug)]
pub enum Update {
    Backend {
        backend: Id,
        event: Event,
    },
    Thread {
        thread: thread::Id,
        event: thread::Event,
    },
    History {
        scope: thread::Scope,
        entries: Vec<history::Stub>,
        next: Option<history::Cursor>,
    },
    Location {
        thread: thread::Id,
        location: crate::collab::Location,
    },
    Terminal {
        thread: thread::Id,
        event: terminal::Event,
    },
    Auth {
        thread: thread::Id,
        event: auth::Event,
    },
    Permission {
        thread: thread::Id,
        request: permission::Request,
    },
    ReviewAcceptedFile {
        thread: thread::Id,
        path: std::path::PathBuf,
        text: String,
    },
    Error {
        at: Target,
        error: Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Backend(Id),
    Thread(thread::Id),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
