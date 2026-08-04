use crate::{
    ClientFrame, ClientRequest, ProcessId, RemoteError, RequestId, ServerEvent, ServerFrame,
    ServerResponse, MAX_PROCESS_INPUT_BYTES,
};
use helix_ipc::{FrameCodec, FrameError};
use std::{
    collections::HashMap,
    io::ErrorKind,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;

const OUTBOUND_CAPACITY: usize = 256;
const EVENT_CAPACITY: usize = 256;
const MAX_PENDING_REQUESTS: usize = 256;

type PendingResult = Result<Result<ServerResponse, RemoteError>, ClientError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnected(Arc<str>),
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum ClientError {
    #[error("remote connection is closed")]
    Closed,
    #[error("remote connection handshake is still in progress")]
    NotReady,
    #[error("too many remote requests are in flight")]
    Overloaded,
    #[error("remote request identifier space is exhausted")]
    RequestIdsExhausted,
    #[error("remote request was canceled")]
    Canceled,
    #[error("remote transport failed: {0}")]
    Transport(Arc<str>),
    #[error("remote response channel closed")]
    ResponseClosed,
    #[error("remote process input exceeds {MAX_PROCESS_INPUT_BYTES} bytes")]
    ProcessInputTooLarge,
}

#[derive(Debug, Clone)]
pub enum ClientEvent {
    Remote(ServerEvent),
    TransportLog(Arc<str>),
}

pub struct ClientEvents {
    events: mpsc::Receiver<ClientEvent>,
}

impl ClientEvents {
    pub async fn recv(&mut self) -> Option<ClientEvent> {
        self.events.recv().await
    }

    pub fn try_recv(&mut self) -> Result<ClientEvent, mpsc::error::TryRecvError> {
        self.events.try_recv()
    }
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    transport: parking_lot::Mutex<Option<ActiveTransport>>,
    pending: parking_lot::Mutex<HashMap<RequestId, oneshot::Sender<PendingResult>>>,
    next_request: AtomicU64,
    next_generation: AtomicU64,
    state: watch::Sender<ConnectionState>,
    shutdown: CancellationToken,
    events: mpsc::Sender<ClientEvent>,
    event_router: parking_lot::RwLock<Option<Arc<dyn ServerEventRouter>>>,
}

pub trait ServerEventRouter: Send + Sync {
    fn route(&self, event: ServerEvent) -> Result<(), ServerEvent>;
}

#[derive(Clone)]
struct ActiveTransport {
    generation: u64,
    outbound: mpsc::Sender<ClientFrame>,
    canceled: CancellationToken,
}

impl Client {
    pub fn detached() -> (Self, ClientEvents) {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let (state, _) = watch::channel(ConnectionState::Connecting);
        let inner = Arc::new(ClientInner {
            transport: parking_lot::Mutex::new(None),
            pending: parking_lot::Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            next_generation: AtomicU64::new(1),
            state,
            shutdown: CancellationToken::new(),
            events,
            event_router: parking_lot::RwLock::new(None),
        });
        (Self { inner }, ClientEvents { events: receiver })
    }

    pub fn from_io<R, W>(reader: R, writer: W) -> (Self, ClientEvents)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (client, events) = Self::detached();
        let generation = client
            .attach_io(reader, writer)
            .expect("new remote client should accept its first transport");
        let connected = client.mark_connected(generation);
        debug_assert!(connected);
        (client, events)
    }

    pub(crate) fn attach_io<R, W>(&self, reader: R, writer: W) -> Result<u64, ClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        if self.inner.shutdown.is_cancelled() {
            return Err(ClientError::Closed);
        }
        let generation = self
            .inner
            .next_generation
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| ClientError::Closed)?;
        let (outbound, outbound_rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let canceled = self.inner.shutdown.child_token();
        let previous = self.inner.transport.lock().replace(ActiveTransport {
            generation,
            outbound,
            canceled: canceled.clone(),
        });
        if let Some(previous) = previous {
            previous.canceled.cancel();
            self.inner.fail_pending(ClientError::Transport(Arc::from(
                "remote transport replaced",
            )));
        }
        self.inner.state.send_replace(ConnectionState::Connecting);
        tokio::spawn(read_frames(
            reader,
            self.inner.clone(),
            generation,
            canceled.clone(),
        ));
        tokio::spawn(write_frames(
            writer,
            outbound_rx,
            self.inner.clone(),
            generation,
            canceled,
        ));
        Ok(generation)
    }

    pub(crate) fn mark_connected(&self, generation: u64) -> bool {
        if self.inner.shutdown.is_cancelled()
            || self
                .inner
                .transport
                .lock()
                .as_ref()
                .is_none_or(|active| active.generation != generation)
        {
            return false;
        }
        self.inner.state.send_replace(ConnectionState::Connected);
        true
    }

    pub(crate) fn finish_handshake(&self) -> bool {
        let generation = self
            .inner
            .transport
            .lock()
            .as_ref()
            .map(|active| active.generation);
        generation.is_some_and(|generation| self.mark_connected(generation))
    }

    pub(crate) fn disconnect_current(&self, reason: impl Into<Arc<str>>) {
        let generation = self
            .inner
            .transport
            .lock()
            .as_ref()
            .map(|active| active.generation);
        if let Some(generation) = generation {
            self.inner.disconnect(generation, reason);
        }
    }

    pub fn connection_state(&self) -> watch::Receiver<ConnectionState> {
        self.inner.state.subscribe()
    }

    pub fn is_connected(&self) -> bool {
        matches!(*self.inner.state.borrow(), ConnectionState::Connected)
    }

    pub fn set_event_router(&self, router: Arc<dyn ServerEventRouter>) {
        *self.inner.event_router.write() = Some(router);
    }

    pub async fn send_process_input(
        &self,
        process: ProcessId,
        bytes: Vec<u8>,
    ) -> Result<(), ClientError> {
        if bytes.len() > MAX_PROCESS_INPUT_BYTES {
            return Err(ClientError::ProcessInputTooLarge);
        }
        let outbound = self.connected_outbound()?;
        outbound
            .send(ClientFrame::ProcessInput {
                process,
                bytes: serde_bytes::ByteBuf::from(bytes),
            })
            .await
            .map_err(|_| ClientError::Closed)
    }

    pub async fn close_process_input(&self, process: ProcessId) -> Result<(), ClientError> {
        self.connected_outbound()?
            .send(ClientFrame::CloseProcessInput { process })
            .await
            .map_err(|_| ClientError::Closed)
    }

    pub async fn request(
        &self,
        request: ClientRequest,
    ) -> Result<ServerResponse, ClientRequestError> {
        self.request_inner(request, CancellationToken::new(), false)
            .await
    }

    pub async fn request_cancellable(
        &self,
        request: ClientRequest,
        canceled: CancellationToken,
    ) -> Result<ServerResponse, ClientRequestError> {
        self.request_inner(request, canceled, false).await
    }

    pub(crate) async fn request_handshake(
        &self,
        request: ClientRequest,
    ) -> Result<ServerResponse, ClientRequestError> {
        self.request_inner(request, CancellationToken::new(), true)
            .await
    }

    async fn request_inner(
        &self,
        request: ClientRequest,
        canceled: CancellationToken,
        during_handshake: bool,
    ) -> Result<ServerResponse, ClientRequestError> {
        let outbound = if during_handshake {
            self.handshake_outbound()?
        } else {
            self.connected_outbound()?
        };
        let id = RequestId(
            self.inner
                .next_request
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                    next.checked_add(1)
                })
                .map_err(|_| ClientError::RequestIdsExhausted)?,
        );
        let (respond_to, response) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock();
            if pending.len() >= MAX_PENDING_REQUESTS {
                return Err(ClientError::Overloaded.into());
            }
            pending.insert(id, respond_to);
        }
        tokio::select! {
            biased;
            _ = canceled.cancelled() => {
                self.inner.pending.lock().remove(&id);
                return Err(ClientError::Canceled.into());
            }
            result = outbound.send(ClientFrame::Request { id, request }) => {
                if result.is_err() {
                    self.inner.pending.lock().remove(&id);
                    return Err(ClientError::Closed.into());
                }
            }
        }

        tokio::select! {
            biased;
            _ = canceled.cancelled() => {
                self.inner.pending.lock().remove(&id);
                let _ = outbound.send(ClientFrame::Cancel { id }).await;
                Err(ClientError::Canceled.into())
            }
            result = response => match result {
                Ok(Ok(Ok(response))) => Ok(response),
                Ok(Ok(Err(error))) => Err(ClientRequestError::Remote(error)),
                Ok(Err(error)) => Err(ClientRequestError::Client(error)),
                Err(_) => Err(ClientError::ResponseClosed.into()),
            }
        }
    }

    pub async fn shutdown(&self) {
        if self.is_connected() {
            let _ = self.request(ClientRequest::Shutdown).await;
        }
        self.inner.shutdown.cancel();
        let generation = self
            .inner
            .transport
            .lock()
            .as_ref()
            .map(|active| active.generation);
        if let Some(generation) = generation {
            self.inner.disconnect(generation, "remote client shut down");
        }
    }

    fn connected_outbound(&self) -> Result<mpsc::Sender<ClientFrame>, ClientError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(ClientError::Closed);
        }
        match &*self.inner.state.borrow() {
            ConnectionState::Connected => self.current_outbound(),
            ConnectionState::Connecting => Err(ClientError::NotReady),
            ConnectionState::Disconnected(_) => Err(ClientError::Closed),
        }
    }

    fn handshake_outbound(&self) -> Result<mpsc::Sender<ClientFrame>, ClientError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(ClientError::Closed);
        }
        match &*self.inner.state.borrow() {
            ConnectionState::Connected | ConnectionState::Connecting => self.current_outbound(),
            ConnectionState::Disconnected(_) => Err(ClientError::Closed),
        }
    }

    fn current_outbound(&self) -> Result<mpsc::Sender<ClientFrame>, ClientError> {
        self.inner
            .transport
            .lock()
            .as_ref()
            .map(|active| active.outbound.clone())
            .ok_or(ClientError::Closed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientRequestError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Remote(#[from] RemoteError),
}

impl ClientInner {
    fn is_active_generation(&self, generation: u64) -> bool {
        self.transport
            .lock()
            .as_ref()
            .is_some_and(|active| active.generation == generation)
    }

    fn disconnect(&self, generation: u64, reason: impl Into<Arc<str>>) {
        let reason = reason.into();
        let mut transport = self.transport.lock();
        if transport
            .as_ref()
            .is_none_or(|active| active.generation != generation)
        {
            return;
        }
        if let Some(active) = transport.take() {
            active.canceled.cancel();
        }
        drop(transport);
        self.state
            .send_replace(ConnectionState::Disconnected(reason.clone()));
        self.fail_pending(ClientError::Transport(reason));
    }

    fn fail_pending(&self, error: ClientError) {
        for (_, pending) in self.pending.lock().drain() {
            let _ = pending.send(Err(error.clone()));
        }
    }

    fn complete(&self, id: RequestId, result: Result<ServerResponse, RemoteError>) {
        if let Some(pending) = self.pending.lock().remove(&id) {
            let _ = pending.send(Ok(result));
        }
    }
}

async fn read_frames<R>(
    mut reader: R,
    inner: Arc<ClientInner>,
    generation: u64,
    canceled: CancellationToken,
) where
    R: AsyncRead + Unpin,
{
    let mut codec = FrameCodec::with_limits(8 * 1024, crate::MAX_REMOTE_FRAME_BYTES);
    let reason = loop {
        let frame = tokio::select! {
            _ = canceled.cancelled() => return,
            frame = codec.read::<ServerFrame, _>(&mut reader) => frame,
        };
        if !inner.is_active_generation(generation) {
            return;
        }
        match frame {
            Ok(ServerFrame::Response { id, result }) => inner.complete(id, result),
            Ok(ServerFrame::Event(event)) => {
                let event = match inner.event_router.read().clone() {
                    Some(router) => match router.route(event) {
                        Ok(()) => continue,
                        Err(event) => event,
                    },
                    None => event,
                };
                if inner.events.send(ClientEvent::Remote(event)).await.is_err() {
                    break Arc::from("remote event consumer closed");
                }
            }
            Err(FrameError::Io(error)) if error.kind() == ErrorKind::UnexpectedEof => {
                break Arc::from("remote server closed the connection");
            }
            Err(error) => break Arc::from(format!("remote read failed: {error}")),
        }
    };
    inner.disconnect(generation, reason);
}

async fn write_frames<W>(
    mut writer: W,
    mut outbound: mpsc::Receiver<ClientFrame>,
    inner: Arc<ClientInner>,
    generation: u64,
    canceled: CancellationToken,
) where
    W: AsyncWrite + Unpin,
{
    let mut codec = FrameCodec::with_limits(8 * 1024, crate::MAX_REMOTE_FRAME_BYTES);
    loop {
        let frame = tokio::select! {
            _ = canceled.cancelled() => break,
            frame = outbound.recv() => frame,
        };
        let Some(frame) = frame else {
            break;
        };
        if let Err(error) = codec.write(&mut writer, &frame).await {
            inner.disconnect(
                generation,
                Arc::from(format!("remote write failed: {error}")),
            );
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::{RemoteProcessSpec, RemoteWatchUpdate, RemoteWorkspaceClient},
        server::run_connection,
        Capability, ClientHello, FileOperation, FileTransaction, OpenWorkspace, ReadFile,
        ResolveLanguageServerWorkspace, WorkspacePath, PROTOCOL_VERSION,
    };
    use tokio::io::{duplex, split, AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn client_routes_concurrent_responses() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("one"), b"one").unwrap();
        std::fs::write(workspace.path().join("two"), b"two").unwrap();
        let (client_stream, server_stream) = duplex(64 * 1024);
        let (client_read, client_write) = split(client_stream);
        let (server_read, server_write) = split(server_stream);
        tokio::spawn(run_connection(server_read, server_write, "test"));
        let (client, _events) = Client::from_io(client_read, client_write);

        client
            .request(ClientRequest::Hello(ClientHello {
                protocol: PROTOCOL_VERSION,
                client_version: "test".to_owned(),
                requested: vec![Capability::FileSystem],
            }))
            .await
            .unwrap();
        client
            .request(ClientRequest::OpenWorkspace(OpenWorkspace {
                root: workspace.path().to_string_lossy().into_owned(),
                session: crate::SessionId(1),
            }))
            .await
            .unwrap();

        let one = client.request(ClientRequest::ReadFile(ReadFile {
            path: WorkspacePath::from_slash_path("one").unwrap(),
            offset: 0,
            max_bytes: 16,
            expected: None,
        }));
        let two = client.request(ClientRequest::ReadFile(ReadFile {
            path: WorkspacePath::from_slash_path("two").unwrap(),
            offset: 0,
            max_bytes: 16,
            expected: None,
        }));
        let (one, two) = tokio::join!(one, two);
        let ServerResponse::FileChunk(one) = one.unwrap() else {
            panic!("expected first chunk");
        };
        let ServerResponse::FileChunk(two) = two.unwrap() else {
            panic!("expected second chunk");
        };
        assert_eq!(one.bytes.as_ref(), b"one");
        assert_eq!(two.bytes.as_ref(), b"two");
    }

    #[tokio::test]
    async fn cancellable_requests_notify_the_server_and_preserve_request_routing() {
        let (client_stream, server_stream) = duplex(64 * 1024);
        let (client_read, client_write) = split(client_stream);
        let (mut server_read, mut server_write) = split(server_stream);
        let (started, started_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut codec = FrameCodec::with_limits(8 * 1024, crate::MAX_REMOTE_FRAME_BYTES);
            let ClientFrame::Request {
                id: canceled_id,
                request: ClientRequest::Ping { nonce: 1 },
            } = codec.read(&mut server_read).await.unwrap()
            else {
                panic!("expected cancellable ping request")
            };
            started.send(()).unwrap();
            assert_eq!(
                codec
                    .read::<ClientFrame, _>(&mut server_read)
                    .await
                    .unwrap(),
                ClientFrame::Cancel { id: canceled_id }
            );
            codec
                .write(
                    &mut server_write,
                    &ServerFrame::Response {
                        id: canceled_id,
                        result: Err(RemoteError::new(
                            crate::ErrorCode::Canceled,
                            "remote request was canceled",
                        )),
                    },
                )
                .await
                .unwrap();

            let ClientFrame::Request {
                id: followup_id,
                request: ClientRequest::Ping { nonce: 2 },
            } = codec.read(&mut server_read).await.unwrap()
            else {
                panic!("expected follow-up ping request")
            };
            codec
                .write(
                    &mut server_write,
                    &ServerFrame::Response {
                        id: followup_id,
                        result: Ok(ServerResponse::Pong { nonce: 2 }),
                    },
                )
                .await
                .unwrap();
        });
        let (client, _events) = Client::from_io(client_read, client_write);
        let canceled = CancellationToken::new();
        let request = tokio::spawn({
            let client = client.clone();
            let canceled = canceled.clone();
            async move {
                client
                    .request_cancellable(ClientRequest::Ping { nonce: 1 }, canceled)
                    .await
            }
        });
        started_rx.await.unwrap();
        canceled.cancel();
        assert!(matches!(
            request.await.unwrap(),
            Err(ClientRequestError::Client(ClientError::Canceled))
        ));
        assert_eq!(
            client
                .request(ClientRequest::Ping { nonce: 2 })
                .await
                .unwrap(),
            ServerResponse::Pong { nonce: 2 }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stable_client_reconnects_and_preserves_remote_undo_history() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("important.txt"), b"restore me").unwrap();
        let (client, _events) = Client::detached();

        let (first_client, first_server) = duplex(64 * 1024);
        let (first_read, first_write) = split(first_client);
        let (first_server_read, first_server_write) = split(first_server);
        client.attach_io(first_read, first_write).unwrap();
        let first_server = tokio::spawn(run_connection(
            first_server_read,
            first_server_write,
            "test",
        ));
        let backend = RemoteWorkspaceClient::open(
            client.clone(),
            "example.test",
            "test",
            workspace.path().to_string_lossy().into_owned(),
        )
        .await
        .unwrap();
        let mut watch = backend
            .watch_files(WorkspacePath::root(), true)
            .await
            .unwrap();
        std::fs::write(workspace.path().join("watched.txt"), b"changed").unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
                .await
                .unwrap(),
            Some(RemoteWatchUpdate::Changes(changes))
                if changes.iter().any(|change| change.path.to_string() == "watched.txt")
        ));
        backend
            .apply_file_transaction(FileTransaction {
                operations: vec![FileOperation::Remove {
                    path: WorkspacePath::from_slash_path("important.txt").unwrap(),
                    recursive: false,
                }],
            })
            .await
            .unwrap();
        assert!(!workspace.path().join("important.txt").exists());

        let mut state = client.connection_state();
        first_server.abort();
        let _ = first_server.await;
        while !matches!(&*state.borrow(), ConnectionState::Disconnected(_)) {
            state.changed().await.unwrap();
        }

        let (second_client, second_server) = duplex(64 * 1024);
        let (second_read, second_write) = split(second_client);
        let (second_server_read, second_server_write) = split(second_server);
        client.attach_io(second_read, second_write).unwrap();
        let second_server = tokio::spawn(run_connection(
            second_server_read,
            second_server_write,
            "test",
        ));
        backend.reopen().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if matches!(watch.next().await, Some(RemoteWatchUpdate::Rescan)) {
                    break;
                }
            }
        })
        .await
        .expect("watch did not request a reconnect rescan");
        assert!(backend.undo_file_transaction().await.unwrap());
        assert_eq!(
            std::fs::read(workspace.path().join("important.txt")).unwrap(),
            b"restore me"
        );

        client.shutdown().await;
        second_server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn remote_process_streams_are_ordered_and_bypass_ui_events() {
        let workspace = tempfile::tempdir().unwrap();
        let (client_stream, server_stream) = duplex(1024 * 1024);
        let (client_read, client_write) = split(client_stream);
        let (server_read, server_write) = split(server_stream);
        let server = tokio::spawn(run_connection(server_read, server_write, "test"));
        let (client, mut events) = Client::from_io(client_read, client_write);
        let backend = RemoteWorkspaceClient::open(
            client.clone(),
            "example.test",
            "test",
            workspace.path().to_string_lossy().into_owned(),
        )
        .await
        .unwrap();

        #[cfg(windows)]
        let (program, args) = (
            "powershell.exe".to_owned(),
            vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "$value = [Console]::In.ReadToEnd(); [Console]::Out.Write($value); [Console]::Error.Write('stderr')".to_owned(),
            ],
        );
        #[cfg(unix)]
        let (program, args) = (
            "/bin/sh".to_owned(),
            vec!["-c".to_owned(), "cat; printf stderr >&2".to_owned()],
        );

        let process = backend
            .start_process(RemoteProcessSpec {
                program,
                args,
                cwd: WorkspacePath::root(),
                env: Default::default(),
            })
            .await
            .unwrap();
        let mut parts = process.into_parts();
        parts.stdin.write_all(b"alpha-").await.unwrap();
        parts.stdin.write_all(b"beta").await.unwrap();
        drop(parts.stdin);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let (stdout_result, stderr_result, exit_result) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(
                    parts.stdout.read_to_end(&mut stdout),
                    parts.stderr.read_to_end(&mut stderr),
                    parts.control.wait(),
                )
            })
            .await
            .expect("remote process timed out");
        stdout_result.unwrap();
        stderr_result.unwrap();
        exit_result.unwrap();
        assert_eq!(stdout, b"alpha-beta");
        assert_eq!(stderr, b"stderr");
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        client.shutdown().await;
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn remote_language_server_workspace_uses_host_roots() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(workspace.path().join("src/main.rs"), b"fn main() {}").unwrap();
        let (client_stream, server_stream) = duplex(1024 * 1024);
        let (client_read, client_write) = split(client_stream);
        let (server_read, server_write) = split(server_stream);
        let server = tokio::spawn(run_connection(server_read, server_write, "test"));
        let (client, _events) = Client::from_io(client_read, client_write);
        let backend = RemoteWorkspaceClient::open(
            client.clone(),
            "example.test",
            "test",
            workspace.path().to_string_lossy().into_owned(),
        )
        .await
        .unwrap();

        let resolved = backend
            .resolve_language_server_workspace(ResolveLanguageServerWorkspace {
                document: WorkspacePath::from_slash_path("src/main.rs").unwrap(),
                root_markers: vec!["Cargo.toml".to_owned()],
                root_dirs: Vec::new(),
                required_root_patterns: Some(vec!["Cargo.toml".to_owned()]),
            })
            .await
            .unwrap()
            .unwrap();
        assert!(resolved.root.is_root());
        assert_eq!(
            std::path::PathBuf::from(&resolved.absolute_path),
            workspace.path()
        );
        assert_eq!(url::Url::parse(&resolved.uri).unwrap().scheme(), "file");

        let missing = backend
            .resolve_language_server_workspace(ResolveLanguageServerWorkspace {
                document: WorkspacePath::from_slash_path("src/main.rs").unwrap(),
                root_markers: vec!["Cargo.toml".to_owned()],
                root_dirs: Vec::new(),
                required_root_patterns: Some(vec!["go.mod".to_owned()]),
            })
            .await
            .unwrap();
        assert!(missing.is_none());

        client.shutdown().await;
        server.await.unwrap().unwrap();
    }
}
