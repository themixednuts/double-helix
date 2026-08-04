use crate::{
    BufferId, Client, ClientError, ConnectCode, ConnectionState, Event, ParticipantInfo, Presence,
    ProjectInfo, ProjectState, ReplicaError, ReplicaProject, Request, Response, TextChange,
};
use helix_workspace::{ContentSearchPage, ContentSearchQuery, ScanOptions, WorkspacePath};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, Notify},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const COMMAND_CAPACITY: usize = 256;
const UPDATE_CAPACITY: usize = 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_LIST_CACHE_TTL: Duration = Duration::from_secs(2);
const RELEASE_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_PENDING_EDIT_BATCHES_PER_BUFFER: usize = 1024;
const MAX_PENDING_INSERT_BYTES_PER_BUFFER: usize = 4 * 1024 * 1024;

enum PendingLocalEdits {
    Changes {
        batches: Vec<Vec<TextChange>>,
        insert_bytes: usize,
    },
    Snapshot(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedBuffer {
    pub buffer: BufferId,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPresence {
    pub participant: crate::ParticipantId,
    pub buffer: BufferId,
    pub cursor: Option<usize>,
    pub selection: Option<(usize, usize)>,
    pub viewport: Option<usize>,
    pub active_view: Option<crate::ViewId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPresence {
    pub buffer: BufferId,
    pub cursor: Option<usize>,
    pub selection: Option<(usize, usize)>,
    pub viewport: Option<usize>,
    pub active_view: Option<crate::ViewId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFollowLocation {
    pub path: WorkspacePath,
    pub presence: ResolvedPresence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestSessionUpdate {
    TextChanged {
        buffer: BufferId,
        changes: Vec<TextChange>,
    },
    Snapshot {
        buffer: BufferId,
        text: String,
    },
    ParticipantJoined(ParticipantInfo),
    ParticipantLeft(crate::ParticipantId),
    RoleChanged {
        participant: crate::ParticipantId,
        role: crate::Role,
    },
    Presence(ResolvedPresence),
    PresenceCleared {
        participant: crate::ParticipantId,
        buffer: BufferId,
    },
    FollowRequested {
        follower: crate::ParticipantId,
        leader: crate::ParticipantId,
    },
    Following {
        participant: crate::ParticipantId,
        location: Option<ResolvedFollowLocation>,
    },
    BufferSaved {
        buffer: BufferId,
        version: crate::FileVersion,
    },
    ProjectState(ProjectState),
    FilesChanged {
        transaction: helix_workspace::FileTransaction,
        undone: bool,
    },
    WorktreeChanged {
        changes: Vec<helix_workspace::FileChange>,
        rescan: bool,
    },
    LanguageServerDiagnostics(crate::LanguageServerDiagnostics),
    LanguageServerRefresh(crate::LanguageServerRefresh),
    Connection(ConnectionState),
    Error(String),
}

enum Command {
    Open {
        path: WorkspacePath,
        response: oneshot::Sender<Result<OpenedBuffer, GuestSessionError>>,
    },
    Edit {
        buffer: BufferId,
        changes: Vec<TextChange>,
        response: oneshot::Sender<Result<(), GuestSessionError>>,
    },
    Flush {
        buffer: BufferId,
        response: oneshot::Sender<Result<(), GuestSessionError>>,
    },
    Save {
        buffer: BufferId,
        overwrite: bool,
        response: oneshot::Sender<Result<crate::FileVersion, GuestSessionError>>,
    },
    ApplyFileTransaction {
        transaction: helix_workspace::FileTransaction,
        response: oneshot::Sender<Result<bool, GuestSessionError>>,
    },
    ReplayFileTransaction {
        redo: bool,
        response: oneshot::Sender<Result<bool, GuestSessionError>>,
    },
    Follow {
        participant: crate::ParticipantId,
        response: oneshot::Sender<Result<(), GuestSessionError>>,
    },
    Invite {
        role: crate::Role,
        expires_unix_secs: u64,
        response: oneshot::Sender<Result<ConnectCode, GuestSessionError>>,
    },
    SetRole {
        participant: crate::ParticipantId,
        role: crate::Role,
        response: oneshot::Sender<Result<(), GuestSessionError>>,
    },
    RemoveParticipant {
        participant: crate::ParticipantId,
        response: oneshot::Sender<Result<(), GuestSessionError>>,
    },
    Leave {
        response: oneshot::Sender<Result<(), GuestSessionError>>,
    },
}

#[derive(Clone)]
pub struct GuestSessionHandle {
    commands: mpsc::Sender<Command>,
    shared: Arc<GuestSessionShared>,
}

struct GuestSessionShared {
    participant: StdRwLock<ParticipantInfo>,
    project: StdRwLock<ProjectInfo>,
    local_edits: StdMutex<HashMap<BufferId, PendingLocalEdits>>,
    local_edit_wake: Notify,
    local_presence: StdMutex<Option<LocalPresence>>,
    local_presence_wake: Notify,
    open_buffers: StdMutex<HashSet<BufferId>>,
    pending_releases: StdMutex<HashSet<BufferId>>,
    release_wake: Notify,
    requests: crate::client::ClientRequestHandle,
    file_lists: StdMutex<HashMap<ScanOptions, CachedFileList>>,
}

impl std::fmt::Debug for GuestSessionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuestSessionHandle")
            .field("participant", &self.participant().id)
            .field("project", &self.project().id)
            .finish_non_exhaustive()
    }
}

impl GuestSessionHandle {
    pub fn participant(&self) -> ParticipantInfo {
        self.shared
            .participant
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn project(&self) -> ProjectInfo {
        self.shared
            .project
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub async fn list_files(
        &self,
        options: ScanOptions,
    ) -> Result<Arc<[WorkspacePath]>, GuestSessionError> {
        {
            let lists = self
                .shared
                .file_lists
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = lists
                .get(&options)
                .filter(|cached| cached.loaded.elapsed() < FILE_LIST_CACHE_TTL)
            {
                return Ok(cached.files.clone());
            }
        }
        let files = load_file_list(&self.shared.requests, options).await?;
        self.shared
            .file_lists
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                options,
                CachedFileList {
                    files: files.clone(),
                    loaded: std::time::Instant::now(),
                },
            );
        Ok(files)
    }

    pub async fn search_content_page(
        &self,
        query: ContentSearchQuery,
        canceled: CancellationToken,
    ) -> Result<ContentSearchPage, GuestSessionError> {
        query
            .validate()
            .map_err(|message| GuestSessionError::InvalidSearch(message.to_owned()))?;
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.shared
                .requests
                .request_cancellable(Request::SearchContent(query), canceled),
        )
        .await
        .map_err(|_| GuestSessionError::Timeout)??;
        let Response::ContentSearch(page) = response else {
            return Err(GuestSessionError::UnexpectedResponse("ContentSearch"));
        };
        Ok(page)
    }

    pub async fn open(&self, path: WorkspacePath) -> Result<OpenedBuffer, GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::Open { path, response })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    pub fn release(&self, buffer: BufferId) {
        self.shared
            .open_buffers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&buffer);
        self.shared
            .local_edits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&buffer);
        self.shared
            .pending_releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(buffer);
        self.shared.release_wake.notify_one();
    }

    pub async fn path_exists(&self, path: WorkspacePath) -> Result<bool, GuestSessionError> {
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.shared.requests.request(Request::PathExists { path }),
        )
        .await
        .map_err(|_| GuestSessionError::Timeout)??
        {
            Response::PathExists(exists) => Ok(exists),
            _ => Err(GuestSessionError::UnexpectedResponse("PathExists")),
        }
    }

    pub async fn language_server_request(
        &self,
        buffer: BufferId,
        server: String,
        method: String,
        params: Vec<u8>,
    ) -> Result<Result<Vec<u8>, crate::LanguageServerError>, GuestSessionError> {
        if params.len() > crate::MAX_LANGUAGE_SERVER_PAYLOAD_BYTES {
            return Err(GuestSessionError::RequestTooLarge);
        }
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.shared.requests.request(Request::LanguageServer {
                buffer,
                server,
                method,
                params: params.into(),
            }),
        )
        .await
        .map_err(|_| GuestSessionError::Timeout)??
        {
            Response::LanguageServer(response) => {
                Ok(response.result.map(serde_bytes::ByteBuf::into_vec))
            }
            _ => Err(GuestSessionError::UnexpectedResponse("LanguageServer")),
        }
    }

    pub async fn edit(
        &self,
        buffer: BufferId,
        changes: Vec<TextChange>,
    ) -> Result<(), GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::Edit {
                buffer,
                changes,
                response,
            })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    /// Queue UI-thread edits without waiting on the network. The snapshot is
    /// evaluated only when sustained backpressure requires coalescing.
    pub fn queue_edit(
        &self,
        buffer: BufferId,
        changes: Vec<TextChange>,
        snapshot: impl FnOnce() -> String,
    ) {
        if !self
            .shared
            .open_buffers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&buffer)
        {
            return;
        }
        let insert_bytes = changes.iter().map(|change| change.insert.len()).sum();
        let mut pending = self
            .shared
            .local_edits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match pending.entry(buffer) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(PendingLocalEdits::Changes {
                    batches: vec![changes],
                    insert_bytes,
                });
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => match entry.get_mut() {
                PendingLocalEdits::Changes {
                    batches,
                    insert_bytes: pending_bytes,
                } if batches.len() < MAX_PENDING_EDIT_BATCHES_PER_BUFFER
                    && pending_bytes.saturating_add(insert_bytes)
                        <= MAX_PENDING_INSERT_BYTES_PER_BUFFER =>
                {
                    batches.push(changes);
                    *pending_bytes += insert_bytes;
                }
                edits => *edits = PendingLocalEdits::Snapshot(snapshot()),
            },
        }
        drop(pending);
        self.shared.local_edit_wake.notify_one();
    }

    /// Replace a local replica from the newest editor snapshot. This is used
    /// when a host document joins the shared graph after it was already open;
    /// the replica resolves the replacement against its current CRDT state.
    pub fn queue_snapshot(&self, buffer: BufferId, text: String) {
        if !self
            .shared
            .open_buffers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&buffer)
        {
            return;
        }
        self.shared
            .local_edits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(buffer, PendingLocalEdits::Snapshot(text));
        self.shared.local_edit_wake.notify_one();
    }

    pub async fn flush(&self, buffer: BufferId) -> Result<(), GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::Flush { buffer, response })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    pub async fn save(
        &self,
        buffer: BufferId,
        overwrite: bool,
    ) -> Result<crate::FileVersion, GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::Save {
                buffer,
                overwrite,
                response,
            })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    pub async fn apply_file_transaction(
        &self,
        transaction: helix_workspace::FileTransaction,
    ) -> Result<bool, GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::ApplyFileTransaction {
                transaction,
                response,
            })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    pub async fn replay_file_transaction(&self, redo: bool) -> Result<bool, GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::ReplayFileTransaction { redo, response })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    pub fn queue_presence(&self, presence: LocalPresence) {
        if !self
            .shared
            .open_buffers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&presence.buffer)
        {
            return;
        }
        *self
            .shared
            .local_presence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(presence);
        self.shared.local_presence_wake.notify_one();
    }

    pub async fn follow(&self, participant: crate::ParticipantId) -> Result<(), GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::Follow {
                participant,
                response,
            })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    pub async fn invite(
        &self,
        role: crate::Role,
        expires_unix_secs: u64,
    ) -> Result<ConnectCode, GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::Invite {
                role,
                expires_unix_secs,
                response,
            })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    pub async fn set_role(
        &self,
        participant: crate::ParticipantId,
        role: crate::Role,
    ) -> Result<(), GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::SetRole {
                participant,
                role,
                response,
            })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    pub async fn remove_participant(
        &self,
        participant: crate::ParticipantId,
    ) -> Result<(), GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::RemoveParticipant {
                participant,
                response,
            })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }

    pub async fn leave(&self) -> Result<(), GuestSessionError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command::Leave { response })
            .await
            .map_err(|_| GuestSessionError::Closed)?;
        receiver.await.map_err(|_| GuestSessionError::Closed)?
    }
}

pub struct GuestSession {
    handle: GuestSessionHandle,
    updates: mpsc::Receiver<GuestSessionUpdate>,
    task: Option<JoinHandle<()>>,
}

impl GuestSession {
    pub async fn join(
        code: ConnectCode,
        name: impl Into<String>,
    ) -> Result<Self, GuestSessionError> {
        let client = Client::connect(code, name).await?;
        let participant = client.participant();
        let Response::Project(project) = request(&client, Request::ProjectInfo).await? else {
            return Err(GuestSessionError::UnexpectedResponse("Project"));
        };
        let replica = ReplicaProject::new(participant.id, &project);
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (updates_tx, updates_rx) = mpsc::channel(UPDATE_CAPACITY);
        let shared = Arc::new(GuestSessionShared {
            participant: StdRwLock::new(participant),
            project: StdRwLock::new(project),
            local_edits: StdMutex::new(HashMap::new()),
            local_edit_wake: Notify::new(),
            local_presence: StdMutex::new(None),
            local_presence_wake: Notify::new(),
            open_buffers: StdMutex::new(HashSet::new()),
            pending_releases: StdMutex::new(HashSet::new()),
            release_wake: Notify::new(),
            requests: client.request_handle(),
            file_lists: StdMutex::new(HashMap::new()),
        });
        let handle = GuestSessionHandle {
            commands: commands_tx,
            shared: shared.clone(),
        };
        let task = tokio::spawn(run(client, replica, commands_rx, updates_tx, shared));
        Ok(Self {
            handle,
            updates: updates_rx,
            task: Some(task),
        })
    }

    pub fn handle(&self) -> GuestSessionHandle {
        self.handle.clone()
    }

    pub async fn next_update(&mut self) -> Option<GuestSessionUpdate> {
        self.updates.recv().await
    }

    pub async fn leave(mut self) -> Result<(), GuestSessionError> {
        let result = self.handle.leave().await;
        if let Some(task) = self.task.take() {
            task.await.map_err(GuestSessionError::Task)?;
        }
        result
    }
}

impl Drop for GuestSession {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run(
    mut client: Client,
    mut replica: ReplicaProject,
    mut commands: mpsc::Receiver<Command>,
    updates: mpsc::Sender<GuestSessionUpdate>,
    shared: Arc<GuestSessionShared>,
) {
    let mut state = client.subscribe_connection_state();
    let mut last_presence = None;
    let mut file_revision = None;
    let mut release_retry_at = None;
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    let _ = client.shutdown().await;
                    return;
                };
                if handle_command(
                    command,
                    &client,
                    &mut replica,
                    &shared,
                    &updates,
                ).await {
                    let _ = client.shutdown().await;
                    return;
                }
            }
            event = client.next_event() => {
                let Some(event) = event else {
                    return;
                };
                if let Err(error) = handle_event(
                    event,
                    &client,
                    &mut replica,
                    &shared,
                    &updates,
                    &mut file_revision,
                ).await {
                    send_update(&updates, GuestSessionUpdate::Error(error.to_string())).await;
                }
            }
            _ = shared.local_edit_wake.notified() => {
                let pending = {
                    let mut pending = shared.local_edits
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    std::mem::take(&mut *pending)
                };
                for (buffer, edits) in pending {
                    if let Err(error) = flush_edits(
                        buffer,
                        edits,
                        &client,
                        &mut replica,
                        &shared.open_buffers,
                    ).await {
                        send_update(&updates, GuestSessionUpdate::Error(error.to_string())).await;
                    }
                }
            }
            _ = shared.local_presence_wake.notified() => {
                let presence = shared.local_presence
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(presence) = presence {
                    if !shared.open_buffers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains(&presence.buffer)
                    {
                        continue;
                    }
                    last_presence = Some(presence.clone());
                    if let Err(error) = publish_local_presence(&client, &replica, presence).await {
                        send_update(&updates, GuestSessionUpdate::Error(error.to_string())).await;
                    }
                }
            }
            _ = shared.release_wake.notified() => {
                if !matches!(&*state.borrow(), ConnectionState::Connected(_)) {
                    release_retry_at = None;
                    continue;
                }
                match flush_releases(
                    &client,
                    &mut replica,
                    &shared.open_buffers,
                    &shared.pending_releases,
                ).await {
                    Ok(()) => release_retry_at = None,
                    Err(error) => {
                        release_retry_at = Some(tokio::time::Instant::now() + RELEASE_RETRY_DELAY);
                        log::debug!("collaboration buffer release retry deferred: {error}");
                    }
                }
                if last_presence
                    .as_ref()
                    .is_some_and(|presence| !replica.contains(presence.buffer))
                {
                    last_presence = None;
                }
            }
            _ = tokio::time::sleep_until(
                release_retry_at.unwrap_or_else(tokio::time::Instant::now)
            ), if release_retry_at.is_some() => {
                if !matches!(&*state.borrow(), ConnectionState::Connected(_)) {
                    release_retry_at = None;
                    continue;
                }
                match flush_releases(
                    &client,
                    &mut replica,
                    &shared.open_buffers,
                    &shared.pending_releases,
                ).await {
                    Ok(()) => release_retry_at = None,
                    Err(error) => {
                        release_retry_at = Some(tokio::time::Instant::now() + RELEASE_RETRY_DELAY);
                        log::debug!("collaboration buffer release retry deferred: {error}");
                    }
                }
                if last_presence
                    .as_ref()
                    .is_some_and(|presence| !replica.contains(presence.buffer))
                {
                    last_presence = None;
                }
            }
            changed = state.changed() => {
                if changed.is_err() {
                    return;
                }
                let current = state.borrow().clone();
                send_update(&updates, GuestSessionUpdate::Connection(current.clone())).await;
                if matches!(current, ConnectionState::Connected(_)) {
                    if let Err(error) = flush_releases(
                        &client,
                        &mut replica,
                        &shared.open_buffers,
                        &shared.pending_releases,
                    ).await {
                        release_retry_at = Some(tokio::time::Instant::now() + RELEASE_RETRY_DELAY);
                        send_update(&updates, GuestSessionUpdate::Error(error.to_string())).await;
                    }
                    if last_presence
                        .as_ref()
                        .is_some_and(|presence| !replica.contains(presence.buffer))
                    {
                        last_presence = None;
                    }
                    match replica.sync_all() {
                        Ok(requests) => {
                            for sync in requests {
                                if let Err(error) = request(&client, sync).await {
                                    send_update(&updates, GuestSessionUpdate::Error(error.to_string())).await;
                                    break;
                                }
                            }
                            if let Some(presence) = last_presence.clone() {
                                if let Err(error) = publish_local_presence(&client, &replica, presence).await {
                                    send_update(&updates, GuestSessionUpdate::Error(error.to_string())).await;
                                }
                            }
                        }
                        Err(error) => {
                            send_update(&updates, GuestSessionUpdate::Error(error.to_string())).await;
                        }
                    }
                }
            }
        }
    }
}

async fn handle_command(
    command: Command,
    client: &Client,
    replica: &mut ReplicaProject,
    shared: &GuestSessionShared,
    updates: &mpsc::Sender<GuestSessionUpdate>,
) -> bool {
    match command {
        Command::Open { path, response } => {
            let result = async {
                let response = request(client, Request::OpenBuffer { path }).await?;
                let buffer = replica.install(response)?;
                shared
                    .open_buffers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(buffer);
                shared
                    .pending_releases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&buffer);
                let text = replica.text(buffer)?;
                Ok(OpenedBuffer { buffer, text })
            }
            .await;
            let _ = response.send(result);
        }
        Command::Edit {
            buffer,
            changes,
            response,
        } => {
            let result = async {
                if let Some(sync) = replica.replace_many(buffer, &changes)? {
                    request(client, sync).await?;
                }
                Ok(())
            }
            .await;
            let _ = response.send(result);
        }
        Command::Save {
            buffer,
            overwrite,
            response,
        } => {
            let pending = shared
                .local_edits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&buffer);
            let result = async {
                if let Some(edits) = pending {
                    flush_edits(buffer, edits, client, replica, &shared.open_buffers).await?;
                }
                request(client, Request::SaveBuffer { buffer, overwrite })
                    .await
                    .and_then(buffer_saved)
            }
            .await;
            let _ = response.send(result);
        }
        Command::Flush { buffer, response } => {
            let pending = shared
                .local_edits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&buffer);
            let result = async {
                if let Some(edits) = pending {
                    flush_edits(buffer, edits, client, replica, &shared.open_buffers).await?;
                }
                Ok(())
            }
            .await;
            let _ = response.send(result);
        }
        Command::ApplyFileTransaction {
            transaction,
            response,
        } => {
            let result = request(client, Request::ApplyFileTransaction { transaction })
                .await
                .and_then(file_transaction_changed);
            let _ = response.send(result);
        }
        Command::ReplayFileTransaction { redo, response } => {
            let result = request(client, Request::ReplayFileTransaction { redo })
                .await
                .and_then(file_transaction_changed);
            let _ = response.send(result);
        }
        Command::Follow {
            participant,
            response,
        } => {
            let result = async {
                let Response::Following { location } =
                    request(client, Request::Follow { participant }).await?
                else {
                    return Err(GuestSessionError::UnexpectedResponse("Following"));
                };
                let location = if let Some(location) = location {
                    if !replica.contains(location.presence.buffer) {
                        let snapshot = request(
                            client,
                            Request::OpenBuffer {
                                path: location.path.clone(),
                            },
                        )
                        .await?;
                        let buffer = replica.install(snapshot)?;
                        if buffer != location.presence.buffer {
                            return Err(GuestSessionError::FollowBufferMismatch);
                        }
                        shared
                            .open_buffers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(buffer);
                        shared
                            .pending_releases
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&buffer);
                    }
                    Some(ResolvedFollowLocation {
                        path: location.path,
                        presence: resolve_presence(replica, location.presence)?,
                    })
                } else {
                    None
                };
                Ok(location)
            }
            .await;
            match result {
                Ok(location) => {
                    send_update(
                        updates,
                        GuestSessionUpdate::Following {
                            participant,
                            location,
                        },
                    )
                    .await;
                    let _ = response.send(Ok(()));
                }
                Err(error) => {
                    let _ = response.send(Err(error));
                }
            }
        }
        Command::Invite {
            role,
            expires_unix_secs,
            response,
        } => {
            let result = async {
                match request(
                    client,
                    Request::Invite {
                        role,
                        expires_unix_secs,
                    },
                )
                .await?
                {
                    Response::Invitation(code) => {
                        code.parse().map_err(GuestSessionError::Transport)
                    }
                    _ => Err(GuestSessionError::UnexpectedResponse("Invitation")),
                }
            }
            .await;
            let _ = response.send(result);
        }
        Command::SetRole {
            participant,
            role,
            response,
        } => {
            let result = request(client, Request::SetRole { participant, role })
                .await
                .map(|_| ());
            let _ = response.send(result);
        }
        Command::RemoveParticipant {
            participant,
            response,
        } => {
            let result = request(client, Request::RemoveParticipant { participant })
                .await
                .map(|_| ());
            let _ = response.send(result);
        }
        Command::Leave { response } => {
            let result = request(client, Request::Leave).await.map(|_| ());
            let _ = response.send(result);
            return true;
        }
    }
    false
}

struct CachedFileList {
    files: Arc<[WorkspacePath]>,
    loaded: std::time::Instant,
}

async fn load_file_list(
    client: &crate::client::ClientRequestHandle,
    options: ScanOptions,
) -> Result<Arc<[WorkspacePath]>, GuestSessionError> {
    let mut files = Vec::new();
    let mut after = None;
    loop {
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.request(Request::ListFiles {
                options,
                after: after.clone(),
                limit: crate::MAX_FILE_PAGE_ENTRIES,
            }),
        )
        .await
        .map_err(|_| GuestSessionError::Timeout)??;
        let Response::Files { entries, next } = response else {
            return Err(GuestSessionError::UnexpectedResponse("Files"));
        };
        if entries
            .first()
            .zip(after.as_ref())
            .is_some_and(|(first, after)| first <= after)
            || next
                .as_ref()
                .zip(after.as_ref())
                .is_some_and(|(next, after)| next <= after)
            || (next.is_some() && entries.is_empty())
        {
            return Err(GuestSessionError::InvalidFilePage);
        }
        files.extend(entries);
        if files.len() > crate::MAX_PROJECT_FILES {
            return Err(GuestSessionError::FileLimit);
        }
        let Some(next) = next else {
            files.sort_unstable();
            files.dedup();
            return Ok(Arc::from(files));
        };
        after = Some(next);
    }
}

async fn flush_edits(
    buffer: BufferId,
    edits: PendingLocalEdits,
    client: &Client,
    replica: &mut ReplicaProject,
    open_buffers: &StdMutex<HashSet<BufferId>>,
) -> Result<(), GuestSessionError> {
    if !open_buffers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&buffer)
    {
        return Ok(());
    }
    let sync = match edits {
        PendingLocalEdits::Changes { batches, .. } => replica.replace_batches(buffer, &batches)?,
        PendingLocalEdits::Snapshot(text) => replica.replace_all(buffer, &text)?,
    };
    if !open_buffers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&buffer)
    {
        return Ok(());
    }
    if let Some(sync) = sync {
        request(client, sync).await?;
    }
    Ok(())
}

async fn flush_releases(
    client: &Client,
    replica: &mut ReplicaProject,
    open_buffers: &StdMutex<HashSet<BufferId>>,
    pending_releases: &StdMutex<HashSet<BufferId>>,
) -> Result<(), GuestSessionError> {
    let buffers = pending_releases
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .copied()
        .collect::<Vec<_>>();
    for buffer in buffers {
        if open_buffers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&buffer)
        {
            pending_releases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&buffer);
            continue;
        }
        let response = request(client, Request::CloseBuffer { buffer }).await?;
        if response != Response::Unit {
            return Err(GuestSessionError::UnexpectedResponse("Unit"));
        }
        let released = pending_releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&buffer);
        if released {
            replica.release(buffer);
        }
    }
    Ok(())
}

async fn publish_local_presence(
    client: &Client,
    replica: &ReplicaProject,
    presence: LocalPresence,
) -> Result<(), GuestSessionError> {
    let cursor = presence
        .cursor
        .map(|position| replica.anchor(presence.buffer, position, crate::AnchorAffinity::After))
        .transpose()?;
    let selection = presence
        .selection
        .map(|(anchor, head)| {
            Ok::<_, ReplicaError>((
                replica.anchor(presence.buffer, anchor, crate::AnchorAffinity::Before)?,
                replica.anchor(presence.buffer, head, crate::AnchorAffinity::After)?,
            ))
        })
        .transpose()?;
    let viewport = presence
        .viewport
        .map(|position| replica.anchor(presence.buffer, position, crate::AnchorAffinity::Before))
        .transpose()?;
    request(
        client,
        Request::PublishPresence(Presence {
            participant: replica.participant(),
            buffer: presence.buffer,
            cursor,
            selection,
            viewport,
            active_view: presence.active_view,
        }),
    )
    .await
    .map(|_| ())
}

async fn handle_event(
    event: Event,
    client: &Client,
    replica: &mut ReplicaProject,
    shared: &GuestSessionShared,
    updates: &mpsc::Sender<GuestSessionUpdate>,
    file_revision: &mut Option<u64>,
) -> Result<(), GuestSessionError> {
    match event {
        Event::ProjectState(state) => {
            apply_project_state(
                state,
                updates,
                &shared.file_lists,
                file_revision,
                &shared.participant,
                &shared.project,
            )
            .await;
        }
        Event::BufferSync { buffer, .. }
            if !shared
                .open_buffers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&buffer) => {}
        Event::BufferSync { .. } => {
            if let Some(update) = replica.apply(event)? {
                if let Some(reply) = update.reply {
                    request(client, reply).await?;
                }
                if !update.changes.is_empty() {
                    send_update(
                        updates,
                        GuestSessionUpdate::TextChanged {
                            buffer: update.buffer,
                            changes: update.changes,
                        },
                    )
                    .await;
                }
            }
        }
        Event::ResyncRequired { buffer, .. } => {
            if !shared
                .open_buffers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&buffer)
            {
                return Ok(());
            }
            let response = request(client, Request::ReadBuffer { buffer }).await?;
            replica.install(response)?;
            send_update(
                updates,
                GuestSessionUpdate::Snapshot {
                    buffer,
                    text: replica.text(buffer)?,
                },
            )
            .await;
        }
        Event::ParticipantJoined(participant) => {
            {
                let mut project = shared
                    .project
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                project
                    .participants
                    .retain(|current| current.id != participant.id);
                project.participants.push(participant.clone());
            }
            send_update(updates, GuestSessionUpdate::ParticipantJoined(participant)).await;
        }
        Event::ParticipantLeft { participant } => {
            shared
                .project
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .participants
                .retain(|current| current.id != participant);
            send_update(updates, GuestSessionUpdate::ParticipantLeft(participant)).await;
        }
        Event::RoleChanged { participant, role } => {
            {
                if let Some(info) = shared
                    .project
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .participants
                    .iter_mut()
                    .find(|info| info.id == participant)
                {
                    info.role = role;
                }
            }
            {
                let mut local = shared
                    .participant
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if local.id == participant {
                    local.role = role;
                }
            }
            send_update(
                updates,
                GuestSessionUpdate::RoleChanged { participant, role },
            )
            .await;
        }
        Event::Presence(presence) => match resolve_presence(replica, presence) {
            Ok(resolved) => {
                let _ = updates.try_send(GuestSessionUpdate::Presence(resolved));
            }
            Err(ReplicaError::UnknownBuffer(_)) => {}
            Err(error) => return Err(error.into()),
        },
        Event::PresenceCleared {
            participant,
            buffer,
        } => {
            send_update(
                updates,
                GuestSessionUpdate::PresenceCleared {
                    participant,
                    buffer,
                },
            )
            .await;
        }
        Event::FollowRequested { follower, leader } => {
            send_update(
                updates,
                GuestSessionUpdate::FollowRequested { follower, leader },
            )
            .await;
        }
        Event::BufferSaved { buffer, version } => {
            send_update(updates, GuestSessionUpdate::BufferSaved { buffer, version }).await;
        }
        Event::FilesChanged {
            file_revision: incoming_revision,
            transaction,
            undone,
        } => {
            if file_revision.is_some_and(|current| incoming_revision <= current) {
                return Ok(());
            }
            if file_revision.and_then(|current| current.checked_add(1)) != Some(incoming_revision) {
                let response = request(client, Request::ProjectState).await?;
                let Response::ProjectState(state) = response else {
                    return Err(GuestSessionError::UnexpectedResponse("ProjectState"));
                };
                apply_project_state(
                    state,
                    updates,
                    &shared.file_lists,
                    file_revision,
                    &shared.participant,
                    &shared.project,
                )
                .await;
                return Ok(());
            }
            *file_revision = Some(incoming_revision);
            shared
                .file_lists
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            send_update(
                updates,
                GuestSessionUpdate::FilesChanged {
                    transaction,
                    undone,
                },
            )
            .await;
        }
        Event::WorktreeChanged {
            file_revision: incoming_revision,
            mut changes,
            mut rescan,
        } => {
            if file_revision.is_some_and(|current| incoming_revision <= current) {
                return Ok(());
            }
            if file_revision.and_then(|current| current.checked_add(1)) != Some(incoming_revision) {
                let response = request(client, Request::ProjectState).await?;
                let Response::ProjectState(state) = response else {
                    return Err(GuestSessionError::UnexpectedResponse("ProjectState"));
                };
                apply_project_state(
                    state,
                    updates,
                    &shared.file_lists,
                    file_revision,
                    &shared.participant,
                    &shared.project,
                )
                .await;
                return Ok(());
            }
            if changes.len() > crate::MAX_WORKTREE_CHANGES_PER_EVENT {
                changes.clear();
                rescan = true;
            }
            *file_revision = Some(incoming_revision);
            shared
                .file_lists
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            send_update(
                updates,
                GuestSessionUpdate::WorktreeChanged { changes, rescan },
            )
            .await;
        }
        Event::LanguageServerDiagnostics(diagnostics) => {
            send_update(
                updates,
                GuestSessionUpdate::LanguageServerDiagnostics(diagnostics),
            )
            .await;
        }
        Event::LanguageServerRefresh(refresh) => {
            send_update(updates, GuestSessionUpdate::LanguageServerRefresh(refresh)).await;
        }
    }
    Ok(())
}

fn resolve_presence(
    replica: &ReplicaProject,
    presence: Presence,
) -> Result<ResolvedPresence, ReplicaError> {
    Ok(ResolvedPresence {
        participant: presence.participant,
        buffer: presence.buffer,
        cursor: presence
            .cursor
            .as_ref()
            .map(|anchor| replica.resolve_anchor(presence.buffer, anchor))
            .transpose()?,
        selection: presence
            .selection
            .as_ref()
            .map(|(anchor, head)| {
                Ok::<_, ReplicaError>((
                    replica.resolve_anchor(presence.buffer, anchor)?,
                    replica.resolve_anchor(presence.buffer, head)?,
                ))
            })
            .transpose()?,
        viewport: presence
            .viewport
            .as_ref()
            .map(|anchor| replica.resolve_anchor(presence.buffer, anchor))
            .transpose()?,
        active_view: presence.active_view,
    })
}

async fn apply_project_state(
    state: ProjectState,
    updates: &mpsc::Sender<GuestSessionUpdate>,
    file_lists: &StdMutex<HashMap<ScanOptions, CachedFileList>>,
    file_revision: &mut Option<u64>,
    participant_state: &StdRwLock<ParticipantInfo>,
    project_state: &StdRwLock<ProjectInfo>,
) {
    if file_revision.is_some_and(|current| state.file_revision < current) {
        return;
    }
    *file_revision = Some(state.file_revision);
    {
        let mut project = project_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        project.participants = state.participants.clone();
        let mut local = participant_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(current) = project
            .participants
            .iter()
            .find(|participant| participant.id == local.id)
        {
            *local = current.clone();
        }
    }
    file_lists
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    send_update(updates, GuestSessionUpdate::ProjectState(state)).await;
}

fn file_transaction_changed(response: Response) -> Result<bool, GuestSessionError> {
    match response {
        Response::FileTransaction { changed } => Ok(changed),
        _ => Err(GuestSessionError::UnexpectedResponse("FileTransaction")),
    }
}

fn buffer_saved(response: Response) -> Result<crate::FileVersion, GuestSessionError> {
    match response {
        Response::BufferSaved { version } => Ok(version),
        _ => Err(GuestSessionError::UnexpectedResponse("BufferSaved")),
    }
}

async fn request(client: &Client, request: Request) -> Result<Response, GuestSessionError> {
    tokio::time::timeout(REQUEST_TIMEOUT, client.request(request))
        .await
        .map_err(|_| GuestSessionError::Timeout)?
        .map_err(Into::into)
}

async fn send_update(updates: &mpsc::Sender<GuestSessionUpdate>, update: GuestSessionUpdate) {
    let _ = updates.send(update).await;
}

#[derive(Debug, thiserror::Error)]
pub enum GuestSessionError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Transport(#[from] crate::TransportError),
    #[error(transparent)]
    Replica(#[from] ReplicaError),
    #[error("collaboration request timed out")]
    Timeout,
    #[error("collaboration client is busy")]
    Busy,
    #[error("collaboration request exceeds the protocol payload limit")]
    RequestTooLarge,
    #[error("collaboration session is closed")]
    Closed,
    #[error("collaboration server returned an unexpected response; expected {0}")]
    UnexpectedResponse(&'static str),
    #[error("collaboration server returned a non-advancing file page")]
    InvalidFilePage,
    #[error("invalid collaboration content search: {0}")]
    InvalidSearch(String),
    #[error("collaboration project contains too many files")]
    FileLimit,
    #[error("collaboration follow target changed buffers while opening")]
    FollowBufferMismatch,
    #[error("collaboration session task failed: {0}")]
    Task(#[source] tokio::task::JoinError),
}
