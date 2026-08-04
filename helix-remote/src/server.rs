use crate::{
    process::ProcessTable,
    protocol::*,
    search::SearchIndex,
    transaction::TransactionStore,
    watch::WatchTable,
    workspace::{file_metadata, io_error, Workspace},
    WorkspacePath,
};
use helix_ipc::{FrameCodec, FrameError};
use serde_bytes::ByteBuf;
use std::{
    collections::{HashMap, VecDeque},
    io::ErrorKind,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, SeekFrom},
    sync::{mpsc, Mutex, OwnedSemaphorePermit, RwLock, Semaphore},
};
use tokio_util::sync::CancellationToken;

const OUTBOUND_CAPACITY: usize = 256;
const SEARCH_INDEX_CACHE_CAPACITY: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("remote server writer task failed: {0}")]
    WriterTask(#[from] tokio::task::JoinError),
}

pub async fn run_stdio(server_version: impl Into<String>) -> Result<(), ServerError> {
    run_connection(tokio::io::stdin(), tokio::io::stdout(), server_version).await
}

pub async fn run_connection<R, W>(
    mut reader: R,
    writer: W,
    server_version: impl Into<String>,
) -> Result<(), ServerError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let state = Arc::new(ServerState::new(server_version.into()));
    let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let writer_task = tokio::spawn(write_frames(writer, outbound_rx));
    let mut codec = FrameCodec::with_limits(8 * 1024, MAX_REMOTE_FRAME_BYTES);

    loop {
        let frame = match codec.read::<ClientFrame, _>(&mut reader).await {
            Ok(frame) => frame,
            Err(FrameError::Io(error)) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        };
        match frame {
            ClientFrame::Cancel { id } => state.cancel_request(id).await,
            ClientFrame::ProcessInput { process, bytes } => {
                if state.handshake_complete.load(Ordering::Acquire) {
                    let _ = state.processes.enqueue_input(process, bytes).await;
                }
            }
            ClientFrame::CloseProcessInput { process } => {
                if state.handshake_complete.load(Ordering::Acquire) {
                    state.processes.close_input(process).await;
                }
            }
            ClientFrame::Request { id, request } => {
                let token = match state.begin_request(id).await {
                    Ok(token) => token,
                    Err(error) => {
                        let _ = outbound_tx
                            .send(ServerFrame::Response {
                                id,
                                result: Err(error),
                            })
                            .await;
                        continue;
                    }
                };
                if matches!(request, ClientRequest::Shutdown) {
                    let result = state.handle(request, outbound_tx.clone(), token).await;
                    state.finish_request(id).await;
                    let _ = outbound_tx.send(ServerFrame::Response { id, result }).await;
                    break;
                }

                let request_state = state.clone();
                let request_outbound = outbound_tx.clone();
                let is_transaction = matches!(
                    &request,
                    ClientRequest::ApplyFileTransaction(_)
                        | ClientRequest::UndoFileTransaction { .. }
                );
                let manages_own_cancellation = matches!(&request, ClientRequest::SearchContent(_));
                tokio::spawn(async move {
                    let result = if is_transaction || manages_own_cancellation {
                        request_state
                            .handle(request, request_outbound.clone(), token)
                            .await
                    } else {
                        tokio::select! {
                            _ = token.cancelled() => Err(canceled_error()),
                            result = request_state.handle(
                                request,
                                request_outbound.clone(),
                                token.clone(),
                            ) => result,
                        }
                    };
                    request_state.finish_request(id).await;
                    let _ = request_outbound
                        .send(ServerFrame::Response { id, result })
                        .await;
                });
            }
        }
    }

    state.cancel_all().await;
    drop(outbound_tx);
    writer_task.await??;
    Ok(())
}

async fn write_frames<W>(
    mut writer: W,
    mut outbound: mpsc::Receiver<ServerFrame>,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let mut codec = FrameCodec::with_limits(8 * 1024, MAX_REMOTE_FRAME_BYTES);
    while let Some(frame) = outbound.recv().await {
        codec.write(&mut writer, &frame).await?;
    }
    Ok(())
}

struct ServerState {
    server_version: String,
    handshake_complete: AtomicBool,
    workspace: RwLock<Option<Arc<Workspace>>>,
    search_indexes: Mutex<VecDeque<Arc<SearchIndex>>>,
    operations: Mutex<HashMap<OperationId, Arc<AtomicBool>>>,
    writes: Mutex<HashMap<WriteId, Arc<Mutex<PendingWrite>>>>,
    write_slots: Arc<Semaphore>,
    processes: Arc<ProcessTable>,
    watches: Arc<WatchTable>,
    requests: Mutex<HashMap<RequestId, CancellationToken>>,
    transactions: RwLock<Option<Arc<parking_lot::Mutex<TransactionStore>>>>,
    next_id: AtomicU64,
}

impl ServerState {
    fn new(server_version: String) -> Self {
        Self {
            server_version,
            handshake_complete: AtomicBool::new(false),
            workspace: RwLock::new(None),
            search_indexes: Mutex::new(VecDeque::new()),
            operations: Mutex::new(HashMap::new()),
            writes: Mutex::new(HashMap::new()),
            write_slots: Arc::new(Semaphore::new(MAX_PENDING_WRITES)),
            processes: ProcessTable::new(),
            watches: WatchTable::new(),
            requests: Mutex::new(HashMap::new()),
            transactions: RwLock::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    async fn begin_request(&self, id: RequestId) -> Result<CancellationToken, RemoteError> {
        let token = CancellationToken::new();
        let mut requests = self.requests.lock().await;
        if requests.contains_key(&id) {
            return Err(RemoteError::new(
                ErrorCode::Conflict,
                "remote request ID is already active",
            ));
        }
        if requests.len() >= MAX_IN_FLIGHT_REQUESTS as usize {
            return Err(RemoteError::new(
                ErrorCode::ResourceExhausted,
                "remote request limit reached",
            )
            .retryable());
        }
        requests.insert(id, token.clone());
        Ok(token)
    }

    async fn cancel_request(&self, id: RequestId) {
        if let Some(token) = self.requests.lock().await.remove(&id) {
            token.cancel();
        }
    }

    async fn finish_request(&self, id: RequestId) {
        self.requests.lock().await.remove(&id);
    }

    async fn cancel_all(&self) {
        for (_, token) in self.requests.lock().await.drain() {
            token.cancel();
        }
        for (_, canceled) in self.operations.lock().await.drain() {
            canceled.store(true, Ordering::Release);
        }
        self.abort_all_writes().await;
        self.processes.kill_all().await;
        self.watches.stop_all().await;
    }

    fn next_id(&self) -> Result<u64, RemoteError> {
        self.next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| {
                RemoteError::new(
                    ErrorCode::ResourceExhausted,
                    "remote identifier space is exhausted",
                )
            })
    }

    async fn handle(
        self: &Arc<Self>,
        request: ClientRequest,
        outbound: mpsc::Sender<ServerFrame>,
        canceled: CancellationToken,
    ) -> Result<ServerResponse, RemoteError> {
        if let ClientRequest::Hello(hello) = request {
            return self.hello(hello);
        }
        if !self.handshake_complete.load(Ordering::Acquire) {
            return Err(RemoteError::new(
                ErrorCode::HandshakeRequired,
                "Hello must be the first remote request",
            ));
        }

        match request {
            ClientRequest::Hello(_) => unreachable!(),
            ClientRequest::OpenWorkspace(request) => self.open_workspace(request).await,
            ClientRequest::CloseWorkspace => {
                self.close_workspace(true).await;
                Ok(ServerResponse::Unit)
            }
            ClientRequest::Stat { path } => {
                let workspace = self.workspace().await?;
                workspace.stat(path).await.map(ServerResponse::Metadata)
            }
            ClientRequest::ReadDir(request) => {
                let workspace = self.workspace().await?;
                workspace
                    .read_dir(request.path, request.options)
                    .await
                    .map(ServerResponse::Directory)
            }
            ClientRequest::ReadFile(request) => self.read_file(request).await,
            ClientRequest::SearchFiles(request) => self.start_search(request, outbound).await,
            ClientRequest::SearchContent(request) => self.search_content(request, canceled).await,
            ClientRequest::CancelOperation { operation } => {
                self.cancel_operation(operation).await;
                Ok(ServerResponse::Unit)
            }
            ClientRequest::Ping { nonce } => Ok(ServerResponse::Pong { nonce }),
            ClientRequest::Shutdown => {
                self.close_workspace(true).await;
                Ok(ServerResponse::Unit)
            }
            ClientRequest::BeginWrite(request) => self.begin_write(request).await,
            ClientRequest::WriteChunk(request) => self.write_chunk(request).await,
            ClientRequest::CommitWrite { write } => self.commit_write(write).await,
            ClientRequest::AbortWrite { write } => {
                self.abort_write(write).await;
                Ok(ServerResponse::Unit)
            }
            ClientRequest::Watch(request) => {
                let workspace = self.workspace().await?;
                self.watches
                    .start(request.watch, request, workspace, outbound)
                    .await?;
                Ok(ServerResponse::Unit)
            }
            ClientRequest::Unwatch { watch } => {
                self.watches.stop(watch).await;
                Ok(ServerResponse::Unit)
            }
            ClientRequest::ResolveLanguageServerWorkspace(request) => {
                let workspace = self.workspace().await?;
                let workspace =
                    crate::language_server::resolve_workspace(workspace, request).await?;
                Ok(ServerResponse::LanguageServerWorkspace(workspace))
            }
            ClientRequest::StartProcess(spec) => {
                let workspace = self.workspace().await?;
                self.processes.start(spec, workspace, outbound).await?;
                Ok(ServerResponse::Unit)
            }
            ClientRequest::KillProcess { process } => {
                self.processes.kill(process).await?;
                Ok(ServerResponse::Unit)
            }
            ClientRequest::ResizeProcess { .. } => Err(RemoteError::new(
                ErrorCode::CapabilityUnavailable,
                "remote pseudoterminals are unavailable in this server build",
            )),
            ClientRequest::ApplyFileTransaction(transaction) => {
                self.apply_file_transaction(transaction).await
            }
            ClientRequest::UndoFileTransaction { transaction } => {
                self.undo_file_transaction(transaction).await
            }
        }
    }

    fn hello(&self, hello: ClientHello) -> Result<ServerResponse, RemoteError> {
        if hello.protocol != PROTOCOL_VERSION {
            return Err(RemoteError::new(
                ErrorCode::ProtocolMismatch,
                format!(
                    "remote protocol mismatch: client {}, server {}",
                    hello.protocol, PROTOCOL_VERSION
                ),
            ));
        }
        if hello.client_version.len() > MAX_CLIENT_VERSION_BYTES
            || hello.client_version.contains('\0')
        {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "remote client version is invalid",
            ));
        }
        if hello.requested.len() > MAX_REQUESTED_CAPABILITIES {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "too many remote capabilities were requested",
            ));
        }
        let mut requested = hello.requested;
        requested.sort_unstable();
        if requested.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "remote capabilities must not be duplicated",
            ));
        }
        self.handshake_complete
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                RemoteError::new(ErrorCode::Conflict, "remote handshake is already complete")
            })?;
        Ok(ServerResponse::Hello(ServerHello {
            protocol: PROTOCOL_VERSION,
            server_version: self.server_version.clone(),
            platform: platform(),
            capabilities: vec![
                Capability::FileSystem,
                Capability::FileSearch,
                Capability::FileWatch,
                Capability::FileTransactions,
                Capability::Processes,
                Capability::LanguageServers,
            ],
            limits: ProtocolLimits {
                max_frame_bytes: MAX_REMOTE_FRAME_BYTES as u32,
                max_file_chunk_bytes: MAX_FILE_CHUNK_BYTES,
                max_write_bytes: MAX_WRITE_BYTES,
                max_search_batch: MAX_SEARCH_BATCH,
                max_in_flight_requests: MAX_IN_FLIGHT_REQUESTS,
                max_active_searches: MAX_ACTIVE_SEARCHES as u16,
                max_pending_writes: MAX_PENDING_WRITES as u16,
                max_active_processes: MAX_ACTIVE_PROCESSES as u16,
                max_active_watches: MAX_ACTIVE_WATCHES as u16,
                max_transaction_operations: MAX_TRANSACTION_OPERATIONS as u16,
                max_process_spec_bytes: MAX_PROCESS_SPEC_BYTES as u32,
                max_process_input_bytes: MAX_PROCESS_INPUT_BYTES as u32,
            },
        }))
    }

    async fn open_workspace(&self, request: OpenWorkspace) -> Result<ServerResponse, RemoteError> {
        if self.workspace.read().await.is_some() {
            return Err(RemoteError::new(
                ErrorCode::Conflict,
                "a remote workspace is already open",
            ));
        }
        if request.root.is_empty()
            || request.root.len() > MAX_WORKSPACE_ROOT_BYTES
            || request.root.contains('\0')
        {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "remote workspace root is invalid",
            ));
        }
        if request.session.0 == 0 {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "remote workspace session must be nonzero",
            ));
        }
        let workspace = Arc::new(Workspace::open(request.root, request.session).await?);
        let info = workspace.info();
        *self.transactions.write().await = Some(Arc::new(parking_lot::Mutex::new(
            TransactionStore::open(workspace.root(), request.session)?,
        )));
        *self.workspace.write().await = Some(workspace);
        self.search_indexes.lock().await.clear();
        for (_, canceled) in self.operations.lock().await.drain() {
            canceled.store(true, Ordering::Release);
        }
        self.abort_all_writes().await;
        self.processes.kill_all().await;
        self.watches.stop_all().await;
        Ok(ServerResponse::WorkspaceOpened(info))
    }

    async fn close_workspace(&self, clear_history: bool) {
        *self.workspace.write().await = None;
        if let Some(transactions) = self.transactions.write().await.take() {
            if clear_history {
                transactions.lock().clear();
            }
        }
        self.search_indexes.lock().await.clear();
        for (_, canceled) in self.operations.lock().await.drain() {
            canceled.store(true, Ordering::Release);
        }
        self.abort_all_writes().await;
        self.processes.kill_all().await;
        self.watches.stop_all().await;
    }

    async fn workspace(&self) -> Result<Arc<Workspace>, RemoteError> {
        self.workspace.read().await.clone().ok_or_else(|| {
            RemoteError::new(ErrorCode::WorkspaceNotOpen, "no remote workspace is open")
        })
    }

    async fn apply_file_transaction(
        &self,
        transaction: FileTransaction,
    ) -> Result<ServerResponse, RemoteError> {
        let store = self.transactions.read().await.clone().ok_or_else(|| {
            RemoteError::new(ErrorCode::WorkspaceNotOpen, "no remote workspace is open")
        })?;
        tokio::task::spawn_blocking(move || {
            let mut store = store.lock();
            let transaction_id = store.next_id()?;
            store
                .apply(transaction_id, transaction)
                .map(ServerResponse::FileTransactionApplied)
        })
        .await
        .map_err(|error| {
            RemoteError::new(
                ErrorCode::Internal,
                format!("file transaction worker failed: {error}"),
            )
        })?
    }

    async fn undo_file_transaction(
        &self,
        transaction: TransactionId,
    ) -> Result<ServerResponse, RemoteError> {
        let store = self.transactions.read().await.clone().ok_or_else(|| {
            RemoteError::new(ErrorCode::WorkspaceNotOpen, "no remote workspace is open")
        })?;
        tokio::task::spawn_blocking(move || {
            store
                .lock()
                .undo(transaction)
                .map(|()| ServerResponse::FileTransactionUndone)
        })
        .await
        .map_err(|error| {
            RemoteError::new(
                ErrorCode::Internal,
                format!("file transaction undo worker failed: {error}"),
            )
        })?
    }

    async fn read_file(&self, request: ReadFile) -> Result<ServerResponse, RemoteError> {
        if request.max_bytes == 0 || request.max_bytes > MAX_FILE_CHUNK_BYTES {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                format!("file chunk size must be between 1 and {MAX_FILE_CHUNK_BYTES} bytes"),
            )
            .at(request.path));
        }
        let workspace = self.workspace().await?;
        let resolved = workspace.resolve_existing(&request.path).await?;
        let mut file = tokio::fs::File::open(&resolved)
            .await
            .map_err(|error| io_error(error, Some(request.path.clone())))?;
        let before = file
            .metadata()
            .await
            .map_err(|error| io_error(error, Some(request.path.clone())))?;
        if !before.is_file() {
            return Err(
                RemoteError::new(ErrorCode::InvalidPath, "path is not a file").at(request.path),
            );
        }
        let content = file_metadata(&before)
            .content
            .expect("regular file metadata has a content id");
        if request.expected.is_some_and(|expected| expected != content) {
            return Err(RemoteError::new(
                ErrorCode::Conflict,
                "remote file changed before the requested chunk was read",
            )
            .at(request.path)
            .retryable());
        }
        file.seek(SeekFrom::Start(request.offset))
            .await
            .map_err(|error| io_error(error, Some(request.path.clone())))?;
        let mut bytes = vec![0; request.max_bytes as usize];
        let read = file
            .read(&mut bytes)
            .await
            .map_err(|error| io_error(error, Some(request.path.clone())))?;
        bytes.truncate(read);
        let after = file
            .metadata()
            .await
            .map_err(|error| io_error(error, Some(request.path.clone())))?;
        let after_content = file_metadata(&after)
            .content
            .expect("regular file metadata has a content id");
        if after_content != content {
            return Err(RemoteError::new(
                ErrorCode::Conflict,
                "remote file changed while a chunk was being read",
            )
            .at(request.path)
            .retryable());
        }
        Ok(ServerResponse::FileChunk(FileChunk {
            metadata: file_metadata(&after),
            offset: request.offset,
            bytes: ByteBuf::from(bytes),
            eof: request.offset.saturating_add(read as u64) >= content.len,
        }))
    }

    async fn start_search(
        self: &Arc<Self>,
        request: SearchFiles,
        outbound: mpsc::Sender<ServerFrame>,
    ) -> Result<ServerResponse, RemoteError> {
        if request.query.len() > MAX_SEARCH_QUERY_BYTES || request.query.contains('\0') {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "remote search query is invalid",
            ));
        }
        let workspace = self.workspace().await?;
        let limit = request.limit.clamp(1, 100_000) as usize;
        let index_root = workspace.resolve_existing(&request.root).await?;
        if !tokio::fs::metadata(&index_root)
            .await
            .map_err(|error| io_error(error, Some(request.root.clone())))?
            .is_dir()
        {
            return Err(RemoteError::new(
                ErrorCode::InvalidPath,
                "remote search root is not a directory",
            )
            .at(request.root));
        }
        let index = self
            .search_index(&workspace, index_root, request.options)
            .await?;
        let operation = request.operation;
        let canceled = Arc::new(AtomicBool::new(false));
        let mut operations = self.operations.lock().await;
        if operations.contains_key(&operation) {
            return Err(RemoteError::new(
                ErrorCode::Conflict,
                "remote operation ID is already active",
            ));
        }
        if operations.len() >= MAX_ACTIVE_SEARCHES {
            return Err(RemoteError::new(
                ErrorCode::ResourceExhausted,
                "remote search limit reached",
            )
            .retryable());
        }
        operations.insert(operation, canceled.clone());
        drop(operations);

        let state = self.clone();
        tokio::spawn(async move {
            let mut revision = 0_u64;
            let mut previous: Option<Vec<SearchEntry>> = None;
            let mut previous_scanned = 0;
            loop {
                if canceled.load(Ordering::Acquire) {
                    break;
                }
                let query = request.query.clone();
                let snapshot_index = index.clone();
                let snapshot_canceled = canceled.clone();
                let snapshot = tokio::task::spawn_blocking(move || {
                    snapshot_index.snapshot(
                        &query,
                        limit,
                        Duration::from_millis(40),
                        &snapshot_canceled,
                    )
                })
                .await;
                let snapshot = match snapshot {
                    Ok(Ok(snapshot)) => snapshot,
                    Ok(Err(error)) => {
                        let _ = outbound
                            .send(ServerFrame::Event(ServerEvent::Log(RemoteLog {
                                level: RemoteLogLevel::Error,
                                target: "remote_search".to_owned(),
                                message: error.to_string(),
                            })))
                            .await;
                        break;
                    }
                    Err(error) => {
                        let _ = outbound
                            .send(ServerFrame::Event(ServerEvent::Log(RemoteLog {
                                level: RemoteLogLevel::Error,
                                target: "remote_search".to_owned(),
                                message: format!("remote search worker failed: {error}"),
                            })))
                            .await;
                        break;
                    }
                };
                if canceled.load(Ordering::Acquire) {
                    break;
                }

                let changed = previous.as_ref() != Some(&snapshot.entries)
                    || previous_scanned != snapshot.scanned
                    || snapshot.scan_complete;
                if changed {
                    revision = revision.saturating_add(1);
                    if send_search_revision(
                        &outbound,
                        operation,
                        revision,
                        snapshot.entries.clone(),
                        snapshot.scanned,
                        snapshot.scan_complete,
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                    previous = Some(snapshot.entries);
                    previous_scanned = snapshot.scanned;
                }
                if snapshot.scan_complete {
                    break;
                }
            }
            state.operations.lock().await.remove(&operation);
        });
        Ok(ServerResponse::Unit)
    }

    async fn search_content(
        self: &Arc<Self>,
        request: ContentSearchQuery,
        canceled: CancellationToken,
    ) -> Result<ServerResponse, RemoteError> {
        let abort = Arc::new(AtomicBool::new(false));
        let worker_abort = abort.clone();
        let search = async {
            request
                .validate()
                .map_err(|message| RemoteError::new(ErrorCode::InvalidRequest, message))?;
            let workspace = self.workspace().await?;
            let index_root = workspace.resolve_existing(&request.root).await?;
            if !tokio::fs::metadata(&index_root)
                .await
                .map_err(|error| io_error(error, Some(request.root.clone())))?
                .is_dir()
            {
                return Err(RemoteError::new(
                    ErrorCode::InvalidPath,
                    "content search root is not a directory",
                )
                .at(request.root.clone()));
            }
            let index = self
                .search_index(&workspace, index_root, request.options)
                .await?;
            let worker = tokio::task::spawn_blocking(move || {
                index.content_page(&request, Duration::from_millis(40), worker_abort)
            });
            let page = worker.await.map_err(|error| {
                RemoteError::new(
                    ErrorCode::Internal,
                    format!("remote content search worker failed: {error}"),
                )
            })??;
            Ok(ServerResponse::ContentSearch(page))
        };
        tokio::select! {
            _ = canceled.cancelled() => {
                abort.store(true, Ordering::Release);
                Err(canceled_error())
            }
            result = search => result,
        }
    }

    async fn search_index(
        &self,
        workspace: &Arc<Workspace>,
        index_root: PathBuf,
        options: ScanOptions,
    ) -> Result<Arc<SearchIndex>, RemoteError> {
        {
            let mut indexes = self.search_indexes.lock().await;
            if let Some(position) = indexes
                .iter()
                .position(|index| index.matches(&index_root, options))
            {
                let index = indexes
                    .remove(position)
                    .expect("search index position came from this cache");
                indexes.push_back(index.clone());
                return Ok(index);
            }
        }

        let workspace_root = workspace.root().to_path_buf();
        let index = tokio::task::spawn_blocking(move || {
            SearchIndex::new(workspace_root, index_root, options)
        })
        .await
        .map_err(|error| {
            RemoteError::new(
                ErrorCode::Internal,
                format!("remote index worker failed: {error}"),
            )
        })??;
        let index = Arc::new(index);
        let mut indexes = self.search_indexes.lock().await;
        indexes.push_back(index.clone());
        while indexes.len() > SEARCH_INDEX_CACHE_CAPACITY {
            indexes.pop_front();
        }
        Ok(index)
    }

    async fn cancel_operation(&self, operation: OperationId) {
        if let Some(canceled) = self.operations.lock().await.remove(&operation) {
            canceled.store(true, Ordering::Release);
        }
    }

    async fn begin_write(&self, request: BeginWrite) -> Result<ServerResponse, RemoteError> {
        let slot = self.write_slots.clone().try_acquire_owned().map_err(|_| {
            RemoteError::new(ErrorCode::ResourceExhausted, "remote write limit reached").retryable()
        })?;
        let workspace = self.workspace().await?;
        let destination = workspace
            .resolve_for_write(&request.path, request.create_parents)
            .await?;
        validate_expected_content(&destination, request.expected, &request.path).await?;
        let parent = destination.parent().ok_or_else(|| {
            RemoteError::new(ErrorCode::InvalidPath, "file path has no parent")
                .at(request.path.clone())
        })?;
        let write = WriteId(self.next_id()?);
        let temp = parent.join(format!(".dhx-write-{}-{}", std::process::id(), write.0));
        let file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .await
            .map_err(|error| io_error(error, Some(request.path.clone())))?;
        self.writes.lock().await.insert(
            write,
            Arc::new(Mutex::new(PendingWrite {
                path: request.path,
                destination,
                temp,
                file: Some(file),
                expected: request.expected,
                next_offset: 0,
                _slot: slot,
            })),
        );
        Ok(ServerResponse::WriteStarted { write })
    }

    async fn write_chunk(&self, request: WriteChunk) -> Result<ServerResponse, RemoteError> {
        if request.bytes.len() > MAX_FILE_CHUNK_BYTES as usize {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                format!("write chunk exceeds {MAX_FILE_CHUNK_BYTES} bytes"),
            ));
        }
        let pending = self
            .writes
            .lock()
            .await
            .get(&request.write)
            .cloned()
            .ok_or_else(|| {
                RemoteError::new(ErrorCode::InvalidRequest, "unknown or completed write")
            })?;
        let mut pending = pending.lock().await;
        if request.offset != pending.next_offset {
            return Err(RemoteError::new(
                ErrorCode::Conflict,
                format!(
                    "write chunk offset {} does not match expected offset {}",
                    request.offset, pending.next_offset
                ),
            )
            .at(pending.path.clone()));
        }
        let path = pending.path.clone();
        pending
            .file
            .as_mut()
            .ok_or_else(|| RemoteError::new(ErrorCode::InvalidRequest, "write is completing"))?
            .write_all(&request.bytes)
            .await
            .map_err(|error| io_error(error, Some(path)))?;
        pending.next_offset = pending
            .next_offset
            .checked_add(request.bytes.len() as u64)
            .filter(|next| *next <= MAX_WRITE_BYTES)
            .ok_or_else(|| {
                RemoteError::new(
                    ErrorCode::ResourceExhausted,
                    format!("remote write exceeds {MAX_WRITE_BYTES} bytes"),
                )
            })?;
        Ok(ServerResponse::Unit)
    }

    async fn commit_write(&self, write: WriteId) -> Result<ServerResponse, RemoteError> {
        let pending = self.writes.lock().await.remove(&write).ok_or_else(|| {
            RemoteError::new(ErrorCode::InvalidRequest, "unknown or completed write")
        })?;
        let mut pending = pending.lock().await;
        let result = async {
            let path = pending.path.clone();
            let file = pending.file.as_mut().ok_or_else(|| {
                RemoteError::new(ErrorCode::InvalidRequest, "write is already completing")
            })?;
            file.flush()
                .await
                .map_err(|error| io_error(error, Some(path.clone())))?;
            file.sync_all()
                .await
                .map_err(|error| io_error(error, Some(path)))?;
            validate_expected_content(&pending.destination, pending.expected, &pending.path)
                .await?;

            drop(pending.file.take());
            helix_workspace::atomic_replace(pending.temp.clone(), pending.destination.clone())
                .await
                .map_err(|error| io_error(error, Some(pending.path.clone())))?;
            let metadata = tokio::fs::metadata(&pending.destination)
                .await
                .map_err(|error| io_error(error, Some(pending.path.clone())))?;
            helix_workspace::sync_parent_directory(&pending.destination).await;
            Ok(file_metadata(&metadata))
        }
        .await;
        if result.is_err() {
            drop(pending.file.take());
            let _ = tokio::fs::remove_file(&pending.temp).await;
        }
        result.map(|metadata| ServerResponse::WriteCommitted { metadata })
    }

    async fn abort_write(&self, write: WriteId) {
        let Some(pending) = self.writes.lock().await.remove(&write) else {
            return;
        };
        let mut pending = pending.lock().await;
        drop(pending.file.take());
        let temp = pending.temp.clone();
        drop(pending);
        let _ = tokio::fs::remove_file(temp).await;
    }

    async fn abort_all_writes(&self) {
        let writes = self
            .writes
            .lock()
            .await
            .drain()
            .map(|(_, write)| write)
            .collect::<Vec<_>>();
        for pending in writes {
            let mut pending = pending.lock().await;
            drop(pending.file.take());
            let temp = pending.temp.clone();
            drop(pending);
            let _ = tokio::fs::remove_file(temp).await;
        }
    }
}

struct PendingWrite {
    path: WorkspacePath,
    destination: std::path::PathBuf,
    temp: std::path::PathBuf,
    file: Option<tokio::fs::File>,
    expected: Option<ContentId>,
    next_offset: u64,
    _slot: OwnedSemaphorePermit,
}

async fn validate_expected_content(
    destination: &std::path::Path,
    expected: Option<ContentId>,
    path: &WorkspacePath,
) -> Result<(), RemoteError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let metadata = match tokio::fs::metadata(destination).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(RemoteError::new(
                ErrorCode::Conflict,
                "remote file was removed before it could be saved",
            )
            .at(path.clone()));
        }
        Err(error) => return Err(io_error(error, Some(path.clone()))),
    };
    let actual = file_metadata(&metadata).content;
    if actual != Some(expected) {
        return Err(RemoteError::new(
            ErrorCode::Conflict,
            "remote file changed before it could be saved",
        )
        .at(path.clone()));
    }
    Ok(())
}

async fn send_search_revision(
    outbound: &mpsc::Sender<ServerFrame>,
    operation: OperationId,
    revision: u64,
    entries: Vec<SearchEntry>,
    scanned: u64,
    done: bool,
) -> Result<(), mpsc::error::SendError<ServerFrame>> {
    if entries.is_empty() {
        return outbound
            .send(ServerFrame::Event(ServerEvent::SearchBatch(SearchBatch {
                operation,
                revision,
                batch: 0,
                entries,
                scanned,
                revision_done: true,
                done,
            })))
            .await;
    }
    let chunks = entries.chunks(MAX_SEARCH_BATCH as usize);
    let chunk_count = chunks.len();
    for (batch, chunk) in chunks.enumerate() {
        outbound
            .send(ServerFrame::Event(ServerEvent::SearchBatch(SearchBatch {
                operation,
                revision,
                batch: batch as u32,
                entries: chunk.to_vec(),
                scanned,
                revision_done: batch + 1 == chunk_count,
                done: done && batch + 1 == chunk_count,
            })))
            .await?;
    }
    Ok(())
}

fn platform() -> Platform {
    Platform {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        family: std::env::consts::FAMILY.to_owned(),
        path_separator: std::path::MAIN_SEPARATOR,
        home: std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(|path| path.to_string_lossy().into_owned()),
        shell: std::env::var_os(if cfg!(windows) { "COMSPEC" } else { "SHELL" })
            .map(|path| path.to_string_lossy().into_owned()),
    }
}

fn canceled_error() -> RemoteError {
    RemoteError::new(ErrorCode::Canceled, "remote request was canceled")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, split};

    async fn request(
        codec: &mut FrameCodec,
        client_write: &mut (impl AsyncWrite + Unpin),
        client_read: &mut (impl AsyncRead + Unpin),
        id: u64,
        request: ClientRequest,
    ) -> Result<ServerResponse, RemoteError> {
        codec
            .write(
                client_write,
                &ClientFrame::Request {
                    id: RequestId(id),
                    request,
                },
            )
            .await
            .unwrap();
        loop {
            match codec.read::<ServerFrame, _>(client_read).await.unwrap() {
                ServerFrame::Response {
                    id: response_id,
                    result,
                } if response_id == RequestId(id) => {
                    return result;
                }
                ServerFrame::Response { .. } | ServerFrame::Event(_) => {}
            }
        }
    }

    #[tokio::test]
    async fn loopback_handshake_and_chunked_read() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("sample.txt"), b"abcdef").unwrap();
        let (client, server) = duplex(64 * 1024);
        let (mut client_read, mut client_write) = split(client);
        let (server_read, server_write) = split(server);
        let server_task = tokio::spawn(run_connection(server_read, server_write, "test"));
        let mut codec = FrameCodec::new();

        let hello = request(
            &mut codec,
            &mut client_write,
            &mut client_read,
            1,
            ClientRequest::Hello(ClientHello {
                protocol: PROTOCOL_VERSION,
                client_version: "test".to_owned(),
                requested: vec![Capability::FileSystem],
            }),
        )
        .await
        .unwrap();
        assert!(matches!(hello, ServerResponse::Hello(_)));

        request(
            &mut codec,
            &mut client_write,
            &mut client_read,
            2,
            ClientRequest::OpenWorkspace(OpenWorkspace {
                root: workspace.path().to_string_lossy().into_owned(),
                session: SessionId(1),
            }),
        )
        .await
        .unwrap();
        let chunk = request(
            &mut codec,
            &mut client_write,
            &mut client_read,
            3,
            ClientRequest::ReadFile(ReadFile {
                path: crate::WorkspacePath::from_slash_path("sample.txt").unwrap(),
                offset: 2,
                max_bytes: 3,
                expected: None,
            }),
        )
        .await
        .unwrap();
        let ServerResponse::FileChunk(chunk) = chunk else {
            panic!("expected file chunk");
        };
        assert_eq!(chunk.bytes.as_ref(), b"cde");
        assert!(!chunk.eof);

        request(
            &mut codec,
            &mut client_write,
            &mut client_read,
            4,
            ClientRequest::Shutdown,
        )
        .await
        .unwrap();
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_content_search_pages_use_workspace_paths() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("sample.txt"),
            b"first line\nremote needle\n",
        )
        .unwrap();
        let (client, server) = duplex(64 * 1024);
        let (mut client_read, mut client_write) = split(client);
        let (server_read, server_write) = split(server);
        let server_task = tokio::spawn(run_connection(server_read, server_write, "test"));
        let mut codec = FrameCodec::new();

        request(
            &mut codec,
            &mut client_write,
            &mut client_read,
            1,
            ClientRequest::Hello(ClientHello {
                protocol: PROTOCOL_VERSION,
                client_version: "test".to_owned(),
                requested: vec![Capability::FileSearch],
            }),
        )
        .await
        .unwrap();
        request(
            &mut codec,
            &mut client_write,
            &mut client_read,
            2,
            ClientRequest::OpenWorkspace(OpenWorkspace {
                root: workspace.path().to_string_lossy().into_owned(),
                session: SessionId(2),
            }),
        )
        .await
        .unwrap();

        let mut cursor = ContentSearchCursor::default();
        let mut entries = Vec::new();
        for id in 3..67 {
            let response = request(
                &mut codec,
                &mut client_write,
                &mut client_read,
                id,
                ClientRequest::SearchContent(ContentSearchQuery {
                    root: WorkspacePath::root(),
                    pattern: "needle".to_owned(),
                    smart_case: true,
                    options: ScanOptions::default(),
                    excluded_paths: Vec::new(),
                    cursor,
                    limit: 4,
                }),
            )
            .await
            .unwrap();
            let ServerResponse::ContentSearch(page) = response else {
                panic!("expected content search page")
            };
            entries.extend(page.entries);
            if page.done {
                break;
            }
            cursor = page.next.expect("unfinished search must continue");
        }
        assert_eq!(
            entries,
            vec![ContentSearchEntry {
                path: WorkspacePath::from_slash_path("sample.txt").unwrap(),
                line: 1,
            }]
        );

        request(
            &mut codec,
            &mut client_write,
            &mut client_read,
            67,
            ClientRequest::Shutdown,
        )
        .await
        .unwrap();
        server_task.await.unwrap().unwrap();
    }
}
