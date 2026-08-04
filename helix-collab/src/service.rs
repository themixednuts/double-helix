use crate::{
    Accepted, AuthError, ClientFrame, ConnectCode, ErrorCode, Event, HostEndpoint, HostFrame,
    LanguageServerDiagnostics, LanguageServerRefresh, LanguageServerResponse, ParticipantId,
    ParticipantInfo, Presence, Project, ProtocolError, Request, Response, Role,
    SnapshotContinuation, SnapshotTransferId, SyncTransferId, TransportError,
    MAX_BUFFER_SNAPSHOT_BYTES, MAX_BUFFER_SNAPSHOT_CHUNK_BYTES,
    MAX_BUFFER_SNAPSHOT_TRANSFERS_BYTES, MAX_LANGUAGE_SERVER_METHOD_BYTES,
    MAX_LANGUAGE_SERVER_NAME_BYTES, MAX_LANGUAGE_SERVER_PAYLOAD_BYTES, MAX_OPEN_BUFFERS,
    MAX_SYNC_MESSAGE_BYTES, MAX_SYNC_MESSAGE_CHUNK_BYTES, MAX_SYNC_MESSAGE_TRANSFERS_BYTES,
};
use parking_lot::Mutex as SyncMutex;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex, Notify, OwnedSemaphorePermit, Semaphore},
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const PEER_QUEUE_CAPACITY: usize = 256;
const HOST_LANGUAGE_SERVER_QUEUE_CAPACITY: usize = 128;
const MAX_PEER_LANGUAGE_SERVER_REQUESTS: usize = 64;
const MAX_PEER_CONCURRENT_REQUESTS: usize = MAX_PEER_LANGUAGE_SERVER_REQUESTS;
const MAX_COALESCED_LANGUAGE_SERVER_BYTES: usize = 16 * 1024 * 1024;
const MAX_COALESCED_LANGUAGE_SERVER_REFRESHES: usize = 128;
const PAYLOAD_TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

struct SnapshotTransfer {
    id: SnapshotTransferId,
    bytes: Arc<[u8]>,
    next_offset: usize,
    expires_at: Instant,
    _budget: OwnedSemaphorePermit,
}

struct SnapshotTransfers {
    active: Option<SnapshotTransfer>,
    next_id: u64,
}

impl SnapshotTransfers {
    fn new() -> Self {
        Self {
            active: None,
            next_id: 1,
        }
    }

    fn reserve(&mut self, budget: &Arc<Semaphore>) -> Result<OwnedSemaphorePermit, ProtocolError> {
        self.active = None;
        budget
            .clone()
            .try_acquire_many_owned(MAX_BUFFER_SNAPSHOT_BYTES as u32)
            .map_err(|_| resource_exhausted("collaboration snapshot transfer capacity is busy"))
    }

    fn start(
        &mut self,
        response: Response,
        mut budget: OwnedSemaphorePermit,
    ) -> Result<Response, ProtocolError> {
        let Response::Buffer {
            buffer,
            epoch,
            total_bytes,
            snapshot,
            continuation: None,
        } = response
        else {
            return Err(invalid_request(
                "snapshot reservation completed without a buffer response",
            ));
        };
        let bytes = snapshot.into_vec();
        if total_bytes != bytes.len() as u64 || bytes.len() > MAX_BUFFER_SNAPSHOT_BYTES {
            return Err(invalid_request("collaboration snapshot size is invalid"));
        }
        if bytes.len() <= MAX_BUFFER_SNAPSHOT_CHUNK_BYTES {
            return Ok(Response::Buffer {
                buffer,
                epoch,
                total_bytes,
                snapshot: bytes.into(),
                continuation: None,
            });
        }

        let unused = MAX_BUFFER_SNAPSHOT_BYTES - bytes.len();
        drop(
            budget
                .split(unused)
                .expect("snapshot reservation covers the validated snapshot"),
        );
        let id = SnapshotTransferId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| resource_exhausted("collaboration snapshot transfer IDs exhausted"))?;
        let bytes: Arc<[u8]> = bytes.into();
        let next_offset = MAX_BUFFER_SNAPSHOT_CHUNK_BYTES;
        let snapshot = bytes[..next_offset].to_vec().into();
        self.active = Some(SnapshotTransfer {
            id,
            bytes,
            next_offset,
            expires_at: Instant::now() + PAYLOAD_TRANSFER_TIMEOUT,
            _budget: budget,
        });
        Ok(Response::Buffer {
            buffer,
            epoch,
            total_bytes,
            snapshot,
            continuation: Some(SnapshotContinuation {
                transfer: id,
                offset: next_offset as u64,
            }),
        })
    }

    fn next(&mut self, continuation: SnapshotContinuation) -> Result<Response, ProtocolError> {
        let Some(transfer) = self.active.as_mut() else {
            return Err(snapshot_transfer_missing());
        };
        if continuation.transfer != transfer.id {
            return Err(snapshot_transfer_missing());
        }
        if continuation.offset != transfer.next_offset as u64 {
            return Err(invalid_request(
                "collaboration snapshot continuation offset is invalid",
            ));
        }

        let offset = transfer.next_offset;
        let transfer_id = transfer.id;
        let end = offset
            .saturating_add(MAX_BUFFER_SNAPSHOT_CHUNK_BYTES)
            .min(transfer.bytes.len());
        let snapshot = transfer.bytes[offset..end].to_vec().into();
        transfer.next_offset = end;
        let continuation = (end < transfer.bytes.len()).then_some(SnapshotContinuation {
            transfer: transfer.id,
            offset: end as u64,
        });
        if continuation.is_none() {
            self.active = None;
        }
        Ok(Response::BufferSnapshotChunk {
            transfer: transfer_id,
            offset: offset as u64,
            snapshot,
            continuation,
        })
    }

    fn deadline(&self) -> Option<Instant> {
        self.active.as_ref().map(|transfer| transfer.expires_at)
    }

    fn expire(&mut self) {
        self.active = None;
    }
}

struct SyncTransfer {
    id: SyncTransferId,
    buffer: crate::BufferId,
    epoch: u64,
    total_bytes: usize,
    message: Vec<u8>,
    expires_at: Instant,
    budget: OwnedSemaphorePermit,
}

enum SyncUpload {
    Pending,
    Complete {
        request: Request,
        _budget: OwnedSemaphorePermit,
    },
}

#[derive(Default)]
struct SyncTransfers {
    active: Option<SyncTransfer>,
}

impl SyncTransfers {
    fn start(
        &mut self,
        budget: &Arc<Semaphore>,
        id: SyncTransferId,
        buffer: crate::BufferId,
        epoch: u64,
        total_bytes: u64,
        message: Vec<u8>,
    ) -> Result<SyncUpload, ProtocolError> {
        self.active = None;
        let total_bytes = usize::try_from(total_bytes)
            .ok()
            .filter(|total| *total <= MAX_SYNC_MESSAGE_BYTES)
            .ok_or_else(|| invalid_request("collaboration sync size is invalid"))?;
        if id.0 == 0
            || message.is_empty()
            || message.len() > MAX_SYNC_MESSAGE_CHUNK_BYTES
            || message.len() >= total_bytes
        {
            return Err(invalid_request("collaboration sync start chunk is invalid"));
        }
        let permit = budget
            .clone()
            .try_acquire_many_owned(total_bytes as u32)
            .map_err(|_| resource_exhausted("collaboration sync transfer capacity is busy"))?;
        let mut assembled = message;
        assembled
            .try_reserve_exact(total_bytes - assembled.len())
            .map_err(|_| resource_exhausted("collaboration sync allocation failed"))?;
        self.active = Some(SyncTransfer {
            id,
            buffer,
            epoch,
            total_bytes,
            message: assembled,
            expires_at: Instant::now() + PAYLOAD_TRANSFER_TIMEOUT,
            budget: permit,
        });
        Ok(SyncUpload::Pending)
    }

    fn next(
        &mut self,
        id: SyncTransferId,
        offset: u64,
        message: Vec<u8>,
    ) -> Result<SyncUpload, ProtocolError> {
        let Some(mut transfer) = self.active.take() else {
            return Err(sync_transfer_missing());
        };
        if id != transfer.id {
            return Err(sync_transfer_missing());
        }
        if offset != transfer.message.len() as u64
            || message.is_empty()
            || message.len() > MAX_SYNC_MESSAGE_CHUNK_BYTES
            || message.len() > transfer.total_bytes - transfer.message.len()
        {
            return Err(invalid_request(
                "collaboration sync continuation chunk is invalid",
            ));
        }
        transfer.message.extend_from_slice(&message);
        if transfer.message.len() < transfer.total_bytes {
            self.active = Some(transfer);
            return Ok(SyncUpload::Pending);
        }
        Ok(SyncUpload::Complete {
            request: Request::SyncBuffer {
                buffer: transfer.buffer,
                epoch: transfer.epoch,
                message: transfer.message.into(),
            },
            _budget: transfer.budget,
        })
    }

    fn deadline(&self) -> Option<Instant> {
        self.active.as_ref().map(|transfer| transfer.expires_at)
    }

    fn expire(&mut self) {
        self.active = None;
    }
}

#[derive(Clone)]
struct Peer {
    incarnation: u64,
    outbound: PeerOutbound,
    disconnect: CancellationToken,
}

#[derive(Clone)]
struct PeerOutbound {
    reliable: mpsc::Sender<HostFrame>,
    coalesced: Arc<SyncMutex<CoalescedEvents>>,
    ready: Arc<Notify>,
}

#[derive(Default)]
struct CoalescedEvents {
    worktree: Option<Event>,
    presences: HashMap<ParticipantId, Presence>,
    language_server_diagnostics: VecDeque<LanguageServerDiagnostics>,
    language_server_refreshes: VecDeque<LanguageServerRefresh>,
    language_server_bytes: usize,
    next: CoalescedKind,
}

#[derive(Clone, Copy, Default)]
enum CoalescedKind {
    #[default]
    Worktree,
    Presence,
    LanguageServerDiagnostics,
    LanguageServerRefresh,
}

impl CoalescedKind {
    fn next(self) -> Self {
        match self {
            Self::Worktree => Self::Presence,
            Self::Presence => Self::LanguageServerDiagnostics,
            Self::LanguageServerDiagnostics => Self::LanguageServerRefresh,
            Self::LanguageServerRefresh => Self::Worktree,
        }
    }
}

impl PeerOutbound {
    fn new(reliable: mpsc::Sender<HostFrame>) -> Self {
        Self {
            reliable,
            coalesced: Arc::new(SyncMutex::new(CoalescedEvents::default())),
            ready: Arc::new(Notify::new()),
        }
    }

    fn send(&self, disconnect: &CancellationToken, frame: HostFrame) -> bool {
        match frame {
            HostFrame::Event(Event::Presence(presence)) => {
                self.coalesced
                    .lock()
                    .presences
                    .insert(presence.participant, presence);
                self.ready.notify_one();
                true
            }
            HostFrame::Event(event @ Event::WorktreeChanged { .. }) => {
                let mut pending = self.coalesced.lock();
                match (&mut pending.worktree, event) {
                    (
                        Some(Event::WorktreeChanged {
                            file_revision,
                            changes,
                            rescan,
                        }),
                        Event::WorktreeChanged {
                            file_revision: incoming_revision,
                            changes: incoming_changes,
                            rescan: incoming_rescan,
                        },
                    ) => {
                        *file_revision = (*file_revision).max(incoming_revision);
                        if *rescan || incoming_rescan {
                            changes.clear();
                            *rescan = true;
                        } else {
                            for incoming in incoming_changes {
                                if let Some(change) = changes
                                    .iter_mut()
                                    .find(|change| change.path == incoming.path)
                                {
                                    *change = incoming;
                                } else {
                                    changes.push(incoming);
                                }
                            }
                            if changes.len() > crate::MAX_WORKTREE_CHANGES_PER_EVENT {
                                changes.clear();
                                *rescan = true;
                            } else {
                                changes.sort_unstable_by(|left, right| left.path.cmp(&right.path));
                            }
                        }
                    }
                    (slot, event) => *slot = Some(event),
                }
                drop(pending);
                self.ready.notify_one();
                true
            }
            HostFrame::Event(Event::LanguageServerDiagnostics(diagnostics)) => {
                let mut pending = self.coalesced.lock();
                if let Some(index) = pending
                    .language_server_diagnostics
                    .iter()
                    .position(|queued| {
                        queued.path == diagnostics.path && queued.server == diagnostics.server
                    })
                {
                    if let Some(replaced) = pending.language_server_diagnostics.remove(index) {
                        pending.language_server_bytes = pending
                            .language_server_bytes
                            .saturating_sub(replaced.params.len());
                    }
                }
                while pending.language_server_diagnostics.len() >= MAX_OPEN_BUFFERS
                    || pending
                        .language_server_bytes
                        .saturating_add(diagnostics.params.len())
                        > MAX_COALESCED_LANGUAGE_SERVER_BYTES
                {
                    let Some(evicted) = pending.language_server_diagnostics.pop_front() else {
                        break;
                    };
                    pending.language_server_bytes = pending
                        .language_server_bytes
                        .saturating_sub(evicted.params.len());
                }
                pending.language_server_bytes = pending
                    .language_server_bytes
                    .saturating_add(diagnostics.params.len());
                pending.language_server_diagnostics.push_back(diagnostics);
                drop(pending);
                self.ready.notify_one();
                true
            }
            HostFrame::Event(Event::LanguageServerRefresh(refresh)) => {
                let mut pending = self.coalesced.lock();
                if let Some(index) = pending.language_server_refreshes.iter().position(|queued| {
                    queued.server == refresh.server && queued.kind == refresh.kind
                }) {
                    pending.language_server_refreshes.remove(index);
                }
                if pending.language_server_refreshes.len()
                    >= MAX_COALESCED_LANGUAGE_SERVER_REFRESHES
                {
                    pending.language_server_refreshes.pop_front();
                }
                pending.language_server_refreshes.push_back(refresh);
                drop(pending);
                self.ready.notify_one();
                true
            }
            frame => match self.reliable.try_send(frame) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_)) => {
                    disconnect.cancel();
                    false
                }
            },
        }
    }

    fn take_coalesced(&self) -> Option<HostFrame> {
        let mut pending = self.coalesced.lock();
        let mut event = None;
        let mut kind = pending.next;
        for _ in 0..4 {
            event = match kind {
                CoalescedKind::Worktree => pending.worktree.take(),
                CoalescedKind::Presence => {
                    let participant = pending.presences.keys().next().copied();
                    participant.and_then(|participant| {
                        pending.presences.remove(&participant).map(Event::Presence)
                    })
                }
                CoalescedKind::LanguageServerDiagnostics => pending
                    .language_server_diagnostics
                    .pop_front()
                    .map(|diagnostics| {
                        pending.language_server_bytes = pending
                            .language_server_bytes
                            .saturating_sub(diagnostics.params.len());
                        Event::LanguageServerDiagnostics(diagnostics)
                    }),
                CoalescedKind::LanguageServerRefresh => pending
                    .language_server_refreshes
                    .pop_front()
                    .map(Event::LanguageServerRefresh),
            };
            if event.is_some() {
                pending.next = kind.next();
                break;
            }
            kind = kind.next();
        }
        let more = pending.worktree.is_some()
            || !pending.presences.is_empty()
            || !pending.language_server_diagnostics.is_empty()
            || !pending.language_server_refreshes.is_empty();
        drop(pending);
        if more {
            self.ready.notify_one();
        }
        event.map(HostFrame::Event)
    }
}

struct Service {
    endpoint: Arc<HostEndpoint>,
    project: Arc<Project>,
    peers: Mutex<HashMap<ParticipantId, Peer>>,
    presences: Mutex<HashMap<ParticipantId, Presence>>,
    lease_expirations: SyncMutex<HashMap<ParticipantId, CancellationToken>>,
    lease_expiration_shutdown: CancellationToken,
    language_servers: mpsc::Sender<HostLanguageServerRequest>,
    snapshot_budget: Arc<Semaphore>,
    sync_budget: Arc<Semaphore>,
}

pub struct HostHandle {
    service: Arc<Service>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<Result<(), HostServiceError>>>,
    language_servers: Option<mpsc::Receiver<HostLanguageServerRequest>>,
}

pub struct HostLanguageServerRequest {
    pub participant: ParticipantInfo,
    pub buffer: crate::BufferId,
    pub path: helix_workspace::WorkspacePath,
    pub text: String,
    pub server: String,
    pub method: String,
    pub params: Vec<u8>,
    response: oneshot::Sender<LanguageServerResponse>,
}

impl std::fmt::Debug for HostLanguageServerRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostLanguageServerRequest")
            .field("participant", &self.participant.id)
            .field("buffer", &self.buffer)
            .field("path", &self.path)
            .field("text_bytes", &self.text.len())
            .field("server", &self.server)
            .field("method", &self.method)
            .field("params_bytes", &self.params.len())
            .finish_non_exhaustive()
    }
}

impl HostLanguageServerRequest {
    pub fn respond(self, response: LanguageServerResponse) {
        let _ = self.response.send(response);
    }

    pub fn is_canceled(&self) -> bool {
        self.response.is_closed()
    }

    /// Resolves when the requesting peer no longer accepts a response.
    ///
    /// Host integrations should select this against the language-server call
    /// so guest timeouts cancel work instead of consuming the bounded host
    /// execution lane until the server eventually responds.
    pub async fn canceled(&mut self) {
        self.response.closed().await;
    }
}

#[derive(Clone)]
pub struct HostProjectPublisher {
    service: Arc<Service>,
}

pub struct HostFileMutation {
    service: Arc<Service>,
    mutation: crate::project::ExternalFileMutation,
}

impl HostHandle {
    pub async fn start(
        endpoint: Arc<HostEndpoint>,
        project: Arc<Project>,
    ) -> Result<Self, HostServiceError> {
        let file_watch = project.watch_files().await?;
        let (language_servers, language_server_requests) =
            mpsc::channel(HOST_LANGUAGE_SERVER_QUEUE_CAPACITY);
        let service = Arc::new(Service {
            endpoint,
            project,
            peers: Mutex::new(HashMap::new()),
            presences: Mutex::new(HashMap::new()),
            lease_expirations: SyncMutex::new(HashMap::new()),
            lease_expiration_shutdown: CancellationToken::new(),
            language_servers,
            snapshot_budget: Arc::new(Semaphore::new(MAX_BUFFER_SNAPSHOT_TRANSFERS_BYTES)),
            sync_budget: Arc::new(Semaphore::new(MAX_SYNC_MESSAGE_TRANSFERS_BYTES)),
        });
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(service.clone().run(shutdown_rx, file_watch));
        Ok(Self {
            service,
            shutdown,
            task: Some(task),
            language_servers: Some(language_server_requests),
        })
    }

    pub fn take_language_server_requests(&mut self) -> mpsc::Receiver<HostLanguageServerRequest> {
        self.language_servers
            .take()
            .expect("host language-server requests already taken")
    }

    pub async fn invite(
        &self,
        role: Role,
        expires_unix_secs: u64,
        now_unix_secs: u64,
    ) -> Result<ConnectCode, HostServiceError> {
        let owner = self.service.endpoint.owner().await;
        self.service
            .endpoint
            .invite(owner, role, expires_unix_secs, now_unix_secs)
            .await
            .map_err(Into::into)
    }

    pub async fn owner_code(&self) -> Result<ConnectCode, HostServiceError> {
        self.service
            .endpoint
            .owner_code(now_unix_secs())
            .await
            .map_err(Into::into)
    }

    pub fn project_publisher(&self) -> HostProjectPublisher {
        HostProjectPublisher {
            service: self.service.clone(),
        }
    }

    pub async fn shutdown(mut self) -> Result<(), HostServiceError> {
        let _ = self.shutdown.send(true);
        self.service.cancel_peer_connections().await;
        self.service.endpoint.close();
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await.map_err(HostServiceError::Task)??;
        Ok(())
    }
}

impl HostProjectPublisher {
    pub async fn reserve_file_mutation(
        &self,
        transaction: helix_workspace::FileTransaction,
    ) -> Result<HostFileMutation, HostServiceError> {
        let mutation = self
            .service
            .project
            .clone()
            .reserve_external_file_mutation(transaction)
            .await?;
        Ok(HostFileMutation {
            service: self.service.clone(),
            mutation,
        })
    }

    pub async fn publish_language_server_diagnostics(
        &self,
        diagnostics: LanguageServerDiagnostics,
    ) -> Result<(), ProtocolError> {
        validate_language_server_request(
            &diagnostics.server,
            "textDocument/publishDiagnostics",
            diagnostics.params.len(),
        )?;
        let participants = self
            .service
            .project
            .lease_holders_for_path(&diagnostics.path, self.service.peer_ids().await)
            .await;
        for participant in participants {
            self.service
                .deliver(
                    participant,
                    HostFrame::Event(Event::LanguageServerDiagnostics(diagnostics.clone())),
                )
                .await;
        }
        Ok(())
    }

    pub async fn publish_language_server_refresh(
        &self,
        refresh: LanguageServerRefresh,
    ) -> Result<(), ProtocolError> {
        validate_language_server_request(&refresh.server, "workspace/refresh", 0)?;
        let participants = self
            .service
            .project
            .participants_with_leases(self.service.peer_ids().await)
            .await;
        for participant in participants {
            self.service
                .deliver(
                    participant,
                    HostFrame::Event(Event::LanguageServerRefresh(refresh.clone())),
                )
                .await;
        }
        Ok(())
    }
}

impl HostFileMutation {
    pub async fn commit(self) {
        let (transaction, file_revision) = self.mutation.commit().await;
        self.service
            .broadcast(HostFrame::Event(Event::FilesChanged {
                file_revision,
                transaction,
                undone: false,
            }))
            .await;
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.service.endpoint.close();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Service {
    async fn run(
        self: Arc<Self>,
        mut shutdown: watch::Receiver<bool>,
        mut file_watch: Option<crate::BackendFileWatch>,
    ) -> Result<(), HostServiceError> {
        let mut connections = JoinSet::new();
        // Authentication spans multiple polls and must survive unrelated host events.
        let mut accept = std::pin::pin!(accept_connection(self.endpoint.clone()));
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                accepted = &mut accept => {
                    match accepted {
                        Ok(connection) => {
                            let service = self.clone();
                            connections.spawn(async move { service.run_connection(connection).await });
                        }
                        Err(TransportError::EndpointClosed) => break,
                        Err(error) => log_transport_error(&error),
                    }
                    accept.set(accept_connection(self.endpoint.clone()));
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        return Err(HostServiceError::Task(error));
                    }
                }
                update = next_file_update(&mut file_watch), if file_watch.is_some() => {
                    let update = if let Some(update) = update {
                        update
                    } else {
                        file_watch = None;
                        crate::BackendFileUpdate::Rescan
                    };
                    let (file_revision, update) = self
                        .project
                        .publish_external_file_update(update)
                        .await;
                    let (changes, rescan) = match update {
                        crate::BackendFileUpdate::Changes(changes) => (changes, false),
                        crate::BackendFileUpdate::Rescan => (Vec::new(), true),
                    };
                    self.broadcast(HostFrame::Event(Event::WorktreeChanged {
                        file_revision,
                        changes,
                        rescan,
                    }))
                    .await;
                }
            }
        }
        self.lease_expiration_shutdown.cancel();
        self.cancel_peer_connections().await;
        self.endpoint.close();
        connections.shutdown().await;
        Ok(())
    }

    async fn cancel_peer_connections(&self) {
        let disconnects = self
            .peers
            .lock()
            .await
            .values()
            .map(|peer| peer.disconnect.clone())
            .collect::<Vec<_>>();
        for disconnect in disconnects {
            disconnect.cancel();
        }
        for expiration in self
            .lease_expirations
            .lock()
            .drain()
            .map(|(_, token)| token)
        {
            expiration.cancel();
        }
    }

    async fn run_connection(self: Arc<Self>, accepted: Accepted) -> Result<(), HostServiceError> {
        let participant = accepted.participant;
        if let Some(expiration) = self.lease_expirations.lock().remove(&participant.id) {
            expiration.cancel();
        }
        let (reliable, mut reliable_rx) = mpsc::channel(PEER_QUEUE_CAPACITY);
        let outbound = PeerOutbound::new(reliable);
        let disconnect = CancellationToken::new();
        let mut peers = self.peers.lock().await;
        let participants = self.endpoint.participants().await;
        let project_state = self.project.clone().reserve_file_state(participants).await;
        assert!(deliver_frame(
            &outbound,
            &disconnect,
            HostFrame::Event(Event::ProjectState(project_state.state.clone())),
        ));
        let previous = peers.insert(
            participant.id,
            Peer {
                incarnation: participant.incarnation,
                outbound: outbound.clone(),
                disconnect: disconnect.clone(),
            },
        );
        if let Some(previous) = previous {
            previous.disconnect.cancel();
        }
        drop(project_state);
        drop(peers);
        self.broadcast(HostFrame::Event(Event::ParticipantJoined(
            participant.clone(),
        )))
        .await;

        let mut sender = accepted.sender;
        let writer_disconnect = disconnect.clone();
        let writer_outbound = outbound.clone();
        let writer = tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = writer_disconnect.cancelled() => break,
                    frame = reliable_rx.recv() => {
                        let Some(frame) = frame else {
                            break;
                        };
                        frame
                    },
                    _ = writer_outbound.ready.notified() => {
                        let Some(frame) = writer_outbound.take_coalesced() else {
                            continue;
                        };
                        frame
                    },
                };
                tokio::select! {
                    biased;
                    _ = writer_disconnect.cancelled() => break,
                    result = sender.send(&frame) => result?,
                }
            }
            sender.close();
            Ok::<_, TransportError>(())
        });
        let mut receiver = accepted.receiver;
        let mut last_request_id = 0;
        let mut concurrent_requests = JoinSet::new();
        let mut request_cancellations = HashMap::<u64, CancellationToken>::new();
        let mut snapshot_transfers = SnapshotTransfers::new();
        let mut sync_transfers = SyncTransfers::default();
        loop {
            let snapshot_deadline = snapshot_transfers.deadline();
            let sync_deadline = sync_transfers.deadline();
            let transfer_deadline = snapshot_deadline.into_iter().chain(sync_deadline).min();
            let frame = match tokio::select! {
                biased;
                _ = disconnect.cancelled() => break,
                _ = tokio::time::sleep_until(transfer_deadline.unwrap_or_else(Instant::now)), if transfer_deadline.is_some() => {
                    let now = Instant::now();
                    if snapshot_deadline.is_some_and(|deadline| deadline <= now) {
                        snapshot_transfers.expire();
                    }
                    if sync_deadline.is_some_and(|deadline| deadline <= now) {
                        sync_transfers.expire();
                    }
                    continue;
                },
                frame = receiver.receive::<ClientFrame>() => frame,
            } {
                Ok(frame) => frame,
                Err(_) => break,
            };
            while let Some(result) = concurrent_requests.try_join_next() {
                if let Ok(id) = result {
                    request_cancellations.remove(&id);
                }
            }
            let (id, request) = match frame {
                ClientFrame::Request { id, request } => (id, request),
                ClientFrame::Cancel { id } => {
                    if let Some(canceled) = request_cancellations.get(&id) {
                        canceled.cancel();
                    }
                    continue;
                }
                ClientFrame::Authenticate(_) => {
                    if !self.reply(
                        &outbound,
                        &disconnect,
                        id_for_invalid_frame(),
                        Err(ProtocolError {
                            code: ErrorCode::Conflict,
                            message: "connection is already authenticated".to_owned(),
                            retryable: false,
                        }),
                    ) {
                        break;
                    }
                    continue;
                }
            };
            if id == 0 || id <= last_request_id {
                if !self.reply(
                    &outbound,
                    &disconnect,
                    id,
                    Err(ProtocolError {
                        code: ErrorCode::Conflict,
                        message: "request IDs must increase monotonically".to_owned(),
                        retryable: false,
                    }),
                ) {
                    break;
                }
                continue;
            }
            last_request_id = id;
            if matches!(
                request,
                Request::LanguageServer { .. } | Request::SearchContent(_)
            ) {
                if concurrent_requests.len() >= MAX_PEER_CONCURRENT_REQUESTS {
                    if !self.reply(
                        &outbound,
                        &disconnect,
                        id,
                        Err(resource_exhausted(
                            "too many concurrent collaboration requests are active",
                        )),
                    ) {
                        break;
                    }
                    continue;
                }
                let service = self.clone();
                let participant = participant.clone();
                let outbound = outbound.clone();
                let disconnect = disconnect.clone();
                let canceled = CancellationToken::new();
                request_cancellations.insert(id, canceled.clone());
                concurrent_requests.spawn(async move {
                    let result = match request {
                        request @ Request::LanguageServer { .. } => {
                            service
                                .handle_language_server_request(
                                    &participant,
                                    request,
                                    &disconnect,
                                    &canceled,
                                )
                                .await
                        }
                        request @ Request::SearchContent(_) => {
                            service
                                .handle_content_search_request(&participant, request, &canceled)
                                .await
                        }
                        _ => unreachable!("concurrent request lane received an ordered request"),
                    };
                    service.reply(&outbound, &disconnect, id, result);
                    id
                });
                continue;
            }
            let leave = matches!(request, Request::Leave);
            let result = self
                .handle_ordered(
                    &participant,
                    request,
                    &mut snapshot_transfers,
                    &mut sync_transfers,
                )
                .await;
            if !self.reply(&outbound, &disconnect, id, result) {
                break;
            }
            if leave {
                break;
            }
        }

        for canceled in request_cancellations.values() {
            canceled.cancel();
        }
        concurrent_requests.abort_all();
        while concurrent_requests.join_next().await.is_some() {}

        let removed = {
            let mut peers = self.peers.lock().await;
            if peers
                .get(&participant.id)
                .is_some_and(|peer| peer.incarnation == participant.incarnation)
            {
                peers.remove(&participant.id);
                true
            } else {
                false
            }
        };
        if removed {
            self.service_disconnect(&participant).await;
        }
        drop(outbound);
        writer.await.map_err(HostServiceError::Task)??;
        Ok(())
    }

    async fn handle_ordered(
        &self,
        actor: &ParticipantInfo,
        request: Request,
        snapshot_transfers: &mut SnapshotTransfers,
        sync_transfers: &mut SyncTransfers,
    ) -> Result<Response, ProtocolError> {
        self.endpoint
            .authorize_request(actor.id, actor.incarnation, &request)
            .await
            .map_err(|error| protocol_auth_error(&error))?;
        if let Request::ContinueBufferSnapshot { continuation } = request {
            return snapshot_transfers.next(continuation);
        }

        let (request, sync_budget) = match request {
            Request::StartBufferSync {
                transfer,
                buffer,
                epoch,
                total_bytes,
                message,
            } => match sync_transfers.start(
                &self.sync_budget,
                transfer,
                buffer,
                epoch,
                total_bytes,
                message.into_vec(),
            )? {
                SyncUpload::Pending => return Ok(Response::Unit),
                SyncUpload::Complete { request, _budget } => (request, Some(_budget)),
            },
            Request::ContinueBufferSync {
                transfer,
                offset,
                message,
            } => match sync_transfers.next(transfer, offset, message.into_vec())? {
                SyncUpload::Pending => return Ok(Response::Unit),
                SyncUpload::Complete { request, _budget } => (request, Some(_budget)),
            },
            Request::SyncBuffer { message, .. } if message.len() > MAX_SYNC_MESSAGE_CHUNK_BYTES => {
                return Err(invalid_request(
                    "collaboration sync message requires chunked transfer",
                ));
            }
            request => (request, None),
        };

        let resync_buffer = match &request {
            Request::SyncBuffer { buffer, .. } => Some(*buffer),
            _ => None,
        };
        let snapshot_budget = if matches!(
            request,
            Request::OpenBuffer { .. } | Request::ReadBuffer { .. }
        ) {
            Some(snapshot_transfers.reserve(&self.snapshot_budget)?)
        } else {
            None
        };
        let response = match self.handle_authorized(actor, request).await {
            Ok(response) => response,
            Err(error) if error.code == ErrorCode::ResyncRequired && resync_buffer.is_some() => {
                let Some(buffer) = resync_buffer else {
                    return Err(error);
                };
                let epoch = self
                    .project
                    .buffer_epoch_for(actor.id, buffer)
                    .await
                    .map_err(ProtocolError::from)?;
                self.deliver(
                    actor.id,
                    HostFrame::Event(Event::ResyncRequired { buffer, epoch }),
                )
                .await;
                Response::Unit
            }
            Err(error) => return Err(error),
        };
        drop(sync_budget);
        if let Some(snapshot_budget) = snapshot_budget {
            snapshot_transfers.start(response, snapshot_budget)
        } else {
            Ok(response)
        }
    }

    async fn handle_authorized(
        &self,
        actor: &ParticipantInfo,
        request: Request,
    ) -> Result<Response, ProtocolError> {
        match request {
            Request::LanguageServer { .. } => Err(ProtocolError {
                code: ErrorCode::Internal,
                message: "language-server request reached the ordered project handler".to_owned(),
                retryable: false,
            }),
            Request::ContinueBufferSnapshot { .. }
            | Request::StartBufferSync { .. }
            | Request::ContinueBufferSync { .. } => Err(ProtocolError {
                code: ErrorCode::Internal,
                message: "payload transfer reached the project handler".to_owned(),
                retryable: false,
            }),
            Request::Invite {
                role,
                expires_unix_secs,
            } => self
                .endpoint
                .invite(actor.id, role, expires_unix_secs, now_unix_secs())
                .await
                .map(|code| Response::Invitation(code.to_string()))
                .map_err(|error| transport_protocol_error(&error)),
            Request::SetRole { participant, role } => {
                self.endpoint
                    .set_role(actor.id, participant, role)
                    .await
                    .map_err(|error| protocol_auth_error(&error))?;
                self.broadcast(HostFrame::Event(Event::RoleChanged { participant, role }))
                    .await;
                Ok(Response::Unit)
            }
            Request::RemoveParticipant { participant } => {
                self.endpoint
                    .remove_participant(actor.id, participant)
                    .await
                    .map_err(|error| protocol_auth_error(&error))?;
                self.project
                    .release_participant(participant)
                    .await
                    .map_err(ProtocolError::from)?;
                if let Some(expiration) = self.lease_expirations.lock().remove(&participant) {
                    expiration.cancel();
                }
                if let Some(peer) = self.peers.lock().await.remove(&participant) {
                    peer.disconnect.cancel();
                }
                self.presences.lock().await.remove(&participant);
                self.broadcast(HostFrame::Event(Event::ParticipantLeft { participant }))
                    .await;
                Ok(Response::Unit)
            }
            Request::Follow { participant } => {
                if !self.peers.lock().await.contains_key(&participant) {
                    return Err(ProtocolError {
                        code: ErrorCode::NotFound,
                        message: "collaboration participant is not connected".to_owned(),
                        retryable: false,
                    });
                }
                let location = if let Some(presence) =
                    self.presences.lock().await.get(&participant).cloned()
                {
                    Some(crate::FollowLocation {
                        path: self
                            .project
                            .buffer_path_for(participant, presence.buffer)
                            .await
                            .map_err(ProtocolError::from)?,
                        presence,
                    })
                } else {
                    None
                };
                self.deliver(
                    participant,
                    HostFrame::Event(Event::FollowRequested {
                        follower: actor.id,
                        leader: participant,
                    }),
                )
                .await;
                Ok(Response::Following { location })
            }
            Request::CloseBuffer { buffer } => {
                let participants = self.peer_ids().await;
                let info = self.endpoint.participants().await;
                let outcome = self
                    .project
                    .handle(
                        actor.id,
                        Request::CloseBuffer { buffer },
                        participants.clone(),
                        info,
                    )
                    .await
                    .map_err(ProtocolError::from)?;
                let cleared = {
                    let mut presences = self.presences.lock().await;
                    if presences
                        .get(&actor.id)
                        .is_some_and(|presence| presence.buffer == buffer)
                    {
                        presences.remove(&actor.id);
                        true
                    } else {
                        false
                    }
                };
                if cleared {
                    for participant in participants {
                        if participant != actor.id {
                            self.deliver(
                                participant,
                                HostFrame::Event(Event::PresenceCleared {
                                    participant: actor.id,
                                    buffer,
                                }),
                            )
                            .await;
                        }
                    }
                }
                Ok(outcome.response)
            }
            Request::PublishPresence(presence) => {
                let participants = self.peer_ids().await;
                let info = self.endpoint.participants().await;
                let outcome = self
                    .project
                    .handle(
                        actor.id,
                        Request::PublishPresence(presence.clone()),
                        participants,
                        info,
                    )
                    .await
                    .map_err(ProtocolError::from)?;
                self.presences.lock().await.insert(actor.id, presence);
                for (participant, event) in outcome.deliveries {
                    self.deliver(participant, HostFrame::Event(event)).await;
                }
                Ok(outcome.response)
            }
            Request::Leave => {
                self.project
                    .release_participant(actor.id)
                    .await
                    .map_err(ProtocolError::from)?;
                Ok(Response::Unit)
            }
            request => {
                let participants = self.peer_ids().await;
                let info = self.endpoint.participants().await;
                let outcome = self
                    .project
                    .handle(actor.id, request, participants, info)
                    .await
                    .map_err(ProtocolError::from)?;
                for (participant, event) in outcome.deliveries {
                    self.deliver(participant, HostFrame::Event(event)).await;
                }
                Ok(outcome.response)
            }
        }
    }

    async fn handle_language_server_request(
        &self,
        actor: &ParticipantInfo,
        request: Request,
        disconnect: &CancellationToken,
        canceled: &CancellationToken,
    ) -> Result<Response, ProtocolError> {
        self.endpoint
            .authorize_request(actor.id, actor.incarnation, &request)
            .await
            .map_err(|error| protocol_auth_error(&error))?;
        let Request::LanguageServer {
            buffer,
            server,
            method,
            params,
        } = request
        else {
            unreachable!("language-server handler received another request kind")
        };
        validate_language_server_request(&server, &method, params.len())?;
        let (path, text) = self
            .project
            .buffer_path_and_text_for(actor.id, buffer)
            .await
            .map_err(ProtocolError::from)?;
        let (response, response_rx) = oneshot::channel();
        self.language_servers
            .try_send(HostLanguageServerRequest {
                participant: actor.clone(),
                buffer,
                path,
                text,
                server,
                method,
                params: params.into_vec(),
                response,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    resource_exhausted("host language-server queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => ProtocolError {
                    code: ErrorCode::Internal,
                    message: "host language-server service is unavailable".to_owned(),
                    retryable: true,
                },
            })?;
        let response = tokio::select! {
            _ = canceled.cancelled() => return Err(ProtocolError {
                code: ErrorCode::Conflict,
                message: "collaboration request was canceled".to_owned(),
                retryable: true,
            }),
            _ = disconnect.cancelled() => return Err(ProtocolError {
                code: ErrorCode::Conflict,
                message: "collaboration connection closed".to_owned(),
                retryable: true,
            }),
            response = tokio::time::timeout(crate::LANGUAGE_SERVER_REQUEST_TIMEOUT, response_rx) => {
                response
                    .map_err(|_| ProtocolError {
                        code: ErrorCode::ResourceExhausted,
                        message: "host language-server request timed out".to_owned(),
                        retryable: true,
                    })?
                    .map_err(|_| ProtocolError {
                        code: ErrorCode::Internal,
                        message: "host language-server response was dropped".to_owned(),
                        retryable: true,
                    })?
            }
        };
        validate_language_server_response(&response)?;
        Ok(Response::LanguageServer(response))
    }

    async fn handle_content_search_request(
        &self,
        actor: &ParticipantInfo,
        request: Request,
        canceled: &CancellationToken,
    ) -> Result<Response, ProtocolError> {
        self.endpoint
            .authorize_request(actor.id, actor.incarnation, &request)
            .await
            .map_err(|error| protocol_auth_error(&error))?;
        let Request::SearchContent(query) = request else {
            unreachable!("content-search handler received another request kind")
        };
        self.project
            .search_content(query, canceled.child_token())
            .await
            .map(Response::ContentSearch)
            .map_err(ProtocolError::from)
    }

    async fn peer_ids(&self) -> Vec<ParticipantId> {
        self.peers.lock().await.keys().copied().collect()
    }

    fn reply(
        &self,
        outbound: &PeerOutbound,
        disconnect: &CancellationToken,
        id: u64,
        result: Result<Response, ProtocolError>,
    ) -> bool {
        deliver_frame(outbound, disconnect, HostFrame::Response { id, result })
    }

    async fn broadcast(&self, frame: HostFrame) {
        let peers = self
            .peers
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for peer in peers {
            deliver_frame(&peer.outbound, &peer.disconnect, frame.clone());
        }
    }

    async fn deliver(&self, participant: ParticipantId, frame: HostFrame) {
        let peer = self.peers.lock().await.get(&participant).cloned();
        if let Some(peer) = peer {
            deliver_frame(&peer.outbound, &peer.disconnect, frame);
        }
    }

    async fn service_disconnect(self: &Arc<Self>, participant: &ParticipantInfo) {
        self.endpoint.disconnect(participant).await;
        self.presences.lock().await.remove(&participant.id);
        self.broadcast(HostFrame::Event(Event::ParticipantLeft {
            participant: participant.id,
        }))
        .await;
        self.schedule_lease_expiration(participant.clone());
    }

    fn schedule_lease_expiration(self: &Arc<Self>, participant: ParticipantInfo) {
        let expiration = CancellationToken::new();
        if let Some(previous) = self
            .lease_expirations
            .lock()
            .insert(participant.id, expiration.clone())
        {
            previous.cancel();
        }
        let service = Arc::downgrade(self);
        let shutdown = self.lease_expiration_shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = expiration.cancelled() => return,
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(crate::session::RESUME_TTL) => {}
            }
            let Some(service) = service.upgrade() else {
                return;
            };
            if service.endpoint.is_disconnected(&participant).await {
                if let Err(error) = service.project.release_participant(participant.id).await {
                    log::error!(
                        "failed to expire collaboration buffer leases for {:?}: {error}",
                        participant.id
                    );
                }
            }
        });
    }
}

async fn accept_connection(endpoint: Arc<HostEndpoint>) -> Result<Accepted, TransportError> {
    endpoint.accept(now_unix_secs()).await
}

async fn next_file_update(
    watch: &mut Option<crate::BackendFileWatch>,
) -> Option<crate::BackendFileUpdate> {
    match watch {
        Some(watch) => watch.recv().await,
        None => std::future::pending().await,
    }
}

fn deliver_frame(
    outbound: &PeerOutbound,
    disconnect: &CancellationToken,
    frame: HostFrame,
) -> bool {
    outbound.send(disconnect, frame)
}

fn protocol_auth_error(error: &AuthError) -> ProtocolError {
    let code = match error {
        AuthError::ProtocolMismatch { .. } => ErrorCode::ProtocolMismatch,
        AuthError::Expired => ErrorCode::ExpiredCredential,
        AuthError::Forbidden { .. } | AuthError::IdentityMismatch => ErrorCode::Forbidden,
        AuthError::ParticipantLimit | AuthError::InviteLimit => ErrorCode::ResourceExhausted,
        AuthError::InvalidName => ErrorCode::InvalidRequest,
        AuthError::StaleConnection => ErrorCode::Conflict,
        _ => ErrorCode::InvalidCredential,
    };
    ProtocolError {
        code,
        message: error.to_string(),
        retryable: matches!(error, AuthError::StaleConnection),
    }
}

fn transport_protocol_error(error: &TransportError) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::Internal,
        message: error.to_string(),
        retryable: false,
    }
}

fn id_for_invalid_frame() -> u64 {
    0
}

fn validate_language_server_request(
    server: &str,
    method: &str,
    params_bytes: usize,
) -> Result<(), ProtocolError> {
    if server.is_empty()
        || server.len() > MAX_LANGUAGE_SERVER_NAME_BYTES
        || server.chars().any(char::is_control)
    {
        return Err(invalid_request("invalid language-server name"));
    }
    if method.is_empty()
        || method.len() > MAX_LANGUAGE_SERVER_METHOD_BYTES
        || !method.is_ascii()
        || method.chars().any(char::is_control)
    {
        return Err(invalid_request("invalid language-server method"));
    }
    if params_bytes > MAX_LANGUAGE_SERVER_PAYLOAD_BYTES {
        return Err(resource_exhausted(
            "language-server request exceeds the payload limit",
        ));
    }
    Ok(())
}

fn validate_language_server_response(
    response: &LanguageServerResponse,
) -> Result<(), ProtocolError> {
    let bytes = match &response.result {
        Ok(value) => value.len(),
        Err(error) => {
            if error.message.len() > 16 * 1024 || error.message.chars().any(char::is_control) {
                return Err(invalid_request("invalid language-server error response"));
            }
            error.data.as_ref().map_or(0, |data| data.len())
        }
    };
    if bytes > MAX_LANGUAGE_SERVER_PAYLOAD_BYTES {
        return Err(resource_exhausted(
            "language-server response exceeds the payload limit",
        ));
    }
    Ok(())
}

fn invalid_request(message: &str) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::InvalidRequest,
        message: message.to_owned(),
        retryable: false,
    }
}

fn resource_exhausted(message: &str) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::ResourceExhausted,
        message: message.to_owned(),
        retryable: true,
    }
}

fn snapshot_transfer_missing() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::NotFound,
        message: "collaboration snapshot transfer expired or was replaced".to_owned(),
        retryable: true,
    }
}

fn sync_transfer_missing() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::NotFound,
        message: "collaboration sync transfer expired or was replaced".to_owned(),
        retryable: true,
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn log_transport_error(_error: &TransportError) {
    // The embedding application owns logging policy. Authentication failures are
    // expected network input and must not terminate the host accept loop.
}

#[derive(Debug, thiserror::Error)]
pub enum HostServiceError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("collaboration task failed: {0}")]
    Task(#[source] tokio::task::JoinError),
    #[error(transparent)]
    Project(#[from] crate::ProjectError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Backend, BackendFuture, BackendTransactionId, Client, ClientError, FileData, FileVersion,
        HostEndpoint, LocalBackend, ProjectError, ReplicaProject,
    };
    use helix_workspace::{FileTransaction, WorkspacePath};
    use serde_bytes::ByteBuf;
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex as StdMutex,
        },
        time::Duration,
    };

    struct MemoryBackend {
        path: WorkspacePath,
        bytes: StdMutex<Vec<u8>>,
        version: StdMutex<u64>,
        search_started: AtomicBool,
        search_canceled: AtomicBool,
    }

    impl MemoryBackend {
        fn new(path: WorkspacePath, bytes: &[u8]) -> Self {
            Self {
                path,
                bytes: StdMutex::new(bytes.to_vec()),
                version: StdMutex::new(1),
                search_started: AtomicBool::new(false),
                search_canceled: AtomicBool::new(false),
            }
        }

        fn version(value: u64) -> FileVersion {
            FileVersion::new(ByteBuf::from(value.to_le_bytes().to_vec())).unwrap()
        }
    }

    #[test]
    fn snapshot_transfers_release_their_exact_memory_budget() {
        let capacity = MAX_BUFFER_SNAPSHOT_TRANSFERS_BYTES;
        let budget = Arc::new(Semaphore::new(capacity));
        let mut transfers = SnapshotTransfers::new();
        let bytes = vec![9; MAX_BUFFER_SNAPSHOT_CHUNK_BYTES + 17];
        let reservation = transfers.reserve(&budget).unwrap();
        let response = transfers
            .start(
                Response::Buffer {
                    buffer: crate::BufferId(4),
                    epoch: 2,
                    total_bytes: bytes.len() as u64,
                    snapshot: bytes.clone().into(),
                    continuation: None,
                },
                reservation,
            )
            .unwrap();
        let Response::Buffer {
            continuation: Some(continuation),
            snapshot,
            ..
        } = response
        else {
            panic!("expected chunked snapshot")
        };
        assert_eq!(snapshot.len(), MAX_BUFFER_SNAPSHOT_CHUNK_BYTES);
        assert_eq!(budget.available_permits(), capacity - bytes.len());

        let Response::BufferSnapshotChunk {
            snapshot,
            continuation: None,
            ..
        } = transfers.next(continuation).unwrap()
        else {
            panic!("expected final snapshot chunk")
        };
        assert_eq!(snapshot.len(), 17);
        assert_eq!(budget.available_permits(), capacity);
    }

    #[test]
    fn sync_transfers_assemble_contiguously_and_release_their_budget() {
        let capacity = MAX_SYNC_MESSAGE_TRANSFERS_BYTES;
        let budget = Arc::new(Semaphore::new(capacity));
        let mut transfers = SyncTransfers::default();
        let transfer = SyncTransferId(9);
        let bytes = vec![5; MAX_SYNC_MESSAGE_CHUNK_BYTES + 23];
        assert!(matches!(
            transfers
                .start(
                    &budget,
                    transfer,
                    crate::BufferId(4),
                    2,
                    bytes.len() as u64,
                    bytes[..MAX_SYNC_MESSAGE_CHUNK_BYTES].to_vec(),
                )
                .unwrap(),
            SyncUpload::Pending
        ));
        assert_eq!(budget.available_permits(), capacity - bytes.len());

        let upload = transfers
            .next(
                transfer,
                MAX_SYNC_MESSAGE_CHUNK_BYTES as u64,
                bytes[MAX_SYNC_MESSAGE_CHUNK_BYTES..].to_vec(),
            )
            .unwrap();
        let SyncUpload::Complete { request, .. } = &upload else {
            panic!("expected complete sync upload")
        };
        assert!(matches!(
            request,
            Request::SyncBuffer {
                buffer: crate::BufferId(4),
                epoch: 2,
                message,
            } if message.as_ref() == bytes
        ));
        assert_eq!(budget.available_permits(), capacity - bytes.len());
        drop(upload);
        assert_eq!(budget.available_permits(), capacity);
    }

    impl Backend for MemoryBackend {
        fn list_files(
            &self,
            _options: helix_workspace::ScanOptions,
        ) -> BackendFuture<'_, Vec<WorkspacePath>> {
            Box::pin(async { Ok(vec![self.path.clone()]) })
        }

        fn read_file(&self, path: WorkspacePath) -> BackendFuture<'_, FileData> {
            Box::pin(async move {
                if path != self.path {
                    return Err(ProjectError::Backend("file not found".to_owned()));
                }
                Ok(FileData {
                    bytes: self.bytes.lock().unwrap().clone(),
                    version: Self::version(*self.version.lock().unwrap()),
                })
            })
        }

        fn search_content(
            &self,
            query: helix_workspace::ContentSearchQuery,
            canceled: CancellationToken,
        ) -> BackendFuture<'_, helix_workspace::ContentSearchPage> {
            Box::pin(async move {
                query
                    .validate()
                    .map_err(|message| ProjectError::InvalidContentSearch(message.to_owned()))?;
                if query.pattern == "__wait_for_cancel" {
                    self.search_started.store(true, Ordering::Release);
                    canceled.cancelled().await;
                    self.search_canceled.store(true, Ordering::Release);
                    return Err(ProjectError::Conflict(
                        "content search was canceled".to_owned(),
                    ));
                }
                if canceled.is_cancelled()
                    || query.cursor.file_offset != 0
                    || query.excluded_paths.contains(&self.path)
                {
                    return Ok(helix_workspace::ContentSearchPage {
                        entries: Vec::new(),
                        next: None,
                        scanned: 1,
                        done: true,
                    });
                }
                let parser = fff_search::QueryParser::new(fff_search::GrepConfig);
                let parsed = parser.parse(&query.pattern);
                let (matches, error) = fff_search::grep_bytes(
                    &parsed,
                    &fff_search::GrepSearchOptions {
                        smart_case: query.smart_case,
                        mode: fff_search::GrepMode::Regex,
                        ..fff_search::GrepSearchOptions::default()
                    },
                    &self.bytes.lock().unwrap(),
                );
                if let Some(error) = error {
                    return Err(ProjectError::InvalidContentSearch(error));
                }
                Ok(helix_workspace::ContentSearchPage {
                    entries: matches
                        .into_iter()
                        .take(usize::from(query.limit))
                        .map(|item| helix_workspace::ContentSearchEntry {
                            path: self.path.clone(),
                            line: item.line_number.saturating_sub(1),
                        })
                        .collect(),
                    next: None,
                    scanned: 1,
                    done: true,
                })
            })
        }

        fn path_exists(&self, path: WorkspacePath) -> BackendFuture<'_, bool> {
            Box::pin(async move { Ok(path == self.path) })
        }

        fn write_file(
            &self,
            path: WorkspacePath,
            expected: Option<FileVersion>,
            bytes: Vec<u8>,
        ) -> BackendFuture<'_, FileVersion> {
            Box::pin(async move {
                if path != self.path {
                    return Err(ProjectError::Backend("file not found".to_owned()));
                }
                let mut version = self.version.lock().unwrap();
                if expected.is_some_and(|expected| expected != Self::version(*version)) {
                    return Err(ProjectError::Conflict("version changed".to_owned()));
                }
                *self.bytes.lock().unwrap() = bytes;
                *version += 1;
                Ok(Self::version(*version))
            })
        }

        fn apply_file_transaction(
            &self,
            _transaction: FileTransaction,
        ) -> BackendFuture<'_, BackendTransactionId> {
            Box::pin(async { Ok(BackendTransactionId(1)) })
        }

        fn undo_file_transaction(
            &self,
            _transaction: BackendTransactionId,
        ) -> BackendFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn peer_backpressure_drops_presence_but_disconnects_before_reliable_loss() {
        let (reliable, _inbound) = mpsc::channel(1);
        let outbound = PeerOutbound::new(reliable);
        outbound
            .reliable
            .try_send(HostFrame::Response {
                id: 1,
                result: Ok(Response::Unit),
            })
            .unwrap();
        let disconnect = CancellationToken::new();
        let presence = Event::Presence(crate::Presence {
            participant: ParticipantId([1; 16]),
            buffer: crate::BufferId(1),
            cursor: None,
            selection: None,
            viewport: None,
            active_view: None,
        });

        assert!(deliver_frame(
            &outbound,
            &disconnect,
            HostFrame::Event(presence)
        ));
        assert!(!disconnect.is_cancelled());
        assert!(!deliver_frame(
            &outbound,
            &disconnect,
            HostFrame::Event(Event::BufferSaved {
                buffer: crate::BufferId(1),
                version: FileVersion::new(ByteBuf::new()).unwrap(),
            })
        ));
        assert!(disconnect.is_cancelled());
    }

    #[test]
    fn peer_backpressure_merges_worktree_deltas_without_rescan() {
        let (reliable, _inbound) = mpsc::channel(1);
        let outbound = PeerOutbound::new(reliable);
        outbound
            .reliable
            .try_send(HostFrame::Response {
                id: 1,
                result: Ok(Response::Unit),
            })
            .unwrap();
        let disconnect = CancellationToken::new();
        for (revision, kind) in [
            (1, helix_workspace::FileChangeKind::Created),
            (2, helix_workspace::FileChangeKind::Modified),
        ] {
            assert!(deliver_frame(
                &outbound,
                &disconnect,
                HostFrame::Event(Event::WorktreeChanged {
                    file_revision: revision,
                    changes: vec![helix_workspace::FileChange {
                        path: WorkspacePath::from_slash_path("src/main.rs").unwrap(),
                        kind,
                    }],
                    rescan: false,
                }),
            ));
        }
        assert!(!disconnect.is_cancelled());
        assert!(matches!(
            outbound.take_coalesced(),
            Some(HostFrame::Event(Event::WorktreeChanged {
                file_revision: 2,
                changes,
                rescan: false,
            })) if matches!(changes.as_slice(), [helix_workspace::FileChange {
                kind: helix_workspace::FileChangeKind::Modified,
                ..
            }])
        ));
    }

    #[test]
    fn peer_backpressure_coalesces_latest_language_server_diagnostics() {
        let (reliable, _inbound) = mpsc::channel(1);
        let outbound = PeerOutbound::new(reliable);
        let disconnect = CancellationToken::new();
        let diagnostics = |params: &[u8]| {
            HostFrame::Event(Event::LanguageServerDiagnostics(
                LanguageServerDiagnostics {
                    path: WorkspacePath::from_slash_path("src/main.rs").unwrap(),
                    server: "rust-analyzer".to_owned(),
                    params: ByteBuf::from(params.to_vec()),
                },
            ))
        };

        assert!(deliver_frame(&outbound, &disconnect, diagnostics(b"old")));
        assert!(deliver_frame(&outbound, &disconnect, diagnostics(b"new")));
        assert!(!disconnect.is_cancelled());
        assert!(matches!(
            outbound.take_coalesced(),
            Some(HostFrame::Event(Event::LanguageServerDiagnostics(
                LanguageServerDiagnostics { params, .. }
            ))) if params.as_ref() == b"new"
        ));
        assert!(outbound.take_coalesced().is_none());
    }

    #[test]
    fn peer_coalesced_delivery_is_fair_under_continuous_worktree_updates() {
        let (reliable, _inbound) = mpsc::channel(1);
        let outbound = PeerOutbound::new(reliable);
        let disconnect = CancellationToken::new();
        let worktree = |revision| {
            HostFrame::Event(Event::WorktreeChanged {
                file_revision: revision,
                changes: Vec::new(),
                rescan: true,
            })
        };
        let presence = HostFrame::Event(Event::Presence(crate::Presence {
            participant: ParticipantId([1; 16]),
            buffer: crate::BufferId(1),
            cursor: None,
            selection: None,
            viewport: None,
            active_view: None,
        }));
        let diagnostics = HostFrame::Event(Event::LanguageServerDiagnostics(
            LanguageServerDiagnostics {
                path: WorkspacePath::from_slash_path("src/main.rs").unwrap(),
                server: "rust-analyzer".to_owned(),
                params: ByteBuf::from(vec![1]),
            },
        ));
        let refresh = HostFrame::Event(Event::LanguageServerRefresh(LanguageServerRefresh {
            server: "rust-analyzer".to_owned(),
            kind: crate::LanguageServerRefreshKind::SemanticTokens,
        }));

        for frame in [worktree(1), presence, diagnostics, refresh] {
            assert!(deliver_frame(&outbound, &disconnect, frame));
        }
        assert!(matches!(
            outbound.take_coalesced(),
            Some(HostFrame::Event(Event::WorktreeChanged { .. }))
        ));

        assert!(deliver_frame(&outbound, &disconnect, worktree(2)));
        assert!(matches!(
            outbound.take_coalesced(),
            Some(HostFrame::Event(Event::Presence(_)))
        ));

        assert!(deliver_frame(&outbound, &disconnect, worktree(3)));
        assert!(matches!(
            outbound.take_coalesced(),
            Some(HostFrame::Event(Event::LanguageServerDiagnostics(_)))
        ));

        assert!(deliver_frame(&outbound, &disconnect, worktree(4)));
        assert!(matches!(
            outbound.take_coalesced(),
            Some(HostFrame::Event(Event::LanguageServerRefresh(_)))
        ));
        assert!(!disconnect.is_cancelled());
    }

    #[tokio::test]
    async fn canceled_language_server_requests_wake_host_work() {
        let (response, receiver) = oneshot::channel();
        let mut request = HostLanguageServerRequest {
            participant: ParticipantInfo {
                id: ParticipantId([1; 16]),
                name: "guest".to_owned(),
                role: Role::Read,
                incarnation: 1,
            },
            buffer: crate::BufferId(1),
            path: WorkspacePath::from_slash_path("src/main.rs").unwrap(),
            text: String::new(),
            server: "rust-analyzer".to_owned(),
            method: "textDocument/hover".to_owned(),
            params: Vec::new(),
            response,
        };
        drop(receiver);

        tokio::time::timeout(Duration::from_millis(100), request.canceled())
            .await
            .expect("host cancellation signal");
        assert!(request.is_canceled());
    }

    #[tokio::test]
    async fn canceled_content_search_stops_host_work_and_keeps_session_healthy() {
        let path = WorkspacePath::from_slash_path("src/main.rs").unwrap();
        let backend = Arc::new(MemoryBackend::new(path, b"abc"));
        let endpoint = Arc::new(
            HostEndpoint::bind(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1:0".parse().unwrap(),
                "owner",
            )
            .unwrap(),
        );
        let owner = endpoint.owner().await;
        let project = Arc::new(Project::new("project", owner, backend.clone()).unwrap());
        let host = HostHandle::start(endpoint, project).await.unwrap();
        let invite = host.invite(Role::Read, now() + 60, now()).await.unwrap();
        let guest = Client::connect(invite, "reader").await.unwrap();
        let requests = guest.request_handle();
        let canceled = CancellationToken::new();
        let search = tokio::spawn({
            let canceled = canceled.clone();
            async move {
                requests
                    .request_cancellable(
                        Request::SearchContent(helix_workspace::ContentSearchQuery {
                            root: WorkspacePath::root(),
                            pattern: "__wait_for_cancel".to_owned(),
                            smart_case: true,
                            options: helix_workspace::ScanOptions::default(),
                            excluded_paths: Vec::new(),
                            cursor: helix_workspace::ContentSearchCursor::default(),
                            limit: 1,
                        }),
                        canceled,
                    )
                    .await
            }
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while !backend.search_started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("host content search start");
        canceled.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), search)
            .await
            .expect("canceled client request")
            .expect("content-search task");
        assert!(matches!(result, Err(ClientError::Canceled)));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !backend.search_canceled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("host content search cancellation");
        assert!(matches!(
            guest.request(Request::ProjectInfo).await.unwrap(),
            Response::Project(_)
        ));

        guest.shutdown().await.unwrap();
        host.shutdown().await.unwrap();
    }

    fn now() -> u64 {
        now_unix_secs()
    }

    #[tokio::test]
    async fn host_streams_external_worktree_changes_to_guests() {
        let root = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalBackend::open(root.path()).await.unwrap());
        let endpoint = Arc::new(
            HostEndpoint::bind(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1:0".parse().unwrap(),
                "owner",
            )
            .unwrap(),
        );
        let owner = endpoint.owner().await;
        let project = Arc::new(Project::new("project", owner, backend).unwrap());
        let host = HostHandle::start(endpoint, project).await.unwrap();
        let invite = host.invite(Role::Write, now() + 60, now()).await.unwrap();
        let mut guest = Client::connect(invite, "writer").await.unwrap();

        std::fs::write(root.path().join("external.txt"), b"changed").unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = guest.next_event().await.unwrap();
                if matches!(
                    &event,
                    Event::WorktreeChanged {
                        file_revision,
                        changes,
                        rescan,
                    } if *file_revision > 0
                        && (*rescan
                            || changes
                                .iter()
                                .any(|change| change.path.to_string() == "external.txt"))
                ) {
                    break event;
                }
            }
        })
        .await
        .expect("host worktree event");
        let expected = matches!(
            &event,
            Event::WorktreeChanged {
                file_revision,
                changes,
                rescan,
            } if *file_revision > 0
                && (*rescan
                    || changes.iter().any(|change| change.path.to_string() == "external.txt"))
        );
        assert!(expected, "unexpected worktree event: {event:?}");

        tokio::time::timeout(Duration::from_secs(2), guest.shutdown())
            .await
            .expect("guest shutdown timed out")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), host.shutdown())
            .await
            .expect("host shutdown timed out")
            .unwrap();
    }

    #[tokio::test]
    async fn follow_returns_the_leaders_current_buffer_location() {
        let path = WorkspacePath::from_slash_path("src/main.rs").unwrap();
        let backend = Arc::new(MemoryBackend::new(path.clone(), b"abc"));
        let endpoint = Arc::new(
            HostEndpoint::bind(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1:0".parse().unwrap(),
                "owner",
            )
            .unwrap(),
        );
        let owner = endpoint.owner().await;
        let project = Arc::new(Project::new("project", owner, backend).unwrap());
        let host = HostHandle::start(endpoint, project).await.unwrap();
        let leader_invite = host.invite(Role::Write, now() + 60, now()).await.unwrap();
        let follower_invite = host.invite(Role::Read, now() + 60, now()).await.unwrap();
        let leader = Client::connect(leader_invite, "leader").await.unwrap();
        let mut follower = Client::connect(follower_invite, "follower").await.unwrap();
        let Response::Project(info) = leader.request(Request::ProjectInfo).await.unwrap() else {
            panic!("expected project info");
        };
        let mut replica = ReplicaProject::new(leader.participant().id, &info);
        let opened = leader
            .request(Request::OpenBuffer { path: path.clone() })
            .await
            .unwrap();
        let buffer = replica.install(opened).unwrap();
        let cursor = replica
            .anchor(buffer, 2, crate::AnchorAffinity::Before)
            .unwrap();
        leader
            .request(Request::PublishPresence(crate::Presence {
                participant: leader.participant().id,
                buffer,
                cursor: Some(cursor),
                selection: None,
                viewport: None,
                active_view: Some(crate::ViewId([9; 16])),
            }))
            .await
            .unwrap();

        let response = follower
            .request(Request::Follow {
                participant: leader.participant().id,
            })
            .await
            .unwrap();
        assert!(matches!(
            response,
            Response::Following {
                location: Some(crate::FollowLocation {
                    path: followed_path,
                    presence: crate::Presence {
                        buffer: followed_buffer,
                        active_view: Some(followed_view),
                        ..
                    },
                }),
            } if followed_path == path
                && followed_buffer == buffer
                && followed_view == crate::ViewId([9; 16])
        ));

        let followed = follower
            .request(Request::OpenBuffer { path: path.clone() })
            .await
            .unwrap();
        assert!(matches!(
            followed,
            Response::Buffer {
                buffer: followed_buffer,
                ..
            } if followed_buffer == buffer
        ));
        assert_eq!(
            leader
                .request(Request::CloseBuffer { buffer })
                .await
                .unwrap(),
            Response::Unit
        );
        let cleared = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = follower.next_event().await.unwrap();
                if matches!(
                    event,
                    Event::PresenceCleared {
                        participant,
                        buffer: cleared_buffer,
                    } if participant == leader.participant().id && cleared_buffer == buffer
                ) {
                    break event;
                }
            }
        })
        .await
        .expect("presence clear after closing the leader buffer");
        assert!(matches!(cleared, Event::PresenceCleared { .. }));
        assert!(matches!(
            follower
                .request(Request::Follow {
                    participant: leader.participant().id,
                })
                .await
                .unwrap(),
            Response::Following { location: None }
        ));
        follower
            .request(Request::CloseBuffer { buffer })
            .await
            .unwrap();

        leader.shutdown().await.unwrap();
        follower.shutdown().await.unwrap();
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn guest_session_release_closes_the_host_lease_without_blocking_the_caller() {
        let path = WorkspacePath::from_slash_path("src/main.rs").unwrap();
        let backend = Arc::new(MemoryBackend::new(path.clone(), b"abc"));
        let endpoint = Arc::new(
            HostEndpoint::bind(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1:0".parse().unwrap(),
                "owner",
            )
            .unwrap(),
        );
        let owner = endpoint.owner().await;
        let project = Arc::new(Project::new("project", owner, backend).unwrap());
        let host = HostHandle::start(endpoint, project.clone()).await.unwrap();
        let invite = host.invite(Role::Write, now() + 60, now()).await.unwrap();
        let session = crate::GuestSession::join(invite, "writer").await.unwrap();
        let handle = session.handle();
        let opened = handle.open(path).await.unwrap();

        handle.release(opened.buffer);

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let closed = {
                    let state = project.clone().reserve_file_state(Vec::new()).await;
                    state
                        .state
                        .open_buffers
                        .iter()
                        .all(|open| open.buffer != opened.buffer)
                };
                if closed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("guest buffer release reached the host");
        session.leave().await.unwrap();
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn host_routes_replication_and_save_but_denies_read_only_writes() {
        let path = WorkspacePath::from_slash_path("src/main.rs").unwrap();
        let backend = Arc::new(MemoryBackend::new(path.clone(), b"abc"));
        let endpoint = Arc::new(
            HostEndpoint::bind(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1:0".parse().unwrap(),
                "owner",
            )
            .unwrap(),
        );
        let owner = endpoint.owner().await;
        let project = Arc::new(Project::new("project", owner, backend.clone()).unwrap());
        let host = HostHandle::start(endpoint, project.clone()).await.unwrap();
        let invite = host.invite(Role::Write, now() + 60, now()).await.unwrap();
        let mut guest = Client::connect(invite, "writer").await.unwrap();
        let Response::Project(info) = guest.request(Request::ProjectInfo).await.unwrap() else {
            panic!("expected project info");
        };
        assert_eq!(info.owner, owner);
        let mut replica = ReplicaProject::new(guest.participant().id, &info);
        let response = guest
            .request(Request::OpenBuffer { path: path.clone() })
            .await
            .unwrap();
        let buffer = replica.install(response).unwrap();
        assert!(matches!(
            guest
                .request(Request::SyncBuffer {
                    buffer,
                    epoch: 0,
                    message: ByteBuf::new(),
                })
                .await
                .unwrap(),
            Response::Unit
        ));
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), guest.next_event())
                .await
                .unwrap()
                .unwrap();
            if matches!(
                event,
                Event::ResyncRequired {
                    buffer: stale_buffer,
                    epoch: 1,
                } if stale_buffer == buffer
            ) {
                break;
            }
        }
        let sync = replica.replace(buffer, 1..1, "x").unwrap().unwrap();
        guest.request(sync).await.unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), guest.next_event())
                .await
                .unwrap()
                .unwrap();
            let Some(update) = replica.apply(event).unwrap() else {
                continue;
            };
            let Some(reply) = update.reply else {
                break;
            };
            guest.request(reply).await.unwrap();
        }
        assert_eq!(replica.text(buffer).unwrap(), "axbc");
        guest
            .request(Request::SaveBuffer {
                buffer,
                overwrite: false,
            })
            .await
            .unwrap();
        assert_eq!(&*backend.bytes.lock().unwrap(), b"axbc");

        let invite = host.invite(Role::Read, now() + 60, now()).await.unwrap();
        let reader = Client::connect(invite, "reader").await.unwrap();
        let denied = reader
            .request(Request::SaveBuffer {
                buffer,
                overwrite: false,
            })
            .await
            .unwrap_err();
        assert!(matches!(
            denied,
            ClientError::Protocol(ProtocolError {
                code: ErrorCode::Forbidden,
                ..
            })
        ));

        let renamed = WorkspacePath::from_slash_path("src/renamed.rs").unwrap();
        let transaction = FileTransaction {
            operations: vec![helix_workspace::FileOperation::Rename {
                from: path.clone(),
                to: renamed.clone(),
                overwrite: false,
            }],
        };
        host.project_publisher()
            .reserve_file_mutation(transaction.clone())
            .await
            .unwrap()
            .commit()
            .await;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), guest.next_event())
                .await
                .unwrap()
                .unwrap();
            if event
                == (Event::FilesChanged {
                    file_revision: 1,
                    transaction: transaction.clone(),
                    undone: false,
                })
            {
                break;
            }
        }
        let outcome = project
            .handle(
                owner,
                Request::OpenBuffer { path: renamed },
                vec![owner],
                Vec::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome.response,
            Response::Buffer {
                buffer: opened,
                ..
            } if opened == buffer
        ));

        guest.shutdown().await.unwrap();
        reader.shutdown().await.unwrap();
        host.shutdown().await.unwrap();
    }
}
