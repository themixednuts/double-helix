use crate::{
    client::{Client, ClientRequestError, ServerEventRouter},
    BeginWrite, Capability, ClientHello, ClientRequest, ContentId, ContentSearchPage,
    ContentSearchQuery, DirectoryEntry, DirectoryOptions, FileChange, FileChanges, FileMetadata,
    FileTransaction, FileTransactionReceipt, LanguageServerWorkspace, OpenWorkspace, OperationId,
    ProcessExit, ProcessId, ProcessKind, ProcessOutput, ProcessSpec, ProcessStream, ReadDir,
    ReadFile, ResolveLanguageServerWorkspace, ScanOptions, SearchBatch, SearchEntry, SearchFiles,
    ServerEvent, ServerHello, ServerResponse, SessionId, TransactionId, Watch, WatchId,
    WorkspaceInfo, WorkspacePath, WriteChunk, WriteId, MAX_FILE_CHUNK_BYTES, PROTOCOL_VERSION,
};
use parking_lot::Mutex;
use serde_bytes::ByteBuf;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    sync::{mpsc, watch, Mutex as AsyncMutex},
};
use tokio_util::sync::CancellationToken;

const MAX_STABLE_READ_ATTEMPTS: usize = 3;
const WATCH_EVENT_CAPACITY: usize = 256;
const PROCESS_EVENT_CAPACITY: usize = 64;
const PROCESS_STREAM_CAPACITY: usize = 256 * 1024;
const PROCESS_INPUT_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct RemoteWorkspaceClient {
    client: Client,
    authority: Arc<str>,
    client_version: Arc<str>,
    root: Arc<str>,
    hello: ServerHello,
    workspace: WorkspaceInfo,
    next_operation: Arc<AtomicU64>,
    event_routes: Arc<EventRoutes>,
    file_history: Arc<AsyncMutex<FileTransactionHistory>>,
}

impl std::fmt::Debug for RemoteWorkspaceClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteWorkspaceClient")
            .field("authority", &self.authority)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

pub struct RemoteFile {
    pub bytes: Vec<u8>,
    pub metadata: FileMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSearchSnapshot {
    pub revision: u64,
    pub entries: Vec<SearchEntry>,
    pub scanned: u64,
    pub done: bool,
}

pub struct RemoteSearch {
    operation: OperationId,
    updates: watch::Receiver<Option<RemoteSearchSnapshot>>,
    last_revision: Option<u64>,
    client: Client,
    event_routes: Arc<EventRoutes>,
}

pub struct RemoteWatch {
    watch: WatchId,
    updates: mpsc::Receiver<RemoteWatchUpdate>,
    client: Client,
    event_routes: Arc<EventRoutes>,
    closed: bool,
}

#[derive(Debug, Clone)]
pub struct RemoteProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: WorkspacePath,
    pub env: std::collections::BTreeMap<String, String>,
}

pub struct RemoteProcess {
    stdin: DuplexStream,
    stdout: DuplexStream,
    stderr: DuplexStream,
    control: RemoteProcessControl,
}

pub struct RemoteProcessParts {
    pub stdin: DuplexStream,
    pub stdout: DuplexStream,
    pub stderr: DuplexStream,
    pub control: RemoteProcessControl,
}

#[derive(Clone)]
pub struct RemoteProcessControl {
    process: ProcessId,
    client: Client,
    exit: watch::Receiver<Option<ProcessExit>>,
}

impl std::fmt::Debug for RemoteProcessControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteProcessControl")
            .field("process", &self.process)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteWatchUpdate {
    Changes(Vec<FileChange>),
    Rescan,
}

#[derive(Default)]
struct EventRoutes {
    searches: Mutex<HashMap<OperationId, SearchRoute>>,
    watches: Mutex<HashMap<WatchId, WatchRoute>>,
    processes: Mutex<HashMap<ProcessId, ProcessRoute>>,
}

struct SearchRoute {
    revision: u64,
    next_batch: u32,
    entries: Vec<SearchEntry>,
    updates: watch::Sender<Option<RemoteSearchSnapshot>>,
}

struct WatchRoute {
    request: Watch,
    updates: mpsc::Sender<RemoteWatchUpdate>,
    rescan_pending: Arc<std::sync::atomic::AtomicBool>,
}

struct ProcessRoute {
    output: mpsc::Sender<ProcessOutput>,
    exit: watch::Sender<Option<ProcessExit>>,
    canceled: CancellationToken,
}

#[derive(Default)]
struct FileTransactionHistory {
    undo: Vec<FileTransactionHistoryEntry>,
    redo: Vec<FileTransaction>,
}

struct FileTransactionHistoryEntry {
    id: TransactionId,
    transaction: FileTransaction,
}

impl RemoteWorkspaceClient {
    pub async fn open(
        client: Client,
        authority: impl Into<Arc<str>>,
        client_version: impl Into<Arc<str>>,
        root: impl Into<Arc<str>>,
    ) -> Result<Self, BackendError> {
        let client_version = client_version.into();
        let root = root.into();
        let session = new_session_id()?;
        let (hello, workspace) = open_workspace(&client, &client_version, &root, session).await?;
        if !client.finish_handshake() {
            return Err(BackendError::ConnectionReplaced);
        }
        let event_routes = Arc::new(EventRoutes::default());
        client.set_event_router(event_routes.clone());
        Ok(Self {
            client,
            authority: authority.into(),
            client_version,
            root,
            hello,
            workspace,
            next_operation: Arc::new(AtomicU64::new(1)),
            event_routes,
            file_history: Arc::new(AsyncMutex::new(FileTransactionHistory::default())),
        })
    }

    pub(crate) async fn reopen(&self) -> Result<(), BackendError> {
        let (hello, workspace) = open_workspace(
            &self.client,
            &self.client_version,
            &self.root,
            self.workspace.session,
        )
        .await?;
        if hello != self.hello || workspace != self.workspace {
            self.client
                .disconnect_current("remote workspace identity changed during reconnect");
            return Err(BackendError::WorkspaceChanged);
        }
        self.event_routes.searches.lock().clear();
        self.event_routes.disconnect_processes();
        if !self.client.finish_handshake() {
            return Err(BackendError::ConnectionReplaced);
        }
        self.restart_watches().await?;
        Ok(())
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn hello(&self) -> &ServerHello {
        &self.hello
    }

    pub fn workspace(&self) -> &WorkspaceInfo {
        &self.workspace
    }

    pub fn supports(&self, capability: Capability) -> bool {
        self.hello.capabilities.contains(&capability)
    }

    pub async fn resolve_language_server_workspace(
        &self,
        request: ResolveLanguageServerWorkspace,
    ) -> Result<Option<LanguageServerWorkspace>, BackendError> {
        if !self.supports(Capability::LanguageServers) {
            return Err(BackendError::UnsupportedCapability(
                Capability::LanguageServers,
            ));
        }
        let response = self
            .client
            .request(ClientRequest::ResolveLanguageServerWorkspace(request))
            .await?;
        match response {
            ServerResponse::LanguageServerWorkspace(workspace) => Ok(workspace),
            _ => Err(BackendError::UnexpectedResponse("LanguageServerWorkspace")),
        }
    }

    pub fn route_event(&self, event: &ServerEvent) -> bool {
        self.event_routes.route(event.clone()).is_ok()
    }

    pub async fn start_process(
        &self,
        spec: RemoteProcessSpec,
    ) -> Result<RemoteProcess, BackendError> {
        if !self.supports(Capability::Processes) {
            return Err(BackendError::UnsupportedCapability(Capability::Processes));
        }
        let process = ProcessId(self.next_route_id()?);
        let (output, output_rx) = mpsc::channel(PROCESS_EVENT_CAPACITY);
        let (exit, exit_rx) = watch::channel(None);
        let canceled = CancellationToken::new();
        self.event_routes.processes.lock().insert(
            process,
            ProcessRoute {
                output,
                exit,
                canceled: canceled.clone(),
            },
        );

        let request = ClientRequest::StartProcess(ProcessSpec {
            process,
            program: spec.program,
            args: spec.args,
            cwd: spec.cwd,
            env: spec.env,
            kind: ProcessKind::Pipes,
        });
        let response = self.client.request(request).await;
        match response {
            Ok(ServerResponse::Unit) => {}
            Ok(_) => {
                self.event_routes.processes.lock().remove(&process);
                return Err(BackendError::UnexpectedResponse("Unit"));
            }
            Err(error) => {
                self.event_routes.processes.lock().remove(&process);
                return Err(error.into());
            }
        }

        let (stdin, input) = tokio::io::duplex(PROCESS_STREAM_CAPACITY);
        let (output, stdout) = tokio::io::duplex(PROCESS_STREAM_CAPACITY);
        let (error_output, stderr) = tokio::io::duplex(PROCESS_STREAM_CAPACITY);
        let control = RemoteProcessControl {
            process,
            client: self.client.clone(),
            exit: exit_rx,
        };
        tokio::spawn(pump_remote_process_input(
            self.client.clone(),
            process,
            input,
        ));
        tokio::spawn(pump_remote_process_output(
            output,
            error_output,
            output_rx,
            control.clone(),
            canceled,
        ));
        Ok(RemoteProcess {
            stdin,
            stdout,
            stderr,
            control,
        })
    }

    pub async fn search_files(
        &self,
        root: WorkspacePath,
        query: String,
        options: ScanOptions,
        limit: u32,
        canceled: CancellationToken,
    ) -> Result<RemoteSearch, BackendError> {
        let operation = OperationId(self.next_route_id()?);
        let (updates, receiver) = watch::channel(None);
        self.event_routes.searches.lock().insert(
            operation,
            SearchRoute {
                revision: 0,
                next_batch: 0,
                entries: Vec::new(),
                updates,
            },
        );
        let response = self
            .client
            .request_cancellable(
                ClientRequest::SearchFiles(SearchFiles {
                    operation,
                    root,
                    query,
                    options,
                    limit,
                }),
                canceled,
            )
            .await;
        match response {
            Ok(ServerResponse::Unit) => Ok(RemoteSearch {
                operation,
                updates: receiver,
                last_revision: None,
                client: self.client.clone(),
                event_routes: self.event_routes.clone(),
            }),
            Ok(_) => {
                self.event_routes.searches.lock().remove(&operation);
                Err(BackendError::UnexpectedResponse("Unit"))
            }
            Err(error) => {
                self.event_routes.searches.lock().remove(&operation);
                Err(error.into())
            }
        }
    }

    pub async fn search_content_page(
        &self,
        query: ContentSearchQuery,
        canceled: CancellationToken,
    ) -> Result<ContentSearchPage, BackendError> {
        if !self.supports(Capability::FileSearch) {
            return Err(BackendError::UnsupportedCapability(Capability::FileSearch));
        }
        query
            .validate()
            .map_err(|message| BackendError::InvalidSearch(message.to_owned()))?;
        let response = self
            .client
            .request_cancellable(ClientRequest::SearchContent(query), canceled)
            .await?;
        match response {
            ServerResponse::ContentSearch(page) => Ok(page),
            _ => Err(BackendError::UnexpectedResponse("ContentSearch")),
        }
    }

    pub async fn watch_files(
        &self,
        path: WorkspacePath,
        recursive: bool,
    ) -> Result<RemoteWatch, BackendError> {
        if !self.supports(Capability::FileWatch) {
            return Err(BackendError::UnsupportedCapability(Capability::FileWatch));
        }
        let watch = WatchId(self.next_route_id()?);
        let request = Watch {
            watch,
            path,
            recursive,
        };
        let (updates, receiver) = mpsc::channel(WATCH_EVENT_CAPACITY);
        self.event_routes.watches.lock().insert(
            watch,
            WatchRoute {
                request: request.clone(),
                updates,
                rescan_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        let response = self.client.request(ClientRequest::Watch(request)).await;
        match response {
            Ok(ServerResponse::Unit) => Ok(RemoteWatch {
                watch,
                updates: receiver,
                client: self.client.clone(),
                event_routes: self.event_routes.clone(),
                closed: false,
            }),
            Ok(_) => {
                self.event_routes.watches.lock().remove(&watch);
                Err(BackendError::UnexpectedResponse("Unit"))
            }
            Err(error) => {
                self.event_routes.watches.lock().remove(&watch);
                Err(error.into())
            }
        }
    }

    async fn restart_watches(&self) -> Result<(), BackendError> {
        let watches = self
            .event_routes
            .watches
            .lock()
            .values()
            .map(|route| route.request.clone())
            .collect::<Vec<_>>();
        for watch in watches {
            let response = self
                .client
                .request(ClientRequest::Watch(watch.clone()))
                .await?;
            if !matches!(response, ServerResponse::Unit) {
                return Err(BackendError::UnexpectedResponse("Unit"));
            }
            self.event_routes.request_watch_rescan(watch.watch);
        }
        Ok(())
    }

    fn next_route_id(&self) -> Result<u64, BackendError> {
        self.next_operation
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| BackendError::RouteIdsExhausted)
    }

    pub async fn read_dir(
        &self,
        path: WorkspacePath,
        options: DirectoryOptions,
        canceled: CancellationToken,
    ) -> Result<Vec<DirectoryEntry>, BackendError> {
        let response = self
            .client
            .request_cancellable(ClientRequest::ReadDir(ReadDir { path, options }), canceled)
            .await?;
        match response {
            ServerResponse::Directory(entries) => Ok(entries),
            _ => Err(BackendError::UnexpectedResponse("Directory")),
        }
    }

    pub async fn stat(
        &self,
        path: WorkspacePath,
        canceled: CancellationToken,
    ) -> Result<Option<FileMetadata>, BackendError> {
        let response = self
            .client
            .request_cancellable(ClientRequest::Stat { path }, canceled)
            .await?;
        match response {
            ServerResponse::Metadata(metadata) => Ok(metadata),
            _ => Err(BackendError::UnexpectedResponse("Metadata")),
        }
    }

    pub async fn read_file(
        &self,
        path: WorkspacePath,
        canceled: CancellationToken,
    ) -> Result<RemoteFile, BackendError> {
        for attempt in 0..MAX_STABLE_READ_ATTEMPTS {
            match self
                .read_file_once(path.clone(), canceled.child_token())
                .await
            {
                Ok(file) => return Ok(file),
                Err(BackendError::Request(ClientRequestError::Remote(error)))
                    if error.code == crate::ErrorCode::Conflict
                        && attempt + 1 < MAX_STABLE_READ_ATTEMPTS =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("stable read attempt loop always returns")
    }

    async fn read_file_once(
        &self,
        path: WorkspacePath,
        canceled: CancellationToken,
    ) -> Result<RemoteFile, BackendError> {
        let chunk_size = self
            .hello
            .limits
            .max_file_chunk_bytes
            .clamp(1, MAX_FILE_CHUNK_BYTES);
        let mut bytes = Vec::new();
        let mut expected = None;
        loop {
            let offset = bytes.len() as u64;
            let response = self
                .client
                .request_cancellable(
                    ClientRequest::ReadFile(ReadFile {
                        path: path.clone(),
                        offset,
                        max_bytes: chunk_size,
                        expected,
                    }),
                    canceled.child_token(),
                )
                .await?;
            let ServerResponse::FileChunk(chunk) = response else {
                return Err(BackendError::UnexpectedResponse("FileChunk"));
            };
            if chunk.offset != offset {
                return Err(BackendError::InvalidChunkOffset {
                    expected: offset,
                    actual: chunk.offset,
                });
            }
            let content = chunk
                .metadata
                .content
                .ok_or(BackendError::MissingContentId)?;
            if expected.is_some_and(|expected| expected != content) {
                return Err(BackendError::ContentChanged);
            }
            expected = Some(content);
            if bytes.is_empty() {
                let capacity = usize::try_from(content.len)
                    .map_err(|_| BackendError::FileTooLarge(content.len))?;
                bytes.reserve(capacity.min(16 * 1024 * 1024));
            }
            if chunk.bytes.is_empty() && !chunk.eof {
                return Err(BackendError::EmptyChunk);
            }
            bytes.extend_from_slice(&chunk.bytes);
            if chunk.eof {
                return Ok(RemoteFile {
                    bytes,
                    metadata: chunk.metadata,
                });
            }
        }
    }

    pub async fn write_file(
        &self,
        path: WorkspacePath,
        bytes: &[u8],
        expected: Option<ContentId>,
        canceled: CancellationToken,
    ) -> Result<FileMetadata, BackendError> {
        let response = self
            .client
            .request_cancellable(
                ClientRequest::BeginWrite(BeginWrite {
                    path,
                    expected,
                    create_parents: true,
                }),
                canceled.child_token(),
            )
            .await?;
        let ServerResponse::WriteStarted { write } = response else {
            return Err(BackendError::UnexpectedResponse("WriteStarted"));
        };
        match self
            .write_file_started(write, bytes, canceled.child_token())
            .await
        {
            Ok(metadata) => Ok(metadata),
            Err(error) => {
                let _ = self
                    .client
                    .request(ClientRequest::AbortWrite { write })
                    .await;
                Err(error)
            }
        }
    }

    pub async fn apply_file_transaction(
        &self,
        transaction: FileTransaction,
    ) -> Result<FileTransactionReceipt, BackendError> {
        let mut history = self.file_history.lock().await;
        let receipt = self
            .apply_file_transaction_untracked(transaction.clone())
            .await?;
        history.undo.push(FileTransactionHistoryEntry {
            id: receipt.transaction,
            transaction,
        });
        history.redo.clear();
        Ok(receipt)
    }

    pub async fn undo_file_transaction(&self) -> Result<bool, BackendError> {
        let mut history = self.file_history.lock().await;
        let Some(entry) = history.undo.pop() else {
            return Ok(false);
        };
        match self.undo_file_transaction_exact(entry.id).await {
            Ok(()) => {
                history.redo.push(entry.transaction);
                Ok(true)
            }
            Err(error) => {
                history.undo.push(entry);
                Err(error)
            }
        }
    }

    pub async fn redo_file_transaction(&self) -> Result<bool, BackendError> {
        let mut history = self.file_history.lock().await;
        let Some(transaction) = history.redo.pop() else {
            return Ok(false);
        };
        match self
            .apply_file_transaction_untracked(transaction.clone())
            .await
        {
            Ok(receipt) => {
                history.undo.push(FileTransactionHistoryEntry {
                    id: receipt.transaction,
                    transaction,
                });
                Ok(true)
            }
            Err(error) => {
                history.redo.push(transaction);
                Err(error)
            }
        }
    }

    /// Apply a transaction without adding it to this client's interactive
    /// undo stack. Higher-level shared-project journals use the returned ID
    /// to own their history independently.
    pub async fn apply_file_transaction_untracked(
        &self,
        transaction: FileTransaction,
    ) -> Result<FileTransactionReceipt, BackendError> {
        let response = self
            .client
            .request(ClientRequest::ApplyFileTransaction(transaction))
            .await?;
        match response {
            ServerResponse::FileTransactionApplied(receipt) => Ok(receipt),
            _ => Err(BackendError::UnexpectedResponse("FileTransactionApplied")),
        }
    }

    /// Undo one exact server transaction without consuming this client's
    /// interactive undo stack.
    pub async fn undo_file_transaction_exact(
        &self,
        transaction: TransactionId,
    ) -> Result<(), BackendError> {
        match self
            .client
            .request(ClientRequest::UndoFileTransaction { transaction })
            .await?
        {
            ServerResponse::FileTransactionUndone => Ok(()),
            _ => Err(BackendError::UnexpectedResponse("FileTransactionUndone")),
        }
    }

    async fn write_file_started(
        &self,
        write: WriteId,
        bytes: &[u8],
        canceled: CancellationToken,
    ) -> Result<FileMetadata, BackendError> {
        let chunk_size = self
            .hello
            .limits
            .max_file_chunk_bytes
            .clamp(1, MAX_FILE_CHUNK_BYTES) as usize;
        for (index, chunk) in bytes.chunks(chunk_size).enumerate() {
            let response = self
                .client
                .request_cancellable(
                    ClientRequest::WriteChunk(WriteChunk {
                        write,
                        offset: (index * chunk_size) as u64,
                        bytes: ByteBuf::from(chunk.to_vec()),
                    }),
                    canceled.child_token(),
                )
                .await?;
            if !matches!(response, ServerResponse::Unit) {
                return Err(BackendError::UnexpectedResponse("Unit"));
            }
        }
        let response = self
            .client
            .request_cancellable(ClientRequest::CommitWrite { write }, canceled.child_token())
            .await?;
        match response {
            ServerResponse::WriteCommitted { metadata } => Ok(metadata),
            _ => Err(BackendError::UnexpectedResponse("WriteCommitted")),
        }
    }
}

impl EventRoutes {
    fn route_search(&self, batch: &SearchBatch) -> bool {
        let mut searches = self.searches.lock();
        let Some(route) = searches.get_mut(&batch.operation) else {
            return false;
        };
        if batch.revision < route.revision {
            return true;
        }
        if batch.revision > route.revision {
            route.revision = batch.revision;
            route.next_batch = 0;
            route.entries.clear();
        }
        if batch.batch != route.next_batch {
            searches.remove(&batch.operation);
            return true;
        }
        route.next_batch = route.next_batch.saturating_add(1);
        route.entries.extend(batch.entries.iter().cloned());
        if batch.revision_done {
            route.updates.send_replace(Some(RemoteSearchSnapshot {
                revision: route.revision,
                entries: route.entries.clone(),
                scanned: batch.scanned,
                done: batch.done,
            }));
        }
        if batch.done {
            searches.remove(&batch.operation);
        }
        true
    }

    fn route_file_changes(&self, changes: &FileChanges) -> bool {
        let result = {
            let watches = self.watches.lock();
            let Some(route) = watches.get(&changes.watch) else {
                return false;
            };
            route
                .updates
                .try_send(RemoteWatchUpdate::Changes(changes.changes.clone()))
        };
        match result {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.request_watch_rescan(changes.watch);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.watches.lock().remove(&changes.watch);
            }
        }
        true
    }

    fn request_watch_rescan(&self, watch: WatchId) {
        let (updates, pending) = {
            let watches = self.watches.lock();
            let Some(route) = watches.get(&watch) else {
                return;
            };
            (route.updates.clone(), route.rescan_pending.clone())
        };
        if pending.swap(true, Ordering::AcqRel) {
            return;
        }
        match updates.try_send(RemoteWatchUpdate::Rescan) {
            Ok(()) => {
                pending.store(false, Ordering::Release);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                pending.store(false, Ordering::Release);
                self.watches.lock().remove(&watch);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        let _ = updates.send(RemoteWatchUpdate::Rescan).await;
                        pending.store(false, Ordering::Release);
                    });
                } else {
                    pending.store(false, Ordering::Release);
                }
            }
        }
    }

    fn route_process_output(&self, output: ProcessOutput) -> Result<(), ProcessOutput> {
        let result = {
            let processes = self.processes.lock();
            let Some(route) = processes.get(&output.process) else {
                return Err(output);
            };
            if route.canceled.is_cancelled() {
                return Ok(());
            }
            route
                .output
                .try_send(output)
                .map_err(|error| (error, route.canceled.clone()))
        };
        match result {
            Ok(()) => {}
            Err((mpsc::error::TrySendError::Full(_), canceled)) => canceled.cancel(),
            Err((mpsc::error::TrySendError::Closed(_), canceled)) => canceled.cancel(),
        }
        Ok(())
    }

    fn route_process_exit(&self, exit: ProcessExit) -> Result<(), ProcessExit> {
        let Some(route) = self.processes.lock().remove(&exit.process) else {
            return Err(exit);
        };
        route.exit.send_replace(Some(exit));
        Ok(())
    }

    fn disconnect_processes(&self) {
        for (process, route) in self.processes.lock().drain() {
            route.exit.send_replace(Some(ProcessExit {
                process,
                code: None,
                signal: None,
            }));
            route.canceled.cancel();
        }
    }
}

impl ServerEventRouter for EventRoutes {
    fn route(&self, event: ServerEvent) -> Result<(), ServerEvent> {
        match event {
            ServerEvent::SearchBatch(batch) if self.route_search(&batch) => Ok(()),
            ServerEvent::FileChanges(changes) if self.route_file_changes(&changes) => Ok(()),
            ServerEvent::ProcessOutput(output) => self
                .route_process_output(output)
                .map_err(ServerEvent::ProcessOutput),
            ServerEvent::ProcessExited(exit) => self
                .route_process_exit(exit)
                .map_err(ServerEvent::ProcessExited),
            event => Err(event),
        }
    }
}

impl RemoteSearch {
    pub fn operation(&self) -> OperationId {
        self.operation
    }

    pub async fn next(&mut self) -> Option<RemoteSearchSnapshot> {
        loop {
            let update = self.updates.borrow_and_update().clone();
            if let Some(update) = update {
                if self.last_revision != Some(update.revision) {
                    self.last_revision = Some(update.revision);
                    return Some(update);
                }
            }
            if self.updates.changed().await.is_err() {
                return None;
            }
        }
    }

    pub async fn cancel(self) -> Result<(), BackendError> {
        self.event_routes.searches.lock().remove(&self.operation);
        let response = self
            .client
            .request(ClientRequest::CancelOperation {
                operation: self.operation,
            })
            .await?;
        if matches!(response, ServerResponse::Unit) {
            Ok(())
        } else {
            Err(BackendError::UnexpectedResponse("Unit"))
        }
    }
}

impl Drop for RemoteSearch {
    fn drop(&mut self) {
        self.event_routes.searches.lock().remove(&self.operation);
    }
}

impl RemoteWatch {
    pub fn id(&self) -> WatchId {
        self.watch
    }

    pub async fn next(&mut self) -> Option<RemoteWatchUpdate> {
        self.updates.recv().await
    }

    pub async fn close(mut self) -> Result<(), BackendError> {
        self.event_routes.watches.lock().remove(&self.watch);
        let response = self
            .client
            .request(ClientRequest::Unwatch { watch: self.watch })
            .await?;
        self.closed = true;
        if matches!(response, ServerResponse::Unit) {
            Ok(())
        } else {
            Err(BackendError::UnexpectedResponse("Unit"))
        }
    }
}

impl Drop for RemoteWatch {
    fn drop(&mut self) {
        self.event_routes.watches.lock().remove(&self.watch);
        if self.closed || !self.client.is_connected() {
            return;
        }
        let client = self.client.clone();
        let watch = self.watch;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = client.request(ClientRequest::Unwatch { watch }).await;
            });
        }
    }
}

impl RemoteProcess {
    pub fn into_parts(self) -> RemoteProcessParts {
        RemoteProcessParts {
            stdin: self.stdin,
            stdout: self.stdout,
            stderr: self.stderr,
            control: self.control,
        }
    }
}

impl RemoteProcessControl {
    pub fn id(&self) -> ProcessId {
        self.process
    }

    pub async fn kill(&self) -> Result<(), BackendError> {
        if self.exit.borrow().is_some() || !self.client.is_connected() {
            return Ok(());
        }
        let response = self
            .client
            .request(ClientRequest::KillProcess {
                process: self.process,
            })
            .await?;
        if matches!(response, ServerResponse::Unit) {
            Ok(())
        } else {
            Err(BackendError::UnexpectedResponse("Unit"))
        }
    }

    pub async fn wait(&self) -> Result<ProcessExit, BackendError> {
        let mut exit = self.exit.clone();
        loop {
            if let Some(exit) = exit.borrow_and_update().clone() {
                return Ok(exit);
            }
            exit.changed()
                .await
                .map_err(|_| BackendError::ProcessEventClosed)?;
        }
    }
}

async fn pump_remote_process_input(client: Client, process: ProcessId, mut input: DuplexStream) {
    let mut buffer = vec![0; PROCESS_INPUT_CHUNK_BYTES];
    loop {
        let read = match input.read(&mut buffer).await {
            Ok(0) => {
                let _ = client.close_process_input(process).await;
                return;
            }
            Err(_) => return,
            Ok(read) => read,
        };
        if client
            .send_process_input(process, buffer[..read].to_vec())
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn pump_remote_process_output(
    mut stdout: DuplexStream,
    mut stderr: DuplexStream,
    mut output: mpsc::Receiver<ProcessOutput>,
    control: RemoteProcessControl,
    canceled: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            _ = canceled.cancelled() => break,
            event = output.recv() => event,
        };
        let Some(event) = event else {
            break;
        };
        let result = match event.stream {
            ProcessStream::Stdout | ProcessStream::Pty => stdout.write_all(&event.bytes).await,
            ProcessStream::Stderr => stderr.write_all(&event.bytes).await,
        };
        if result.is_err() {
            break;
        }
    }
    let _ = stdout.shutdown().await;
    let _ = stderr.shutdown().await;
    if control.exit.borrow().is_none() {
        let _ = control.kill().await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error(transparent)]
    Request(#[from] ClientRequestError),
    #[error("remote protocol mismatch: client {client}, server {server}")]
    ProtocolMismatch { client: u16, server: u16 },
    #[error("remote server returned an unexpected response; expected {0}")]
    UnexpectedResponse(&'static str),
    #[error("remote file chunk started at {actual}, expected {expected}")]
    InvalidChunkOffset { expected: u64, actual: u64 },
    #[error("remote file changed between chunks")]
    ContentChanged,
    #[error("remote file is too large for this client: {0} bytes")]
    FileTooLarge(u64),
    #[error("remote server returned an empty non-final file chunk")]
    EmptyChunk,
    #[error("remote file chunk did not include a content generation")]
    MissingContentId,
    #[error("invalid remote workspace path: {0}")]
    InvalidPath(String),
    #[error("invalid remote content search: {0}")]
    InvalidSearch(String),
    #[error("failed to create a remote workspace session: {0}")]
    SessionRandom(String),
    #[error("remote transport changed while completing its handshake")]
    ConnectionReplaced,
    #[error("remote server identity changed while reconnecting")]
    WorkspaceChanged,
    #[error("remote route identifier space is exhausted")]
    RouteIdsExhausted,
    #[error("remote server does not support capability {0:?}")]
    UnsupportedCapability(Capability),
    #[error("remote process event stream closed before the process exited")]
    ProcessEventClosed,
}

fn requested_capabilities() -> Vec<Capability> {
    vec![
        Capability::FileSystem,
        Capability::FileSearch,
        Capability::FileWatch,
        Capability::FileTransactions,
        Capability::Processes,
        Capability::Pseudoterminals,
        Capability::Vcs,
        Capability::Packages,
        Capability::Plugins,
        Capability::LanguageServers,
        Capability::DebugAdapters,
        Capability::PortForwarding,
    ]
}

async fn open_workspace(
    client: &Client,
    client_version: &str,
    root: &str,
    session: SessionId,
) -> Result<(ServerHello, WorkspaceInfo), BackendError> {
    let hello = client
        .request_handshake(ClientRequest::Hello(ClientHello {
            protocol: PROTOCOL_VERSION,
            client_version: client_version.to_owned(),
            requested: requested_capabilities(),
        }))
        .await?;
    let ServerResponse::Hello(hello) = hello else {
        return Err(BackendError::UnexpectedResponse("Hello"));
    };
    if hello.protocol != PROTOCOL_VERSION {
        return Err(BackendError::ProtocolMismatch {
            client: PROTOCOL_VERSION,
            server: hello.protocol,
        });
    }
    let workspace = client
        .request_handshake(ClientRequest::OpenWorkspace(OpenWorkspace {
            root: root.to_owned(),
            session,
        }))
        .await?;
    let ServerResponse::WorkspaceOpened(workspace) = workspace else {
        return Err(BackendError::UnexpectedResponse("WorkspaceOpened"));
    };
    if workspace.session != session {
        return Err(BackendError::WorkspaceChanged);
    }
    Ok((hello, workspace))
}

fn new_session_id() -> Result<SessionId, BackendError> {
    loop {
        let mut bytes = [0; std::mem::size_of::<u64>()];
        getrandom::fill(&mut bytes)
            .map_err(|error| BackendError::SessionRandom(error.to_string()))?;
        let session = u64::from_le_bytes(bytes);
        if session != 0 {
            return Ok(SessionId(session));
        }
    }
}
