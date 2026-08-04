use crate::{
    Buffer, BufferId, Event, OpenBufferInfo, ParticipantId, ProjectId, ProjectInfo, ProjectState,
    ProtocolError, Request, Response, MAX_COLLABORATIVE_FILE_BYTES,
    MAX_FILE_TRANSACTION_OPERATIONS, MAX_OPEN_BUFFERS, MAX_PROJECT_FILES,
    MAX_SYNC_MESSAGE_CHUNK_BYTES,
};
use fff_search::{
    grep_byte_sources_page, ByteSourceGrepCursor, GrepConfig, GrepMode, GrepSearchOptions,
    QueryParser,
};
use helix_workspace::{
    ContentSearchCursor, ContentSearchEntry, ContentSearchPage, ContentSearchQuery, FileChange,
    FileOperation, FileTransaction, ScanOptions, WorkspacePath,
};
use parking_lot::Mutex;
use serde_bytes::ByteBuf;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};
use tokio::sync::RwLock;

const FILE_INDEX_TTL: std::time::Duration = std::time::Duration::from_secs(2);

pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ProjectError>> + Send + 'a>>;

pub trait Backend: Send + Sync + 'static {
    fn watch_files(&self) -> BackendFuture<'_, Option<BackendFileWatch>> {
        Box::pin(async { Ok(None) })
    }

    fn list_files(&self, options: ScanOptions) -> BackendFuture<'_, Vec<WorkspacePath>>;
    fn search_content(
        &self,
        query: ContentSearchQuery,
        canceled: tokio_util::sync::CancellationToken,
    ) -> BackendFuture<'_, ContentSearchPage>;
    fn read_file(&self, path: WorkspacePath) -> BackendFuture<'_, FileData>;
    fn path_exists(&self, path: WorkspacePath) -> BackendFuture<'_, bool>;
    fn write_file(
        &self,
        path: WorkspacePath,
        expected: Option<FileVersion>,
        bytes: Vec<u8>,
    ) -> BackendFuture<'_, FileVersion>;
    fn apply_file_transaction(
        &self,
        transaction: FileTransaction,
    ) -> BackendFuture<'_, BackendTransactionId>;
    fn undo_file_transaction(&self, transaction: BackendTransactionId) -> BackendFuture<'_, ()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendFileUpdate {
    Changes(Vec<FileChange>),
    Rescan,
}

pub struct BackendFileWatch {
    updates: tokio::sync::mpsc::Receiver<BackendFileUpdate>,
    _keepalive: Box<dyn Send>,
}

impl BackendFileWatch {
    pub(crate) fn new(
        updates: tokio::sync::mpsc::Receiver<BackendFileUpdate>,
        keepalive: impl Send + 'static,
    ) -> Self {
        Self {
            updates,
            _keepalive: Box::new(keepalive),
        }
    }

    pub async fn recv(&mut self) -> Option<BackendFileUpdate> {
        self.updates.recv().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackendTransactionId(pub u64);

pub const MAX_FILE_VERSION_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct FileVersion(ByteBuf);

impl FileVersion {
    pub fn new(bytes: impl Into<ByteBuf>) -> Result<Self, FileVersionError> {
        let bytes = bytes.into();
        if bytes.len() > MAX_FILE_VERSION_BYTES {
            return Err(FileVersionError);
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl<'de> serde::Deserialize<'de> for FileVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = ByteBuf::deserialize(deserializer)?;
        Self::new(bytes).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("file version exceeds the protocol limit")]
pub struct FileVersionError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileData {
    pub bytes: Vec<u8>,
    pub version: FileVersion,
}

struct OpenBuffer {
    path: WorkspacePath,
    document: Buffer,
    version: FileVersion,
    generation: u64,
    dirty: bool,
    search_snapshot: Option<(u64, Arc<[u8]>)>,
}

struct Buffers {
    by_id: HashMap<BufferId, Arc<Mutex<OpenBuffer>>>,
    by_path: HashMap<WorkspacePath, BufferId>,
    leases: HashMap<BufferId, HashSet<ParticipantId>>,
    next_id: u64,
}

impl Default for Buffers {
    fn default() -> Self {
        Self {
            by_id: HashMap::new(),
            by_path: HashMap::new(),
            leases: HashMap::new(),
            next_id: 1,
        }
    }
}

pub struct Project {
    id: ProjectId,
    name: String,
    owner: ParticipantId,
    backend: Arc<dyn Backend>,
    buffers: RwLock<Buffers>,
    file_indexes: tokio::sync::Mutex<HashMap<ScanOptions, CachedFileIndex>>,
    file_history: tokio::sync::Mutex<FileHistory>,
    file_mutations: Arc<tokio::sync::Mutex<()>>,
    file_revision: Mutex<u64>,
}

struct CachedFileIndex {
    files: Arc<[WorkspacePath]>,
    loaded: std::time::Instant,
}

#[derive(Default)]
struct FileHistory {
    participants: HashMap<ParticipantId, ParticipantFileHistory>,
}

#[derive(Default)]
struct ParticipantFileHistory {
    undo: Vec<AppliedFileTransaction>,
    redo: Vec<FileTransaction>,
}

struct AppliedFileTransaction {
    transaction: FileTransaction,
    backend: BackendTransactionId,
}

struct BufferPathUpdate {
    paths: Vec<(BufferId, WorkspacePath)>,
}

pub(crate) struct ExternalFileMutation {
    project: Arc<Project>,
    transaction: FileTransaction,
    paths: BufferPathUpdate,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

pub(crate) struct ReservedProjectState {
    pub(crate) state: ProjectState,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl ExternalFileMutation {
    pub(crate) async fn commit(self) -> (FileTransaction, u64) {
        self.project.commit_buffer_paths(self.paths).await;
        self.project.file_indexes.lock().await.clear();
        let revision = self.project.advance_file_revision();
        (self.transaction, revision)
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub response: Response,
    pub deliveries: Vec<(ParticipantId, Event)>,
}

impl Project {
    pub fn new(
        name: impl Into<String>,
        owner: ParticipantId,
        backend: Arc<dyn Backend>,
    ) -> Result<Self, ProjectError> {
        let name = name.into();
        if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
            return Err(ProjectError::InvalidName);
        }
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|error| ProjectError::Entropy(error.to_string()))?;
        Ok(Self {
            id: ProjectId(id),
            name,
            owner,
            backend,
            buffers: RwLock::new(Buffers::default()),
            file_indexes: tokio::sync::Mutex::new(HashMap::new()),
            file_history: tokio::sync::Mutex::new(FileHistory::default()),
            file_mutations: Arc::new(tokio::sync::Mutex::new(())),
            file_revision: Mutex::new(0),
        })
    }

    pub fn id(&self) -> ProjectId {
        self.id
    }

    pub fn info(&self, participants: Vec<crate::ParticipantInfo>) -> ProjectInfo {
        ProjectInfo {
            id: self.id,
            name: self.name.clone(),
            owner: self.owner,
            participants,
        }
    }

    pub(crate) async fn reserve_external_file_mutation(
        self: Arc<Self>,
        transaction: FileTransaction,
    ) -> Result<ExternalFileMutation, ProjectError> {
        let guard = self.file_mutations.clone().lock_owned().await;
        let paths = self.validate_file_transaction(&transaction).await?;
        Ok(ExternalFileMutation {
            project: self,
            transaction,
            paths,
            _guard: guard,
        })
    }

    pub(crate) async fn reserve_file_state(
        self: Arc<Self>,
        participants: Vec<crate::ParticipantInfo>,
    ) -> ReservedProjectState {
        let guard = self.file_mutations.clone().lock_owned().await;
        let state = self.snapshot_file_state(participants).await;
        ReservedProjectState {
            state,
            _guard: guard,
        }
    }

    pub(crate) async fn watch_files(&self) -> Result<Option<BackendFileWatch>, ProjectError> {
        self.backend.watch_files().await
    }

    pub(crate) async fn publish_external_file_update(
        &self,
        update: BackendFileUpdate,
    ) -> (u64, BackendFileUpdate) {
        let _mutation = self.file_mutations.lock().await;
        self.file_indexes.lock().await.clear();
        (self.advance_file_revision(), update)
    }

    pub async fn handle(
        &self,
        actor: ParticipantId,
        request: Request,
        participants: Vec<ParticipantId>,
        info: Vec<crate::ParticipantInfo>,
    ) -> Result<Outcome, ProjectError> {
        match request {
            Request::ProjectInfo => Ok(Outcome {
                response: Response::Project(self.info(info)),
                deliveries: Vec::new(),
            }),
            Request::ProjectState => Ok(Outcome {
                response: Response::ProjectState(self.file_state(info).await),
                deliveries: Vec::new(),
            }),
            Request::ListFiles {
                options,
                after,
                limit,
            } => {
                let (entries, next) = self.list_files(options, after, limit).await?;
                Ok(Outcome {
                    response: Response::Files { entries, next },
                    deliveries: Vec::new(),
                })
            }
            Request::SearchContent(_) => Err(ProjectError::InvalidContentSearch(
                "content search reached the ordered project handler".to_owned(),
            )),
            Request::OpenBuffer { path } => {
                let (buffer, epoch, snapshot, compacted) = self.open_buffer(actor, path).await?;
                let participants = self.lease_holders(buffer, participants).await;
                Ok(Outcome {
                    response: Response::Buffer {
                        buffer,
                        epoch,
                        total_bytes: snapshot.len() as u64,
                        snapshot: snapshot.into(),
                        continuation: None,
                    },
                    deliveries: snapshot_compaction_deliveries(
                        actor,
                        participants,
                        buffer,
                        epoch,
                        compacted,
                    ),
                })
            }
            Request::PathExists { path } => Ok(Outcome {
                response: Response::PathExists(self.backend.path_exists(path).await?),
                deliveries: Vec::new(),
            }),
            Request::ReadBuffer { buffer } => {
                let (epoch, snapshot, compacted) = self.snapshot(actor, buffer).await?;
                let participants = self.lease_holders(buffer, participants).await;
                Ok(Outcome {
                    response: Response::Buffer {
                        buffer,
                        epoch,
                        total_bytes: snapshot.len() as u64,
                        snapshot: snapshot.into(),
                        continuation: None,
                    },
                    deliveries: snapshot_compaction_deliveries(
                        actor,
                        participants,
                        buffer,
                        epoch,
                        compacted,
                    ),
                })
            }
            Request::CloseBuffer { buffer } => {
                self.release_buffer(actor, buffer).await?;
                Ok(Outcome {
                    response: Response::Unit,
                    deliveries: Vec::new(),
                })
            }
            Request::SyncBuffer {
                buffer,
                epoch,
                message,
            } => {
                let deliveries = self
                    .sync(buffer, actor, epoch, message.into_vec(), participants)
                    .await?;
                Ok(Outcome {
                    response: Response::Unit,
                    deliveries,
                })
            }
            Request::ContinueBufferSnapshot { .. }
            | Request::StartBufferSync { .. }
            | Request::ContinueBufferSync { .. } => Err(ProjectError::ManagementRequest),
            Request::SaveBuffer { buffer, overwrite } => {
                let version = self.save(actor, buffer, overwrite).await?;
                let participants = self.lease_holders(buffer, participants).await;
                Ok(Outcome {
                    response: Response::BufferSaved {
                        version: version.clone(),
                    },
                    deliveries: participants
                        .into_iter()
                        .map(|participant| {
                            (
                                participant,
                                Event::BufferSaved {
                                    buffer,
                                    version: version.clone(),
                                },
                            )
                        })
                        .collect(),
                })
            }
            Request::ApplyFileTransaction { transaction } => {
                let file_revision = self
                    .apply_file_transaction(actor, transaction.clone())
                    .await?;
                Ok(Outcome {
                    response: Response::FileTransaction { changed: true },
                    deliveries: participants
                        .into_iter()
                        .map(|participant| {
                            (
                                participant,
                                Event::FilesChanged {
                                    file_revision,
                                    transaction: transaction.clone(),
                                    undone: false,
                                },
                            )
                        })
                        .collect(),
                })
            }
            Request::ReplayFileTransaction { redo } => {
                let replay = self.replay_file_transaction(actor, redo).await?;
                let changed = replay.is_some();
                Ok(Outcome {
                    response: Response::FileTransaction { changed },
                    deliveries: replay.map_or_else(Vec::new, |(transaction, file_revision)| {
                        participants
                            .into_iter()
                            .map(|participant| {
                                (
                                    participant,
                                    Event::FilesChanged {
                                        file_revision,
                                        transaction: transaction.clone(),
                                        undone: !redo,
                                    },
                                )
                            })
                            .collect()
                    }),
                })
            }
            Request::PublishPresence(presence) => {
                if presence.participant != actor {
                    return Err(ProjectError::InvalidPresence);
                }
                self.open_for(actor, presence.buffer).await?;
                Ok(Outcome {
                    response: Response::Unit,
                    deliveries: participants
                        .into_iter()
                        .filter(|participant| *participant != actor)
                        .map(|participant| (participant, Event::Presence(presence.clone())))
                        .collect(),
                })
            }
            Request::Follow { .. } => Err(ProjectError::ManagementRequest),
            Request::LanguageServer { .. } => Err(ProjectError::ManagementRequest),
            Request::Invite { .. }
            | Request::SetRole { .. }
            | Request::RemoveParticipant { .. }
            | Request::Leave => Err(ProjectError::ManagementRequest),
        }
    }

    async fn apply_file_transaction(
        &self,
        actor: ParticipantId,
        transaction: FileTransaction,
    ) -> Result<u64, ProjectError> {
        let _mutation = self.file_mutations.lock().await;
        let paths = self.validate_file_transaction(&transaction).await?;
        let mut history = self.file_history.lock().await;
        let backend = self
            .backend
            .apply_file_transaction(transaction.clone())
            .await?;
        self.commit_buffer_paths(paths).await;
        let participant = history.participants.entry(actor).or_default();
        participant.undo.push(AppliedFileTransaction {
            transaction,
            backend,
        });
        participant.redo.clear();
        drop(history);
        self.file_indexes.lock().await.clear();
        Ok(self.advance_file_revision())
    }

    async fn replay_file_transaction(
        &self,
        actor: ParticipantId,
        redo: bool,
    ) -> Result<Option<(FileTransaction, u64)>, ProjectError> {
        let _mutation = self.file_mutations.lock().await;
        let mut history = self.file_history.lock().await;
        let participant = history.participants.entry(actor).or_default();
        if redo {
            let Some(transaction) = participant.redo.pop() else {
                return Ok(None);
            };
            let paths = self.validate_file_transaction(&transaction).await?;
            match self
                .backend
                .apply_file_transaction(transaction.clone())
                .await
            {
                Ok(backend) => {
                    self.commit_buffer_paths(paths).await;
                    participant.undo.push(AppliedFileTransaction {
                        transaction: transaction.clone(),
                        backend,
                    });
                    drop(history);
                    self.file_indexes.lock().await.clear();
                    Ok(Some((transaction, self.advance_file_revision())))
                }
                Err(error) => {
                    participant.redo.push(transaction);
                    Err(error)
                }
            }
        } else {
            let Some(applied) = participant.undo.pop() else {
                return Ok(None);
            };
            let paths = match self.plan_buffer_paths(&applied.transaction, true).await {
                Ok(paths) => paths,
                Err(error) => {
                    participant.undo.push(applied);
                    return Err(error);
                }
            };
            match self.backend.undo_file_transaction(applied.backend).await {
                Ok(()) => {
                    self.commit_buffer_paths(paths).await;
                    participant.redo.push(applied.transaction.clone());
                    drop(history);
                    self.file_indexes.lock().await.clear();
                    Ok(Some((applied.transaction, self.advance_file_revision())))
                }
                Err(error) => {
                    participant.undo.push(applied);
                    Err(error)
                }
            }
        }
    }

    async fn validate_file_transaction(
        &self,
        transaction: &FileTransaction,
    ) -> Result<BufferPathUpdate, ProjectError> {
        if transaction.operations.is_empty()
            || transaction.operations.len() > MAX_FILE_TRANSACTION_OPERATIONS
        {
            return Err(ProjectError::InvalidFileTransaction(
                "file transaction has an invalid operation count",
            ));
        }
        if transaction.operations.iter().any(|operation| {
            let paths: [&WorkspacePath; 2] = match operation {
                FileOperation::CreateFile { path, .. }
                | FileOperation::CreateDirectory { path }
                | FileOperation::Remove { path, .. } => [path, path],
                FileOperation::Copy { from, to, .. } | FileOperation::Rename { from, to, .. } => {
                    [from, to]
                }
            };
            paths
                .into_iter()
                .any(|path| path.is_root() || helix_workspace::is_internal_path(path))
        }) {
            return Err(ProjectError::InvalidFileTransaction(
                "file transaction targets a protected workspace path",
            ));
        }
        let open = self
            .buffers
            .read()
            .await
            .by_id
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for operation in &transaction.operations {
            let protected = match operation {
                FileOperation::Remove { path, .. } => Some((path, "remove")),
                FileOperation::Rename {
                    from: _,
                    to,
                    overwrite: true,
                } => Some((to, "overwrite")),
                FileOperation::CreateFile {
                    path,
                    overwrite: true,
                } => Some((path, "overwrite")),
                FileOperation::Copy {
                    to,
                    overwrite: true,
                    ..
                } => Some((to, "overwrite")),
                _ => None,
            };
            let Some((target, action)) = protected else {
                continue;
            };
            let open_target = open.iter().any(|open| open.lock().path.starts_with(target));
            if open_target {
                return Err(ProjectError::Conflict(format!(
                    "cannot {action} an open collaborative buffer"
                )));
            }
        }
        self.plan_buffer_paths(transaction, false).await
    }

    async fn plan_buffer_paths(
        &self,
        transaction: &FileTransaction,
        reverse: bool,
    ) -> Result<BufferPathUpdate, ProjectError> {
        let buffers = self.buffers.read().await;
        let mut paths = buffers
            .by_id
            .iter()
            .map(|(&buffer, open)| (buffer, open.lock().path.clone()))
            .collect::<Vec<_>>();
        let operations: Box<dyn Iterator<Item = &FileOperation>> = if reverse {
            Box::new(transaction.operations.iter().rev())
        } else {
            Box::new(transaction.operations.iter())
        };
        for operation in operations {
            let FileOperation::Rename { from, to, .. } = operation else {
                continue;
            };
            let (from, to) = if reverse { (to, from) } else { (from, to) };
            for (_, path) in &mut paths {
                if let Some(relative) = path.strip_prefix(from) {
                    *path = to.join_path(&relative).map_err(|_| {
                        ProjectError::InvalidFileTransaction(
                            "renamed buffer path exceeds workspace path limits",
                        )
                    })?;
                }
            }
        }
        let mut unique = HashMap::with_capacity(paths.len());
        for (buffer, path) in &paths {
            if unique.insert(path, buffer).is_some() {
                return Err(ProjectError::Conflict(
                    "file transaction would merge open collaborative buffers".to_owned(),
                ));
            }
        }
        Ok(BufferPathUpdate { paths })
    }

    async fn commit_buffer_paths(&self, update: BufferPathUpdate) {
        let mut buffers = self.buffers.write().await;
        buffers.by_path.clear();
        for (buffer, path) in update.paths {
            let open = buffers
                .by_id
                .get(&buffer)
                .expect("buffer path plans are committed under the mutation gate");
            open.lock().path = path.clone();
            let previous = buffers.by_path.insert(path, buffer);
            debug_assert!(previous.is_none(), "buffer path plan must be unique");
        }
    }

    async fn file_state(&self, participants: Vec<crate::ParticipantInfo>) -> ProjectState {
        let _mutation = self.file_mutations.lock().await;
        self.snapshot_file_state(participants).await
    }

    async fn snapshot_file_state(&self, participants: Vec<crate::ParticipantInfo>) -> ProjectState {
        let buffers = self.buffers.read().await;
        let mut open_buffers = buffers
            .by_id
            .iter()
            .map(|(&buffer, open)| OpenBufferInfo {
                buffer,
                path: open.lock().path.clone(),
            })
            .collect::<Vec<_>>();
        open_buffers.sort_unstable_by_key(|open| open.buffer);
        ProjectState {
            file_revision: *self.file_revision.lock(),
            open_buffers,
            participants,
        }
    }

    fn advance_file_revision(&self) -> u64 {
        let mut revision = self.file_revision.lock();
        *revision = revision
            .checked_add(1)
            .expect("collaboration file revision exhausted");
        *revision
    }

    async fn open_buffer(
        &self,
        actor: ParticipantId,
        path: WorkspacePath,
    ) -> Result<(BufferId, u64, Vec<u8>, bool), ProjectError> {
        let _mutation = self.file_mutations.lock().await;
        let existing = {
            let mut buffers = self.buffers.write().await;
            let buffer = buffers.by_path.get(&path).copied();
            if let Some(buffer) = buffer {
                buffers.leases.entry(buffer).or_default().insert(actor);
            }
            buffer
        };
        if let Some(buffer) = existing {
            let (epoch, snapshot, compacted) = snapshot_open(self.open(buffer).await?).await?;
            return Ok((buffer, epoch, snapshot, compacted));
        }
        if self.buffers.read().await.by_id.len() >= MAX_OPEN_BUFFERS {
            return Err(ProjectError::ResourceExhausted(
                "too many open collaborative buffers",
            ));
        }
        let file = self.backend.read_file(path.clone()).await?;
        if file.bytes.len() > MAX_COLLABORATIVE_FILE_BYTES {
            return Err(ProjectError::FileTooLarge);
        }
        let content = String::from_utf8(file.bytes).map_err(|_| ProjectError::BinaryFile)?;
        let owner = self.owner;
        let document = tokio::task::spawn_blocking(move || Buffer::new(owner, &content))
            .await
            .map_err(ProjectError::Worker)??;
        let mut buffers = self.buffers.write().await;
        if let Some(buffer) = buffers.by_path.get(&path).copied() {
            buffers.leases.entry(buffer).or_default().insert(actor);
            drop(buffers);
            let (epoch, snapshot, compacted) = snapshot_open(self.open(buffer).await?).await?;
            return Ok((buffer, epoch, snapshot, compacted));
        }
        let id = BufferId(buffers.next_id);
        buffers.next_id = buffers
            .next_id
            .checked_add(1)
            .ok_or(ProjectError::ResourceExhausted(
                "collaboration buffer ID space exhausted",
            ))?;
        let open = Arc::new(Mutex::new(OpenBuffer {
            path: path.clone(),
            document,
            version: file.version,
            generation: 0,
            dirty: false,
            search_snapshot: None,
        }));
        buffers.by_path.insert(path, id);
        buffers.by_id.insert(id, open.clone());
        buffers.leases.insert(id, HashSet::from([actor]));
        drop(buffers);
        let (epoch, snapshot, compacted) = snapshot_open(open).await?;
        Ok((id, epoch, snapshot, compacted))
    }

    async fn list_files(
        &self,
        options: ScanOptions,
        after: Option<WorkspacePath>,
        limit: u16,
    ) -> Result<(Vec<WorkspacePath>, Option<WorkspacePath>), ProjectError> {
        if limit == 0 || limit > crate::MAX_FILE_PAGE_ENTRIES {
            return Err(ProjectError::InvalidFilePageLimit(limit));
        }
        let cached = {
            let indexes = self.file_indexes.lock().await;
            indexes
                .get(&options)
                .map(|index| (index.files.clone(), index.loaded.elapsed() < FILE_INDEX_TTL))
        };
        let files = match cached {
            Some((files, fresh)) if after.is_some() || fresh => files,
            _ => {
                let mut files = self.backend.list_files(options).await?;
                if files.len() > MAX_PROJECT_FILES {
                    return Err(ProjectError::ResourceExhausted(
                        "project contains too many files",
                    ));
                }
                files.sort_unstable();
                files.dedup();
                let files = Arc::<[WorkspacePath]>::from(files);
                self.file_indexes.lock().await.insert(
                    options,
                    CachedFileIndex {
                        files: files.clone(),
                        loaded: std::time::Instant::now(),
                    },
                );
                files
            }
        };
        let start = after
            .as_ref()
            .map_or(0, |after| files.partition_point(|path| path <= after));
        let end = start.saturating_add(limit as usize).min(files.len());
        let entries = files[start..end].to_vec();
        let next = (end < files.len()).then(|| {
            entries
                .last()
                .expect("a non-final file page cannot be empty")
                .clone()
        });
        Ok((entries, next))
    }

    pub(crate) async fn search_content(
        &self,
        mut query: ContentSearchQuery,
        canceled: tokio_util::sync::CancellationToken,
    ) -> Result<ContentSearchPage, ProjectError> {
        if canceled.is_cancelled() {
            return Err(ProjectError::Conflict(
                "content search was canceled".to_owned(),
            ));
        }
        if !query.excluded_paths.is_empty() {
            return Err(ProjectError::InvalidContentSearch(
                "guest-supplied excluded paths are not allowed".to_owned(),
            ));
        }
        query
            .validate()
            .map_err(|message| ProjectError::InvalidContentSearch(message.to_owned()))?;

        let open = self
            .buffers
            .read()
            .await
            .by_id
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let root = query.root.clone();
        let overlay_collection = tokio::task::spawn_blocking(move || {
            let mut overlays = open
                .into_iter()
                .filter_map(|open| {
                    let guard = open.lock();
                    (guard.dirty && guard.path.starts_with(&root))
                        .then(|| (guard.path.clone(), open.clone()))
                })
                .collect::<Vec<_>>();
            overlays.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            overlays
        });
        let overlays = tokio::select! {
            _ = canceled.cancelled() => {
                return Err(ProjectError::Conflict("content search was canceled".to_owned()));
            }
            result = overlay_collection => result.map_err(ProjectError::Worker)?,
        };
        query.excluded_paths = overlays.iter().map(|(path, _)| path.clone()).collect();
        query
            .validate()
            .map_err(|message| ProjectError::InvalidContentSearch(message.to_owned()))?;

        let pattern = query.pattern.clone();
        let smart_case = query.smart_case;
        let cursor = query.cursor;
        let limit = usize::from(query.limit);
        let overlay_search = tokio::task::spawn_blocking(move || {
            search_content_overlays(pattern, smart_case, overlays, cursor, limit)
        });
        let overlay_page = tokio::select! {
            _ = canceled.cancelled() => {
                return Err(ProjectError::Conflict("content search was canceled".to_owned()));
            }
            result = overlay_search => result.map_err(ProjectError::Worker)??,
        };
        if let Some(next) = overlay_page.next {
            return Ok(ContentSearchPage {
                entries: overlay_page.entries,
                next: Some(next),
                scanned: 0,
                done: false,
            });
        }

        let mut entries = overlay_page.entries;
        if entries.len() >= limit {
            return Ok(ContentSearchPage {
                entries,
                next: Some(ContentSearchCursor {
                    overlay: u16::try_from(overlay_page.overlay_count).unwrap_or(u16::MAX),
                    overlay_match: 0,
                    file_offset: query.cursor.file_offset,
                }),
                scanned: 0,
                done: false,
            });
        }
        query.cursor.overlay = u16::try_from(overlay_page.overlay_count).map_err(|_| {
            ProjectError::ResourceExhausted("too many dirty content-search overlays")
        })?;
        query.cursor.overlay_match = 0;
        query.limit = u16::try_from(limit - entries.len())
            .expect("content search page remainder fits in u16");
        let mut disk = self.backend.search_content(query, canceled).await?;
        entries.append(&mut disk.entries);
        Ok(ContentSearchPage {
            entries,
            next: disk.next,
            scanned: disk.scanned,
            done: disk.done,
        })
    }

    async fn snapshot(
        &self,
        actor: ParticipantId,
        buffer: BufferId,
    ) -> Result<(u64, Vec<u8>, bool), ProjectError> {
        snapshot_open(self.open_for(actor, buffer).await?).await
    }

    async fn sync(
        &self,
        buffer: BufferId,
        actor: ParticipantId,
        epoch: u64,
        message: Vec<u8>,
        participants: Vec<ParticipantId>,
    ) -> Result<Vec<(ParticipantId, Event)>, ProjectError> {
        let (open, participants) = self
            .open_for_with_holders(actor, buffer, participants)
            .await?;
        tokio::task::spawn_blocking(move || {
            let mut open = open.lock();
            let changed = !open
                .document
                .receive_sync(actor, epoch, &message)
                .map_err(|error| match error {
                    crate::BufferError::EpochMismatch { .. }
                    | crate::BufferError::ContentTooLarge
                    | crate::BufferError::UnsupportedChange => ProjectError::EpochMismatch,
                    error => ProjectError::Buffer(error),
                })?
                .is_empty();
            if changed {
                open.generation = open.generation.saturating_add(1);
                open.dirty = true;
            }
            let epoch = open.document.epoch();
            let mut deliveries = Vec::with_capacity(participants.len());
            for participant in participants {
                if let Some(message) = open.document.sync_message(participant)? {
                    deliveries.push((participant, buffer_sync_event(buffer, epoch, message)));
                }
            }
            Ok(deliveries)
        })
        .await
        .map_err(ProjectError::Worker)?
    }

    async fn save(
        &self,
        actor: ParticipantId,
        buffer: BufferId,
        overwrite: bool,
    ) -> Result<FileVersion, ProjectError> {
        let _mutation = self.file_mutations.lock().await;
        let open = self.open_for(actor, buffer).await?;
        let (path, version, generation, bytes) = tokio::task::spawn_blocking({
            let open = open.clone();
            move || {
                let open = open.lock();
                Ok::<_, ProjectError>((
                    open.path.clone(),
                    open.version.clone(),
                    open.generation,
                    open.document.text().as_bytes().to_vec(),
                ))
            }
        })
        .await
        .map_err(ProjectError::Worker)??;
        let expected = (!overwrite).then_some(version);
        let next_version = self.backend.write_file(path, expected, bytes).await?;
        let mut open = open.lock();
        open.version = next_version.clone();
        if open.generation == generation {
            open.dirty = false;
        }
        Ok(next_version)
    }

    async fn open_for(
        &self,
        actor: ParticipantId,
        buffer: BufferId,
    ) -> Result<Arc<Mutex<OpenBuffer>>, ProjectError> {
        let buffers = self.buffers.read().await;
        if !buffers
            .leases
            .get(&buffer)
            .is_some_and(|participants| participants.contains(&actor))
        {
            return Err(ProjectError::BufferNotLeased(buffer));
        }
        buffers
            .by_id
            .get(&buffer)
            .cloned()
            .ok_or(ProjectError::BufferNotFound(buffer))
    }

    async fn open_for_with_holders(
        &self,
        actor: ParticipantId,
        buffer: BufferId,
        connected: Vec<ParticipantId>,
    ) -> Result<(Arc<Mutex<OpenBuffer>>, Vec<ParticipantId>), ProjectError> {
        let buffers = self.buffers.read().await;
        let Some(leases) = buffers.leases.get(&buffer) else {
            return Err(ProjectError::BufferNotLeased(buffer));
        };
        if !leases.contains(&actor) {
            return Err(ProjectError::BufferNotLeased(buffer));
        }
        let open = buffers
            .by_id
            .get(&buffer)
            .cloned()
            .ok_or(ProjectError::BufferNotFound(buffer))?;
        let participants = connected
            .into_iter()
            .filter(|participant| leases.contains(participant))
            .collect();
        Ok((open, participants))
    }

    async fn lease_holders(
        &self,
        buffer: BufferId,
        connected: Vec<ParticipantId>,
    ) -> Vec<ParticipantId> {
        let buffers = self.buffers.read().await;
        let Some(leases) = buffers.leases.get(&buffer) else {
            return Vec::new();
        };
        connected
            .into_iter()
            .filter(|participant| leases.contains(participant))
            .collect()
    }

    pub(crate) async fn lease_holders_for_path(
        &self,
        path: &WorkspacePath,
        connected: Vec<ParticipantId>,
    ) -> Vec<ParticipantId> {
        let buffers = self.buffers.read().await;
        let Some(leases) = buffers
            .by_path
            .get(path)
            .and_then(|buffer| buffers.leases.get(buffer))
        else {
            return Vec::new();
        };
        connected
            .into_iter()
            .filter(|participant| leases.contains(participant))
            .collect()
    }

    pub(crate) async fn participants_with_leases(
        &self,
        connected: Vec<ParticipantId>,
    ) -> Vec<ParticipantId> {
        let buffers = self.buffers.read().await;
        connected
            .into_iter()
            .filter(|participant| {
                buffers
                    .leases
                    .values()
                    .any(|leases| leases.contains(participant))
            })
            .collect()
    }

    pub(crate) async fn release_buffer(
        &self,
        actor: ParticipantId,
        buffer: BufferId,
    ) -> Result<(), ProjectError> {
        let _mutation = self.file_mutations.lock().await;
        let candidate = {
            let mut buffers = self.buffers.write().await;
            let Some(leases) = buffers.leases.get_mut(&buffer) else {
                return Ok(());
            };
            if !leases.remove(&actor) || !leases.is_empty() {
                return Ok(());
            }
            buffers.by_id.get(&buffer).cloned()
        };
        let clean =
            tokio::task::spawn_blocking(move || candidate.is_some_and(|open| !open.lock().dirty))
                .await
                .map_err(ProjectError::Worker)?;
        if clean {
            let mut buffers = self.buffers.write().await;
            if !buffers.leases.get(&buffer).is_some_and(HashSet::is_empty) {
                return Ok(());
            }
            buffers.by_id.remove(&buffer);
            buffers.leases.remove(&buffer);
            buffers.by_path.retain(|_, current| *current != buffer);
        }
        Ok(())
    }

    pub(crate) async fn release_participant(
        &self,
        actor: ParticipantId,
    ) -> Result<(), ProjectError> {
        let _mutation = self.file_mutations.lock().await;
        let candidates = {
            let mut buffers = self.buffers.write().await;
            for leases in buffers.leases.values_mut() {
                leases.remove(&actor);
            }
            buffers
                .leases
                .iter()
                .filter(|(_, leases)| leases.is_empty())
                .filter_map(|(&buffer, _)| {
                    buffers
                        .by_id
                        .get(&buffer)
                        .cloned()
                        .map(|open| (buffer, open))
                })
                .collect::<Vec<_>>()
        };
        let removable = tokio::task::spawn_blocking(move || {
            candidates
                .into_iter()
                .filter_map(|(buffer, open)| (!open.lock().dirty).then_some(buffer))
                .collect::<HashSet<_>>()
        })
        .await
        .map_err(ProjectError::Worker)?;
        let mut buffers = self.buffers.write().await;
        let removable = removable
            .into_iter()
            .filter(|buffer| buffers.leases.get(buffer).is_some_and(HashSet::is_empty))
            .collect::<HashSet<_>>();
        buffers
            .by_path
            .retain(|_, buffer| !removable.contains(buffer));
        buffers
            .by_id
            .retain(|buffer, _| !removable.contains(buffer));
        buffers
            .leases
            .retain(|buffer, _| !removable.contains(buffer));
        Ok(())
    }

    async fn open(&self, buffer: BufferId) -> Result<Arc<Mutex<OpenBuffer>>, ProjectError> {
        self.buffers
            .read()
            .await
            .by_id
            .get(&buffer)
            .cloned()
            .ok_or(ProjectError::BufferNotFound(buffer))
    }

    pub(crate) async fn buffer_path_for(
        &self,
        actor: ParticipantId,
        buffer: BufferId,
    ) -> Result<WorkspacePath, ProjectError> {
        let open = self.open_for(actor, buffer).await?;
        tokio::task::spawn_blocking(move || open.lock().path.clone())
            .await
            .map_err(ProjectError::Worker)
    }

    pub(crate) async fn buffer_epoch_for(
        &self,
        actor: ParticipantId,
        buffer: BufferId,
    ) -> Result<u64, ProjectError> {
        let open = self.open_for(actor, buffer).await?;
        tokio::task::spawn_blocking(move || open.lock().document.epoch())
            .await
            .map_err(ProjectError::Worker)
    }

    pub(crate) async fn buffer_path_and_text_for(
        &self,
        actor: ParticipantId,
        buffer: BufferId,
    ) -> Result<(WorkspacePath, String), ProjectError> {
        let open = self.open_for(actor, buffer).await?;
        tokio::task::spawn_blocking(move || {
            let open = open.lock();
            Ok((open.path.clone(), open.document.text().to_owned()))
        })
        .await
        .map_err(ProjectError::Worker)?
    }
}

struct OverlaySearchPage {
    entries: Vec<ContentSearchEntry>,
    next: Option<ContentSearchCursor>,
    overlay_count: usize,
}

fn search_content_overlays(
    pattern: String,
    smart_case: bool,
    overlays: Vec<(WorkspacePath, Arc<Mutex<OpenBuffer>>)>,
    cursor: ContentSearchCursor,
    limit: usize,
) -> Result<OverlaySearchPage, ProjectError> {
    let overlay_count = overlays.len();
    let parser = QueryParser::new(GrepConfig);
    let parsed = parser.parse(&pattern);
    let options = GrepSearchOptions {
        smart_case,
        mode: GrepMode::Regex,
        ..GrepSearchOptions::default()
    };
    let page = grep_byte_sources_page(
        &parsed,
        &options,
        overlay_count,
        ByteSourceGrepCursor {
            source: usize::from(cursor.overlay),
            match_offset: usize::from(cursor.overlay_match),
        },
        limit,
        std::time::Duration::from_millis(40),
        None,
        |index| {
            let (path, open) = &overlays[index];
            let mut open = open.lock();
            if !open.dirty || open.path != *path {
                return None;
            }
            Some(match &open.search_snapshot {
                Some((generation, snapshot)) if *generation == open.generation => snapshot.clone(),
                _ => {
                    let snapshot: Arc<[u8]> = Arc::from(open.document.text().as_bytes());
                    open.search_snapshot = Some((open.generation, snapshot.clone()));
                    snapshot
                }
            })
        },
    )
    .map_err(|error| ProjectError::InvalidContentSearch(error.to_string()))?;
    let entries = page
        .matches
        .into_iter()
        .map(|item| ContentSearchEntry {
            path: overlays[item.source].0.clone(),
            line: item.line_number.saturating_sub(1),
        })
        .collect();
    Ok(OverlaySearchPage {
        entries,
        next: page.next.map(|next| ContentSearchCursor {
            overlay: next.source as u16,
            overlay_match: next.match_offset as u16,
            file_offset: cursor.file_offset,
        }),
        overlay_count,
    })
}

fn buffer_sync_event(buffer: BufferId, epoch: u64, message: Vec<u8>) -> Event {
    if message.len() <= MAX_SYNC_MESSAGE_CHUNK_BYTES {
        Event::BufferSync {
            buffer,
            epoch,
            message: message.into(),
        }
    } else {
        Event::ResyncRequired { buffer, epoch }
    }
}

fn snapshot_compaction_deliveries(
    actor: ParticipantId,
    participants: Vec<ParticipantId>,
    buffer: BufferId,
    epoch: u64,
    compacted: bool,
) -> Vec<(ParticipantId, Event)> {
    if !compacted {
        return Vec::new();
    }
    participants
        .into_iter()
        .filter(|participant| *participant != actor)
        .map(|participant| (participant, Event::ResyncRequired { buffer, epoch }))
        .collect()
}

async fn snapshot_open(open: Arc<Mutex<OpenBuffer>>) -> Result<(u64, Vec<u8>, bool), ProjectError> {
    tokio::task::spawn_blocking(move || {
        let mut open = open.lock();
        let (snapshot, compacted) = open.document.snapshot_for_transfer()?;
        Ok((open.document.epoch(), snapshot, compacted))
    })
    .await
    .map_err(ProjectError::Worker)?
}

impl From<ProjectError> for ProtocolError {
    fn from(error: ProjectError) -> Self {
        use crate::ErrorCode;
        let code = match &error {
            ProjectError::BufferNotFound(_) => ErrorCode::NotFound,
            ProjectError::BufferNotLeased(_) => ErrorCode::Forbidden,
            ProjectError::Conflict(_) => ErrorCode::Conflict,
            ProjectError::FileTooLarge | ProjectError::ResourceExhausted(_) => {
                ErrorCode::ResourceExhausted
            }
            ProjectError::EpochMismatch => ErrorCode::ResyncRequired,
            ProjectError::InvalidName
            | ProjectError::BinaryFile
            | ProjectError::ManagementRequest
            | ProjectError::InvalidPresence
            | ProjectError::InvalidFilePageLimit(_)
            | ProjectError::InvalidContentSearch(_)
            | ProjectError::InvalidFileTransaction(_) => ErrorCode::InvalidRequest,
            _ => ErrorCode::Internal,
        };
        let message = if code == ErrorCode::Internal {
            log::error!("collaboration project operation failed: {error}");
            "collaboration project operation failed".to_owned()
        } else {
            error.to_string()
        };
        Self {
            code,
            message,
            retryable: matches!(code, ErrorCode::Conflict | ErrorCode::ResyncRequired),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("collaboration project name is invalid")]
    InvalidName,
    #[error("operating-system entropy is unavailable: {0}")]
    Entropy(String),
    #[error("collaboration buffer {0:?} was not found")]
    BufferNotFound(BufferId),
    #[error("collaboration buffer {0:?} is not open for this participant")]
    BufferNotLeased(BufferId),
    #[error("binary files cannot be collaboratively edited")]
    BinaryFile,
    #[error("file exceeds the collaborative editing size limit")]
    FileTooLarge,
    #[error("collaboration project resource exhausted: {0}")]
    ResourceExhausted(&'static str),
    #[error("collaboration file page limit {0} is invalid")]
    InvalidFilePageLimit(u16),
    #[error("collaboration content search is invalid: {0}")]
    InvalidContentSearch(String),
    #[error("collaboration file transaction is invalid: {0}")]
    InvalidFileTransaction(&'static str),
    #[error("collaboration file changed outside the shared session: {0}")]
    Conflict(String),
    #[error("collaboration buffer requires a fresh snapshot")]
    EpochMismatch,
    #[error("collaboration presence identity does not match its authenticated participant")]
    InvalidPresence,
    #[error("management request was routed to the project service")]
    ManagementRequest,
    #[error("collaboration buffer lock was poisoned")]
    Poisoned,
    #[error("collaboration worker failed: {0}")]
    Worker(#[source] tokio::task::JoinError),
    #[error(transparent)]
    Buffer(#[from] crate::BufferError),
    #[error("collaboration backend failed: {0}")]
    Backend(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalBackend;

    fn participant(value: u8) -> ParticipantId {
        ParticipantId([value; 16])
    }

    async fn content_search(
        project: &Project,
        _actor: ParticipantId,
        pattern: &str,
    ) -> Vec<ContentSearchEntry> {
        let mut cursor = ContentSearchCursor::default();
        let mut entries = Vec::new();
        for _ in 0..64 {
            let page = project
                .search_content(
                    ContentSearchQuery {
                        root: WorkspacePath::root(),
                        pattern: pattern.to_owned(),
                        smart_case: true,
                        options: ScanOptions::default(),
                        excluded_paths: Vec::new(),
                        cursor,
                        limit: helix_workspace::MAX_CONTENT_SEARCH_PAGE_RESULTS as u16,
                    },
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
            entries.extend(page.entries);
            if page.done {
                return entries;
            }
            let next = page.next.expect("unfinished content search must continue");
            cursor = next;
        }
        panic!("content search did not finish")
    }

    #[test]
    fn internal_backend_errors_do_not_expose_host_details() {
        let error = ProtocolError::from(ProjectError::Backend(
            r"C:\private\workspace\secret.txt: access denied".to_owned(),
        ));
        assert_eq!(error.code, crate::ErrorCode::Internal);
        assert_eq!(error.message, "collaboration project operation failed");
        assert!(!error.message.contains("secret.txt"));
    }

    #[test]
    fn oversized_host_syncs_fall_back_to_snapshot_resync() {
        assert!(matches!(
            buffer_sync_event(BufferId(4), 7, vec![0; MAX_SYNC_MESSAGE_CHUNK_BYTES + 1],),
            Event::ResyncRequired {
                buffer: BufferId(4),
                epoch: 7,
            }
        ));
    }

    #[test]
    fn snapshot_compaction_resyncs_every_peer_except_the_requester() {
        let actor = participant(1);
        let other = participant(2);
        let buffer = BufferId(4);
        assert!(
            snapshot_compaction_deliveries(actor, vec![actor, other], buffer, 3, false).is_empty()
        );
        assert_eq!(
            snapshot_compaction_deliveries(actor, vec![actor, other], buffer, 3, true),
            vec![(other, Event::ResyncRequired { buffer, epoch: 3 })]
        );
    }

    fn path(value: &str) -> WorkspacePath {
        WorkspacePath::from_slash_path(value).unwrap()
    }

    fn response_buffer(response: &Response) -> BufferId {
        let Response::Buffer { buffer, .. } = response else {
            panic!("expected buffer response");
        };
        *buffer
    }

    #[tokio::test]
    async fn buffer_leases_isolate_access_and_sync_delivery() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("shared.txt"), "abc").unwrap();
        let owner = participant(1);
        let guest = participant(2);
        let spectator = participant(3);
        let project = Project::new(
            "leases",
            owner,
            Arc::new(LocalBackend::open(root.path()).await.unwrap()),
        )
        .unwrap();
        let opened = project
            .handle(
                guest,
                Request::OpenBuffer {
                    path: path("shared.txt"),
                },
                vec![owner, guest, spectator],
                Vec::new(),
            )
            .await
            .unwrap();
        let buffer = response_buffer(&opened.response);

        assert!(matches!(
            project
                .handle(
                    spectator,
                    Request::ReadBuffer { buffer },
                    vec![owner, guest, spectator],
                    Vec::new(),
                )
                .await,
            Err(ProjectError::BufferNotLeased(current)) if current == buffer
        ));

        let info = project.info(Vec::new());
        let mut replica = crate::ReplicaProject::new(guest, &info);
        replica.install(opened.response).unwrap();
        let sync = replica.replace(buffer, 1..2, "x").unwrap().unwrap();
        let outcome = project
            .handle(guest, sync, vec![owner, guest, spectator], Vec::new())
            .await
            .unwrap();
        assert!(!outcome.deliveries.is_empty());
        assert!(outcome
            .deliveries
            .iter()
            .all(|(participant, _)| *participant == guest));
    }

    #[tokio::test]
    async fn final_close_evicts_clean_buffers_but_retains_dirty_state() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("shared.txt"), "abc").unwrap();
        let owner = participant(1);
        let guest = participant(2);
        let project = Project::new(
            "close",
            owner,
            Arc::new(LocalBackend::open(root.path()).await.unwrap()),
        )
        .unwrap();

        let clean = project
            .handle(
                guest,
                Request::OpenBuffer {
                    path: path("shared.txt"),
                },
                vec![guest],
                Vec::new(),
            )
            .await
            .unwrap();
        let clean_buffer = response_buffer(&clean.response);
        project
            .handle(
                guest,
                Request::CloseBuffer {
                    buffer: clean_buffer,
                },
                vec![guest],
                Vec::new(),
            )
            .await
            .unwrap();
        assert!(!project
            .buffers
            .read()
            .await
            .by_id
            .contains_key(&clean_buffer));

        let dirty = project
            .handle(
                guest,
                Request::OpenBuffer {
                    path: path("shared.txt"),
                },
                vec![guest],
                Vec::new(),
            )
            .await
            .unwrap();
        let dirty_buffer = response_buffer(&dirty.response);
        assert_ne!(dirty_buffer, clean_buffer);
        let mut replica = crate::ReplicaProject::new(guest, &project.info(Vec::new()));
        replica.install(dirty.response).unwrap();
        let mut sync = replica
            .replace(dirty_buffer, 0..3, "changed")
            .unwrap()
            .unwrap();
        for _ in 0..8 {
            let outcome = project
                .handle(guest, sync, vec![guest], Vec::new())
                .await
                .unwrap();
            let Some((_, event)) = outcome.deliveries.into_iter().next() else {
                break;
            };
            let Some(update) = replica.apply(event).unwrap() else {
                break;
            };
            let Some(reply) = update.reply else {
                break;
            };
            sync = reply;
        }
        assert!(
            project
                .buffers
                .read()
                .await
                .by_id
                .get(&dirty_buffer)
                .unwrap()
                .lock()
                .dirty
        );
        project
            .handle(
                guest,
                Request::CloseBuffer {
                    buffer: dirty_buffer,
                },
                vec![guest],
                Vec::new(),
            )
            .await
            .unwrap();
        assert!(project
            .buffers
            .read()
            .await
            .by_id
            .contains_key(&dirty_buffer));

        let reopened = project
            .handle(
                guest,
                Request::OpenBuffer {
                    path: path("shared.txt"),
                },
                vec![guest],
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(response_buffer(&reopened.response), dirty_buffer);
        let mut reopened_replica = crate::ReplicaProject::new(guest, &project.info(Vec::new()));
        reopened_replica.install(reopened.response).unwrap();
        assert_eq!(reopened_replica.text(dirty_buffer).unwrap(), "changed");
    }

    #[tokio::test]
    async fn file_transactions_are_per_participant_undoable_and_protect_open_buffers() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let owner = participant(1);
        let project = Project::new(
            "project",
            owner,
            Arc::new(LocalBackend::open(root.path()).await.unwrap()),
        )
        .unwrap();
        let peers = vec![owner];

        let create = FileTransaction {
            operations: vec![FileOperation::CreateFile {
                path: path("src/new.rs"),
                overwrite: false,
            }],
        };
        let outcome = project
            .handle(
                owner,
                Request::ApplyFileTransaction {
                    transaction: create.clone(),
                },
                peers.clone(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.response,
            Response::FileTransaction { changed: true }
        );
        assert!(root.path().join("src/new.rs").exists());
        assert!(matches!(
            outcome.deliveries.as_slice(),
            [(_, Event::FilesChanged { undone: false, .. })]
        ));

        let outcome = project
            .handle(
                owner,
                Request::ReplayFileTransaction { redo: false },
                peers.clone(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.response,
            Response::FileTransaction { changed: true }
        );
        assert!(!root.path().join("src/new.rs").exists());

        project
            .handle(
                owner,
                Request::OpenBuffer {
                    path: path("src/main.rs"),
                },
                peers.clone(),
                Vec::new(),
            )
            .await
            .unwrap();
        let remove = project
            .handle(
                owner,
                Request::ApplyFileTransaction {
                    transaction: FileTransaction {
                        operations: vec![FileOperation::Remove {
                            path: path("src/main.rs"),
                            recursive: false,
                        }],
                    },
                },
                peers,
                Vec::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(remove, ProjectError::Conflict(_)));
        assert!(root.path().join("src/main.rs").exists());
    }

    #[tokio::test]
    async fn project_state_reconciles_open_buffer_paths_at_a_file_revision() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let owner = participant(1);
        let project = Project::new(
            "project",
            owner,
            Arc::new(LocalBackend::open(root.path()).await.unwrap()),
        )
        .unwrap();
        let opened = project
            .handle(
                owner,
                Request::OpenBuffer {
                    path: path("src/main.rs"),
                },
                vec![owner],
                Vec::new(),
            )
            .await
            .unwrap();
        let Response::Buffer { buffer, .. } = opened.response else {
            panic!("expected buffer response");
        };

        project
            .handle(
                owner,
                Request::ApplyFileTransaction {
                    transaction: FileTransaction {
                        operations: vec![FileOperation::Rename {
                            from: path("src/main.rs"),
                            to: path("src/renamed.rs"),
                            overwrite: false,
                        }],
                    },
                },
                vec![owner],
                Vec::new(),
            )
            .await
            .unwrap();
        let state = project
            .handle(owner, Request::ProjectState, vec![owner], Vec::new())
            .await
            .unwrap();

        assert_eq!(
            state.response,
            Response::ProjectState(ProjectState {
                file_revision: 1,
                open_buffers: vec![OpenBufferInfo {
                    buffer,
                    path: path("src/renamed.rs"),
                }],
                participants: Vec::new(),
            })
        );
    }

    #[tokio::test]
    async fn file_pages_are_bounded_sorted_and_monotonic() {
        let root = tempfile::tempdir().unwrap();
        for index in (0..300).rev() {
            std::fs::write(root.path().join(format!("file-{index:03}.txt")), "").unwrap();
        }
        let owner = participant(2);
        let project = Project::new(
            "large-project",
            owner,
            Arc::new(LocalBackend::open(root.path()).await.unwrap()),
        )
        .unwrap();
        let mut after = None;
        let mut files = Vec::new();
        loop {
            let outcome = project
                .handle(
                    owner,
                    Request::ListFiles {
                        options: ScanOptions::default(),
                        after: after.clone(),
                        limit: crate::MAX_FILE_PAGE_ENTRIES,
                    },
                    vec![owner],
                    Vec::new(),
                )
                .await
                .unwrap();
            let Response::Files { entries, next } = outcome.response else {
                panic!("expected file page");
            };
            assert!(entries.len() <= crate::MAX_FILE_PAGE_ENTRIES as usize);
            assert!(entries.windows(2).all(|pair| pair[0] < pair[1]));
            if let (Some(after), Some(first)) = (after.as_ref(), entries.first()) {
                assert!(first > after);
            }
            files.extend(entries);
            let Some(next) = next else {
                break;
            };
            assert!(after.as_ref().is_none_or(|after| next > *after));
            after = Some(next);
        }
        assert_eq!(files.len(), 300);
        assert!(files.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[tokio::test]
    async fn content_search_uses_dirty_crdt_text_instead_of_stale_disk_text() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("shared.txt"), "stale_only\n").unwrap();
        let owner = participant(8);
        let project = Project::new(
            "search-overlays",
            owner,
            Arc::new(LocalBackend::open(root.path()).await.unwrap()),
        )
        .unwrap();
        let opened = project
            .handle(
                owner,
                Request::OpenBuffer {
                    path: path("shared.txt"),
                },
                vec![owner],
                Vec::new(),
            )
            .await
            .unwrap();
        let buffer = response_buffer(&opened.response);
        let mut replica = crate::ReplicaProject::new(owner, &project.info(Vec::new()));
        replica.install(opened.response).unwrap();
        let mut sync = replica
            .replace(buffer, 0..11, "fresh_only\nfresh_only\n")
            .unwrap()
            .unwrap();
        for _ in 0..8 {
            let outcome = project
                .handle(owner, sync, vec![owner], Vec::new())
                .await
                .unwrap();
            let Some((_, event)) = outcome.deliveries.into_iter().next() else {
                break;
            };
            let Some(update) = replica.apply(event).unwrap() else {
                break;
            };
            let Some(reply) = update.reply else {
                break;
            };
            sync = reply;
        }

        assert!(content_search(&project, owner, "stale_only")
            .await
            .is_empty());
        assert_eq!(
            content_search(&project, owner, "fresh_only").await,
            vec![
                ContentSearchEntry {
                    path: path("shared.txt"),
                    line: 0,
                },
                ContentSearchEntry {
                    path: path("shared.txt"),
                    line: 1,
                },
            ]
        );
    }

    #[tokio::test]
    async fn rename_paths_are_validated_before_backend_mutation() {
        let root = tempfile::tempdir().unwrap();
        let owner = participant(3);
        let project = Project::new(
            "path-validation",
            owner,
            Arc::new(LocalBackend::open(root.path()).await.unwrap()),
        )
        .unwrap();
        let segment = "s".repeat(helix_workspace::MAX_WORKSPACE_PATH_SEGMENT_BYTES);
        let source = WorkspacePath::new([
            "f".to_owned(),
            segment.clone(),
            segment.clone(),
            segment.clone(),
        ])
        .unwrap();
        let buffer = BufferId(1);
        let open = Arc::new(Mutex::new(OpenBuffer {
            path: source.clone(),
            document: Buffer::new(owner, "").unwrap(),
            version: FileVersion::new(ByteBuf::new()).unwrap(),
            generation: 0,
            dirty: false,
            search_snapshot: None,
        }));
        {
            let mut buffers = project.buffers.write().await;
            buffers.by_path.insert(source.clone(), buffer);
            buffers.by_id.insert(buffer, open);
        }
        let transaction = FileTransaction {
            operations: vec![FileOperation::Rename {
                from: path("f"),
                to: WorkspacePath::new([segment]).unwrap(),
                overwrite: false,
            }],
        };

        let error = project
            .validate_file_transaction(&transaction)
            .await
            .err()
            .expect("rename must be rejected during preflight");
        assert!(matches!(
            error,
            ProjectError::InvalidFileTransaction(
                "renamed buffer path exceeds workspace path limits"
            )
        ));
    }
}
