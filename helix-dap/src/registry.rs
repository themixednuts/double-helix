use crate::{Client, Error, Result, ServerEvent, StackFrame};
use futures_util::stream::SelectAll;
use helix_core::syntax::config::DebugAdapterConfig;
use helix_runtime::{Receiver, Token};
use slotmap::SlotMap;
use std::fmt;

enum ClientStartupState {
    Initializing(Token),
    StartingSession(Token),
}

impl ClientStartupState {
    fn cancel(&self) -> &Token {
        match self {
            Self::Initializing(cancel) | Self::StartingSession(cancel) => cancel,
        }
    }
}

struct ClientSlot {
    client: Option<Client>,
    startup: Option<ClientStartupState>,
}

/// An owned debugger startup operation which can run without borrowing the registry.
#[derive(Debug)]
pub struct ClientStartup {
    id: DebugAdapterId,
    socket: Option<std::net::SocketAddr>,
    config: DebugAdapterConfig,
    cancel: Token,
}

impl ClientStartup {
    pub fn id(&self) -> DebugAdapterId {
        self.id
    }

    pub async fn run(self) -> Result<StartedClient> {
        let Self {
            id,
            socket,
            config,
            cancel,
        } = self;

        let start = async move {
            let (mut client, incoming) = match socket {
                Some(socket) => Client::tcp(socket, id).await?,
                None => {
                    Client::process(
                        &config.transport,
                        &config.command,
                        config.args.iter().map(String::as_str).collect(),
                        config.port_arg.as_deref(),
                        id,
                    )
                    .await?
                }
            };

            client.config = Some(config.clone());
            client.initialize(config.name.clone()).await?;
            client.quirks = config.quirks.clone();

            Ok(StartedClient {
                id,
                client,
                incoming,
            })
        };

        tokio::select! {
            biased;
            _ = cancel.canceled() => {
                Err(Error::Other(anyhow::anyhow!("debug adapter startup cancelled")))
            }
            result = start => result,
        }
    }
}

/// A fully initialized debugger client awaiting main-thread registry installation.
#[derive(Debug)]
pub struct StartedClient {
    id: DebugAdapterId,
    client: Client,
    incoming: Receiver<(DebugAdapterId, ServerEvent)>,
}

impl StartedClient {
    pub fn id(&self) -> DebugAdapterId {
        self.id
    }
}

/// The resgistry is a struct that manages and owns multiple debugger clients
/// This holds the responsibility of managing the lifecycle of each client
/// plus showing the heirarcihical nature betweeen them
pub struct Registry {
    inner: SlotMap<DebugAdapterId, ClientSlot>,
    /// The active debugger client
    ///
    /// TODO: You can have multiple active debuggers, so the concept of a single active debugger
    /// may need to be changed
    current_client_id: Option<DebugAdapterId>,
    /// A stream of incoming messages from all debuggers
    pub incoming: SelectAll<Receiver<(DebugAdapterId, ServerEvent)>>,
}

impl Registry {
    /// Creates a new DebuggerService instance
    pub fn new() -> Self {
        Self {
            inner: SlotMap::with_key(),
            current_client_id: None,
            incoming: SelectAll::new(),
        }
    }

    pub fn start_client(
        &mut self,
        socket: Option<std::net::SocketAddr>,
        config: DebugAdapterConfig,
    ) -> ClientStartup {
        let cancel = Token::new();
        let id = self.inner.insert_with_key(|_| ClientSlot {
            client: None,
            startup: Some(ClientStartupState::Initializing(cancel.clone())),
        });
        ClientStartup {
            id,
            socket,
            config,
            cancel,
        }
    }

    /// Installs an initialized client only if its original startup reservation is live.
    pub fn finish_client_start(&mut self, started: StartedClient) -> Option<DebugAdapterId> {
        let id = started.id;
        let slot = self.inner.get_mut(id)?;
        let cancel = match slot.startup.as_ref() {
            Some(ClientStartupState::Initializing(cancel)) => cancel.clone(),
            Some(ClientStartupState::StartingSession(_)) | None => return None,
        };

        let StartedClient {
            client, incoming, ..
        } = started;
        slot.client = Some(client);
        slot.startup = Some(ClientStartupState::StartingSession(cancel));
        self.incoming.push(incoming);
        Some(id)
    }

    pub fn is_client_initializing(&self, id: DebugAdapterId) -> bool {
        matches!(
            self.inner.get(id).and_then(|slot| slot.startup.as_ref()),
            Some(ClientStartupState::Initializing(_))
        )
    }

    pub fn is_session_starting(&self, id: DebugAdapterId) -> bool {
        matches!(
            self.inner.get(id).and_then(|slot| slot.startup.as_ref()),
            Some(ClientStartupState::StartingSession(_))
        )
    }

    pub fn is_client_starting(&self, id: DebugAdapterId) -> bool {
        self.inner
            .get(id)
            .is_some_and(|slot| slot.startup.is_some())
    }

    pub fn has_pending_clients(&self) -> bool {
        self.inner.values().any(|slot| slot.startup.is_some())
    }

    pub fn session_start_cancellation(&self, id: DebugAdapterId) -> Option<Token> {
        match self.inner.get(id)?.startup.as_ref()? {
            ClientStartupState::StartingSession(cancel) => Some(cancel.clone()),
            ClientStartupState::Initializing(_) => None,
        }
    }

    pub fn finish_session_start(&mut self, id: DebugAdapterId) -> bool {
        let Some(slot) = self.inner.get_mut(id) else {
            return false;
        };
        if !matches!(slot.startup, Some(ClientStartupState::StartingSession(_))) {
            return false;
        }
        slot.startup = None;
        true
    }

    pub fn fail_session_start(&mut self, id: DebugAdapterId) -> bool {
        if !self.is_session_starting(id) {
            return false;
        }
        self.cancel_client_start(id)
    }

    pub fn cancel_client_start(&mut self, id: DebugAdapterId) -> bool {
        let Some(cancel) = self
            .inner
            .get(id)
            .and_then(|slot| slot.startup.as_ref())
            .map(ClientStartupState::cancel)
            .cloned()
        else {
            return false;
        };
        cancel.cancel();
        self.inner.remove(id);
        if self.current_client_id == Some(id) {
            self.current_client_id = None;
        }
        true
    }

    pub fn cancel_pending_clients(&mut self) -> usize {
        let pending: Vec<_> = self
            .inner
            .iter()
            .filter_map(|(id, slot)| slot.startup.is_some().then_some(id))
            .collect();
        let count = pending.len();
        for id in pending {
            self.cancel_client_start(id);
        }
        count
    }

    pub fn remove_client(&mut self, id: DebugAdapterId) {
        if let Some(startup) = self.inner.get(id).and_then(|slot| slot.startup.as_ref()) {
            startup.cancel().cancel();
        }
        self.inner.remove(id);
        if self.current_client_id == Some(id) {
            self.current_client_id = None;
        }
    }

    pub fn get_client(&self, id: DebugAdapterId) -> Option<&Client> {
        self.inner.get(id)?.client.as_ref()
    }

    pub fn get_client_mut(&mut self, id: DebugAdapterId) -> Option<&mut Client> {
        self.inner.get_mut(id)?.client.as_mut()
    }

    pub fn get_active_client(&self) -> Option<&Client> {
        self.current_client_id.and_then(|id| self.get_client(id))
    }

    pub fn get_active_client_mut(&mut self) -> Option<&mut Client> {
        self.current_client_id
            .and_then(|id| self.get_client_mut(id))
    }

    pub fn set_active_client(&mut self, id: DebugAdapterId) {
        if self.get_client(id).is_some() {
            self.current_client_id = Some(id);
        } else {
            self.current_client_id = None;
        }
    }

    pub fn unset_active_client(&mut self) {
        self.current_client_id = None;
    }

    pub fn current_stack_frame(&self) -> Option<&StackFrame> {
        self.get_active_client()
            .and_then(|debugger| debugger.current_stack_frame())
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

slotmap::new_key_type! {
    pub struct DebugAdapterId;
}

impl fmt::Display for DebugAdapterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        for slot in self.inner.values() {
            if let Some(startup) = &slot.startup {
                startup.cancel().cancel();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::syntax::config::DebuggerQuirks;
    use tokio::io::{split, BufReader};

    fn test_config() -> DebugAdapterConfig {
        DebugAdapterConfig {
            name: "test".to_owned(),
            transport: "stdio".to_owned(),
            command: "must-not-run".to_owned(),
            args: Vec::new(),
            port_arg: None,
            templates: Vec::new(),
            quirks: DebuggerQuirks::default(),
        }
    }

    #[test]
    fn start_client_reserves_without_polling_transport() {
        let mut registry = Registry::new();
        let startup = registry.start_client(None, test_config());

        assert!(registry.is_client_starting(startup.id()));
        assert!(registry.get_client(startup.id()).is_none());

        assert!(registry.cancel_client_start(startup.id()));
    }

    #[test]
    fn remove_client_clears_active_identity() {
        let mut registry = Registry::new();
        let startup = registry.start_client(None, test_config());
        let id = startup.id();
        registry.current_client_id = Some(id);

        registry.remove_client(id);

        assert!(registry.current_client_id.is_none());
    }

    #[tokio::test]
    async fn stale_completion_cannot_restore_cancelled_client() {
        let mut registry = Registry::new();
        let startup = registry.start_client(None, test_config());
        let id = startup.id();
        assert!(registry.cancel_client_start(id));

        let (client_stream, _adapter_stream) = tokio::io::duplex(64);
        let (read, write) = split(client_stream);
        let (client, incoming) = Client::streams(
            Box::new(BufReader::new(read)),
            Box::new(write),
            None,
            id,
            None,
        )
        .expect("create test client");
        let started = StartedClient {
            id,
            client,
            incoming,
        };

        assert!(registry.finish_client_start(started).is_none());
        assert!(registry.get_client(id).is_none());
        assert!(!registry.is_client_starting(id));
    }

    #[tokio::test]
    async fn cancelled_session_rejects_late_launch_completion() {
        let mut registry = Registry::new();
        let startup = registry.start_client(None, test_config());
        let id = startup.id();
        let (client_stream, _adapter_stream) = tokio::io::duplex(64);
        let (read, write) = split(client_stream);
        let (client, incoming) = Client::streams(
            Box::new(BufReader::new(read)),
            Box::new(write),
            None,
            id,
            None,
        )
        .expect("create test client");

        registry
            .finish_client_start(StartedClient {
                id,
                client,
                incoming,
            })
            .expect("install initialized client");
        assert!(registry.is_session_starting(id));
        assert!(registry.get_client(id).is_some());

        assert!(registry.cancel_client_start(id));
        assert!(!registry.finish_session_start(id));
        assert!(registry.get_client(id).is_none());
    }

    #[tokio::test]
    async fn failed_session_start_removes_initialized_and_active_client() {
        let mut registry = Registry::new();
        let startup = registry.start_client(None, test_config());
        let id = startup.id();
        let (client_stream, _adapter_stream) = tokio::io::duplex(64);
        let (read, write) = split(client_stream);
        let (client, incoming) = Client::streams(
            Box::new(BufReader::new(read)),
            Box::new(write),
            None,
            id,
            None,
        )
        .expect("create test client");

        registry
            .finish_client_start(StartedClient {
                id,
                client,
                incoming,
            })
            .expect("install initialized client");
        registry.set_active_client(id);
        assert!(registry.get_active_client().is_some());

        assert!(registry.fail_session_start(id));
        assert!(registry.get_client(id).is_none());
        assert!(registry.get_active_client().is_none());
        assert!(!registry.finish_session_start(id));
    }
}
