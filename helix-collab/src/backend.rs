use crate::{
    Backend, BackendFileUpdate, BackendFileWatch, BackendFuture, BackendTransactionId, FileData,
    FileVersion, ProjectError, MAX_COLLABORATIVE_FILE_BYTES, MAX_PROJECT_FILES,
    MAX_WORKTREE_CHANGES_PER_EVENT,
};
use helix_remote::{
    backend::{RemoteWatchUpdate, RemoteWorkspaceClient},
    ContentId, FileKind, WorkspacePath,
};
use helix_workspace::{
    atomic_replace, sync_parent_directory, ContentSearchPage, ContentSearchQuery, DirectoryOptions,
    FileChange, FileChangeKind, FileTransaction, FileTransactionId, FileTransactionStore,
    RootedWorkspace, ScanOptions, WorkspaceSearchIndex, WorkspaceSearchIndexError,
};
use ignore::WalkBuilder;
use notify::{EventKind, RecursiveMode, Watcher};
use serde_bytes::ByteBuf;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, Mutex},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

const MAX_REMOTE_LIST_REQUESTS: usize = 16;
const SEARCH_INDEX_CACHE_CAPACITY: usize = 4;

pub struct LocalBackend {
    workspace: RootedWorkspace,
    mutations: Mutex<()>,
    transactions: Arc<StdMutex<FileTransactionStore>>,
    next_transaction: AtomicU64,
    search_indexes: Mutex<VecDeque<Arc<WorkspaceSearchIndex>>>,
}

impl LocalBackend {
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, ProjectError> {
        let workspace = RootedWorkspace::open(root)
            .await
            .map_err(project_backend_error)?;
        let transactions = Arc::new(StdMutex::new(FileTransactionStore::new(workspace.root())));
        Ok(Self {
            workspace,
            mutations: Mutex::new(()),
            transactions,
            next_transaction: AtomicU64::new(1),
            search_indexes: Mutex::new(VecDeque::new()),
        })
    }

    pub fn root(&self) -> &Path {
        self.workspace.root()
    }

    async fn read(&self, path: WorkspacePath) -> Result<FileData, ProjectError> {
        let resolved = self
            .workspace
            .resolve_existing(&path)
            .await
            .map_err(project_backend_error)?;
        let file = tokio::fs::File::open(&resolved)
            .await
            .map_err(project_backend_error)?;
        let metadata = file.metadata().await.map_err(project_backend_error)?;
        if !metadata.is_file() {
            return Err(ProjectError::Backend("path is not a file".to_owned()));
        }
        if metadata.len() > MAX_COLLABORATIVE_FILE_BYTES as u64 {
            return Err(ProjectError::ResourceExhausted(
                "file is too large for collaboration",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_COLLABORATIVE_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(project_backend_error)?;
        if bytes.len() > MAX_COLLABORATIVE_FILE_BYTES {
            return Err(ProjectError::ResourceExhausted(
                "file grew beyond the collaboration limit while reading",
            ));
        }
        Ok(FileData {
            version: content_version(&bytes),
            bytes,
        })
    }

    async fn write(
        &self,
        path: WorkspacePath,
        expected: Option<FileVersion>,
        bytes: Vec<u8>,
    ) -> Result<FileVersion, ProjectError> {
        if bytes.len() > MAX_COLLABORATIVE_FILE_BYTES {
            return Err(ProjectError::ResourceExhausted(
                "file is too large for collaboration",
            ));
        }
        let _mutation = self.mutations.lock().await;
        let destination = self
            .workspace
            .resolve_for_write(&path, false)
            .await
            .map_err(project_backend_error)?;
        let current = tokio::fs::read(&destination)
            .await
            .map_err(project_backend_error)?;
        if let Some(expected) = expected {
            if content_version(&current) != expected {
                return Err(ProjectError::Conflict(
                    "file changed outside the collaboration session".to_owned(),
                ));
            }
        }
        let permissions = tokio::fs::metadata(&destination)
            .await
            .map_err(project_backend_error)?
            .permissions();
        let temp = create_temp_path(&destination).await?;
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&temp)
                .await
                .map_err(project_backend_error)?;
            file.write_all(&bytes)
                .await
                .map_err(project_backend_error)?;
            file.flush().await.map_err(project_backend_error)?;
            file.sync_all().await.map_err(project_backend_error)?;
            tokio::fs::set_permissions(&temp, permissions)
                .await
                .map_err(project_backend_error)?;
            atomic_replace(temp.clone(), destination.clone())
                .await
                .map_err(project_backend_error)?;
            sync_parent_directory(&destination).await;
            Ok(content_version(&bytes))
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(temp).await;
        }
        result
    }

    async fn search_index(
        &self,
        root: &WorkspacePath,
        options: ScanOptions,
    ) -> Result<Arc<WorkspaceSearchIndex>, ProjectError> {
        let index_root = self
            .workspace
            .resolve_existing(root)
            .await
            .map_err(project_backend_error)?;
        if !tokio::fs::metadata(&index_root)
            .await
            .map_err(project_backend_error)?
            .is_dir()
        {
            return Err(ProjectError::InvalidContentSearch(
                "search root is not a directory".to_owned(),
            ));
        }
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

        let workspace_root = self.workspace.root().to_path_buf();
        let cache_root = index_root.clone();
        let created = tokio::task::spawn_blocking(move || {
            WorkspaceSearchIndex::new(workspace_root, index_root, options)
        })
        .await
        .map_err(ProjectError::Worker)?
        .map_err(project_search_error)?;
        let created = Arc::new(created);
        let mut indexes = self.search_indexes.lock().await;
        if let Some(existing) = indexes
            .iter()
            .find(|index| index.matches(&cache_root, options))
            .cloned()
        {
            return Ok(existing);
        }
        indexes.push_back(created.clone());
        while indexes.len() > SEARCH_INDEX_CACHE_CAPACITY {
            indexes.pop_front();
        }
        Ok(created)
    }
}

impl Backend for LocalBackend {
    fn watch_files(&self) -> BackendFuture<'_, Option<BackendFileWatch>> {
        let root = self.workspace.root().to_path_buf();
        Box::pin(async move {
            let (updates, receiver) = mpsc::channel(256);
            let watch_root = root.clone();
            let mut watcher =
                notify::recommended_watcher(move |result: Result<notify::Event, notify::Error>| {
                    let update = match result {
                        Ok(event) => match local_file_update(&watch_root, event) {
                            Some(update) => update,
                            None => return,
                        },
                        Err(error) => {
                            log::warn!("local collaboration worktree watch failed: {error}");
                            BackendFileUpdate::Rescan
                        }
                    };
                    let _ = updates.blocking_send(update);
                })
                .map_err(project_backend_error)?;
            watcher
                .watch(&root, RecursiveMode::Recursive)
                .map_err(project_backend_error)?;
            Ok(Some(BackendFileWatch::new(receiver, watcher)))
        })
    }

    fn list_files(&self, options: ScanOptions) -> BackendFuture<'_, Vec<WorkspacePath>> {
        let workspace = self.workspace.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || list_local_files(&workspace, options))
                .await
                .map_err(ProjectError::Worker)?
        })
    }

    fn read_file(&self, path: WorkspacePath) -> BackendFuture<'_, FileData> {
        Box::pin(self.read(path))
    }

    fn search_content(
        &self,
        query: ContentSearchQuery,
        canceled: CancellationToken,
    ) -> BackendFuture<'_, ContentSearchPage> {
        Box::pin(async move {
            let index = self.search_index(&query.root, query.options).await?;
            let abort = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_abort = abort.clone();
            let worker = tokio::task::spawn_blocking(move || {
                index.content_page(&query, std::time::Duration::from_millis(40), worker_abort)
            });
            tokio::select! {
                _ = canceled.cancelled() => {
                    abort.store(true, Ordering::Release);
                    Err(ProjectError::Conflict("content search was canceled".to_owned()))
                }
                result = worker => result
                    .map_err(ProjectError::Worker)?
                    .map_err(project_search_error),
            }
        })
    }

    fn path_exists(&self, path: WorkspacePath) -> BackendFuture<'_, bool> {
        Box::pin(async move {
            match self.workspace.resolve_existing(&path).await {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == helix_workspace::WorkspaceFsErrorKind::NotFound => {
                    Ok(false)
                }
                Err(error) => Err(project_backend_error(error)),
            }
        })
    }

    fn write_file(
        &self,
        path: WorkspacePath,
        expected: Option<FileVersion>,
        bytes: Vec<u8>,
    ) -> BackendFuture<'_, FileVersion> {
        Box::pin(self.write(path, expected, bytes))
    }

    fn apply_file_transaction(
        &self,
        transaction: FileTransaction,
    ) -> BackendFuture<'_, BackendTransactionId> {
        let id = self.next_transaction.fetch_add(1, Ordering::Relaxed);
        let transactions = self.transactions.clone();
        Box::pin(async move {
            let _mutation = self.mutations.lock().await;
            tokio::task::spawn_blocking(move || {
                transactions
                    .lock()
                    .map_err(|_| ProjectError::Poisoned)?
                    .apply(FileTransactionId(id), transaction)
                    .map(|_| BackendTransactionId(id))
                    .map_err(project_transaction_error)
            })
            .await
            .map_err(ProjectError::Worker)?
        })
    }

    fn undo_file_transaction(&self, transaction: BackendTransactionId) -> BackendFuture<'_, ()> {
        let transactions = self.transactions.clone();
        Box::pin(async move {
            let _mutation = self.mutations.lock().await;
            tokio::task::spawn_blocking(move || {
                transactions
                    .lock()
                    .map_err(|_| ProjectError::Poisoned)?
                    .undo(FileTransactionId(transaction.0))
                    .map_err(project_transaction_error)
            })
            .await
            .map_err(ProjectError::Worker)?
        })
    }
}

#[derive(Clone)]
pub struct RemoteBackend {
    client: Arc<RemoteWorkspaceClient>,
}

impl RemoteBackend {
    pub fn new(client: Arc<RemoteWorkspaceClient>) -> Self {
        Self { client }
    }

    pub fn decode_file_version(version: &FileVersion) -> Result<ContentId, ProjectError> {
        decode_remote_version(version)
    }
}

impl Backend for RemoteBackend {
    fn watch_files(&self) -> BackendFuture<'_, Option<BackendFileWatch>> {
        let client = self.client.clone();
        Box::pin(async move {
            let mut remote = client
                .watch_files(WorkspacePath::root(), true)
                .await
                .map_err(project_backend_error)?;
            let (updates, receiver) = mpsc::channel(256);
            let task = tokio::spawn(async move {
                while let Some(update) = remote.next().await {
                    let update = match update {
                        RemoteWatchUpdate::Changes(mut changes) => {
                            if changes.len() > MAX_WORKTREE_CHANGES_PER_EVENT {
                                BackendFileUpdate::Rescan
                            } else {
                                changes.retain(|change| !is_private_workspace_path(&change.path));
                                if changes.is_empty() {
                                    continue;
                                }
                                BackendFileUpdate::Changes(changes)
                            }
                        }
                        RemoteWatchUpdate::Rescan => BackendFileUpdate::Rescan,
                    };
                    if updates.send(update).await.is_err() {
                        break;
                    }
                }
            });
            Ok(Some(BackendFileWatch::new(receiver, AbortTask(task))))
        })
    }

    fn list_files(&self, options: ScanOptions) -> BackendFuture<'_, Vec<WorkspacePath>> {
        let client = self.client.clone();
        Box::pin(async move { list_remote_files(client, options).await })
    }

    fn read_file(&self, path: WorkspacePath) -> BackendFuture<'_, FileData> {
        let client = self.client.clone();
        Box::pin(async move {
            let file = client
                .read_file(path, CancellationToken::new())
                .await
                .map_err(project_backend_error)?;
            if file.bytes.len() > MAX_COLLABORATIVE_FILE_BYTES {
                return Err(ProjectError::ResourceExhausted(
                    "file is too large for collaboration",
                ));
            }
            let content = file.metadata.content.ok_or_else(|| {
                ProjectError::Backend("remote file has no content generation".to_owned())
            })?;
            Ok(FileData {
                bytes: file.bytes,
                version: encode_remote_version(content),
            })
        })
    }

    fn search_content(
        &self,
        query: ContentSearchQuery,
        canceled: CancellationToken,
    ) -> BackendFuture<'_, ContentSearchPage> {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .search_content_page(query, canceled)
                .await
                .map_err(project_backend_error)
        })
    }

    fn path_exists(&self, path: WorkspacePath) -> BackendFuture<'_, bool> {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .stat(path, CancellationToken::new())
                .await
                .map(|metadata| metadata.is_some())
                .map_err(project_backend_error)
        })
    }

    fn write_file(
        &self,
        path: WorkspacePath,
        expected: Option<FileVersion>,
        bytes: Vec<u8>,
    ) -> BackendFuture<'_, FileVersion> {
        let client = self.client.clone();
        Box::pin(async move {
            if bytes.len() > MAX_COLLABORATIVE_FILE_BYTES {
                return Err(ProjectError::ResourceExhausted(
                    "file is too large for collaboration",
                ));
            }
            let expected = expected.as_ref().map(decode_remote_version).transpose()?;
            let metadata = client
                .write_file(path, &bytes, expected, CancellationToken::new())
                .await
                .map_err(project_backend_error)?;
            metadata.content.map(encode_remote_version).ok_or_else(|| {
                ProjectError::Backend("remote save has no content generation".to_owned())
            })
        })
    }

    fn apply_file_transaction(
        &self,
        transaction: FileTransaction,
    ) -> BackendFuture<'_, BackendTransactionId> {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .apply_file_transaction_untracked(transaction)
                .await
                .map(|receipt| BackendTransactionId(receipt.transaction.0))
                .map_err(project_backend_error)
        })
    }

    fn undo_file_transaction(&self, transaction: BackendTransactionId) -> BackendFuture<'_, ()> {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .undo_file_transaction_exact(helix_remote::TransactionId(transaction.0))
                .await
                .map_err(project_backend_error)
        })
    }
}

fn list_local_files(
    workspace: &RootedWorkspace,
    options: ScanOptions,
) -> Result<Vec<WorkspacePath>, ProjectError> {
    let filter_root = workspace.root().to_path_buf();
    let mut builder = WalkBuilder::new(workspace.root());
    builder
        .hidden(options.hidden)
        .parents(options.parents)
        .ignore(options.ignore)
        .git_ignore(options.git_ignore)
        .git_global(options.git_global)
        .git_exclude(options.git_exclude)
        .follow_links(options.follow_symlinks)
        .max_depth(options.max_depth.map(|depth| depth as usize))
        .filter_entry(move |entry| {
            entry.depth() == 0
                || (!is_private_directory(entry.file_name())
                    && (!entry.path_is_symlink()
                        || is_safe_project_symlink(
                            entry.path(),
                            &filter_root,
                            options.deduplicate_symlinks,
                        )))
        });
    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry.map_err(project_backend_error)?;
        if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = workspace
            .relative_path(entry.path())
            .map_err(project_backend_error)?;
        if helix_workspace::is_internal_path(&path) {
            continue;
        }
        files.push(path);
        if files.len() > MAX_PROJECT_FILES {
            return Err(ProjectError::ResourceExhausted(
                "project contains too many files",
            ));
        }
    }
    files.sort_unstable();
    Ok(files)
}

fn is_safe_project_symlink(path: &Path, workspace_root: &Path, deduplicate: bool) -> bool {
    !deduplicate
        && std::fs::canonicalize(path).is_ok_and(|target| target.starts_with(workspace_root))
}

fn is_private_directory(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | ".pijul" | ".jj" | ".hg" | ".svn" | ".double-helix")
    )
}

async fn list_remote_files(
    client: Arc<RemoteWorkspaceClient>,
    options: ScanOptions,
) -> Result<Vec<WorkspacePath>, ProjectError> {
    let mut pending = VecDeque::from([(WorkspacePath::root(), 0_u32)]);
    let mut requests = JoinSet::new();
    let mut files = Vec::new();
    loop {
        while requests.len() < MAX_REMOTE_LIST_REQUESTS {
            let Some((path, depth)) = pending.pop_front() else {
                break;
            };
            let client = client.clone();
            requests.spawn(async move {
                client
                    .read_dir(
                        path,
                        DirectoryOptions {
                            scan: ScanOptions {
                                max_depth: Some(1),
                                ..options
                            },
                            flatten_dirs: false,
                        },
                        CancellationToken::new(),
                    )
                    .await
                    .map(|entries| (depth, entries))
            });
        }
        let Some(result) = requests.join_next().await else {
            break;
        };
        let (depth, entries) = result
            .map_err(ProjectError::Worker)?
            .map_err(project_backend_error)?;
        for entry in entries {
            if is_private_workspace_path(&entry.path) {
                continue;
            }
            match entry.metadata.kind {
                FileKind::File => files.push(entry.path),
                FileKind::Directory
                    if options
                        .max_depth
                        .is_none_or(|max_depth| depth.saturating_add(1) < max_depth) =>
                {
                    pending.push_back((entry.path, depth.saturating_add(1)))
                }
                FileKind::Directory => {}
                FileKind::Symlink | FileKind::Other => {}
            }
        }
        if files.len() + pending.len() > MAX_PROJECT_FILES {
            return Err(ProjectError::ResourceExhausted(
                "project contains too many files",
            ));
        }
    }
    files.sort_unstable();
    Ok(files)
}

fn is_private_workspace_path(path: &WorkspacePath) -> bool {
    helix_workspace::is_internal_path(path)
        || path.segments().first().is_some_and(|segment| {
            matches!(segment.as_str(), ".git" | ".pijul" | ".jj" | ".hg" | ".svn")
        })
}

fn local_file_update(root: &Path, event: notify::Event) -> Option<BackendFileUpdate> {
    let kind = local_change_kind(&event.kind);
    let mut changes = Vec::new();
    for path in event.paths {
        let Ok(path) = helix_workspace::relative_workspace_path(root, &path) else {
            continue;
        };
        // Recursive macOS watches can report only the watched root. That proves
        // something changed, but not which descendant changed.
        if path.is_root() {
            return Some(BackendFileUpdate::Rescan);
        }
        if is_private_workspace_path(&path) {
            continue;
        }
        changes.push(FileChange { path, kind });
        if changes.len() > MAX_WORKTREE_CHANGES_PER_EVENT {
            return Some(BackendFileUpdate::Rescan);
        }
    }
    if changes.is_empty() {
        return None;
    }
    changes.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    changes.dedup();
    Some(BackendFileUpdate::Changes(changes))
}

fn local_change_kind(kind: &EventKind) -> FileChangeKind {
    match kind {
        EventKind::Create(_) => FileChangeKind::Created,
        EventKind::Modify(notify::event::ModifyKind::Name(_)) => FileChangeKind::Renamed,
        EventKind::Modify(_) => FileChangeKind::Modified,
        EventKind::Remove(_) => FileChangeKind::Removed,
        EventKind::Access(_) | EventKind::Other | EventKind::Any => FileChangeKind::Other,
    }
}

struct AbortTask(tokio::task::JoinHandle<()>);

impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn create_temp_path(destination: &Path) -> Result<PathBuf, ProjectError> {
    let parent = destination
        .parent()
        .ok_or_else(|| ProjectError::Backend("file has no parent directory".to_owned()))?;
    for _ in 0..8 {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|error| ProjectError::Entropy(error.to_string()))?;
        let name = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = parent.join(format!(".dhx-collab-write-{name}"));
        match tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await
        {
            Ok(file) => {
                drop(file);
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(project_backend_error(error)),
        }
    }
    Err(ProjectError::ResourceExhausted(
        "could not reserve a temporary save path",
    ))
}

fn content_version(bytes: &[u8]) -> FileVersion {
    FileVersion::new(ByteBuf::from(Sha256::digest(bytes).to_vec()))
        .expect("SHA-256 file versions fit the protocol limit")
}

fn encode_remote_version(content: ContentId) -> FileVersion {
    let mut bytes = Vec::with_capacity(17);
    bytes.extend_from_slice(&content.len.to_le_bytes());
    bytes.push(content.modified_unix_nanos.is_some().into());
    bytes.extend_from_slice(
        &content
            .modified_unix_nanos
            .unwrap_or_default()
            .to_le_bytes(),
    );
    FileVersion::new(ByteBuf::from(bytes)).expect("remote file versions fit the protocol limit")
}

fn decode_remote_version(version: &FileVersion) -> Result<ContentId, ProjectError> {
    let bytes = version.as_bytes();
    let len = bytes
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes);
    let marker = bytes.get(8).copied();
    let modified = bytes
        .get(9..17)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes);
    match (len, marker, modified, bytes.len()) {
        (Some(len), Some(0), Some(_), 17) => Ok(ContentId {
            len,
            modified_unix_nanos: None,
        }),
        (Some(len), Some(1), Some(modified), 17) => Ok(ContentId {
            len,
            modified_unix_nanos: Some(modified),
        }),
        _ => Err(ProjectError::Conflict(
            "remote file generation is invalid".to_owned(),
        )),
    }
}

fn project_backend_error(error: impl std::fmt::Display) -> ProjectError {
    ProjectError::Backend(error.to_string())
}

fn project_transaction_error(error: helix_workspace::FileTransactionError) -> ProjectError {
    use helix_workspace::FileTransactionErrorKind as Kind;
    match error.kind() {
        Kind::AlreadyExists | Kind::NotFound => ProjectError::Conflict(error.to_string()),
        Kind::InvalidPath | Kind::InvalidRequest | Kind::WorkspaceOutsideRoot => {
            ProjectError::InvalidFileTransaction("operation contains an invalid workspace path")
        }
        Kind::ResourceExhausted => {
            ProjectError::ResourceExhausted("file transaction history is full")
        }
        Kind::PermissionDenied | Kind::Io => ProjectError::Backend(error.to_string()),
    }
}

fn project_search_error(error: WorkspaceSearchIndexError) -> ProjectError {
    match error {
        WorkspaceSearchIndexError::InvalidQuery(_) | WorkspaceSearchIndexError::InvalidRegex(_) => {
            ProjectError::InvalidContentSearch(error.to_string())
        }
        error => ProjectError::Backend(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_root_notifications_require_a_rescan() {
        let root = Path::new("workspace");
        let event = notify::Event::new(EventKind::Create(notify::event::CreateKind::Folder))
            .add_path(root.to_path_buf());

        assert_eq!(
            local_file_update(root, event),
            Some(BackendFileUpdate::Rescan)
        );
    }

    #[tokio::test]
    async fn local_backend_streams_external_worktree_changes() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::open(root.path()).await.unwrap();
        let mut watch = backend.watch_files().await.unwrap().unwrap();
        std::fs::write(root.path().join("external.txt"), "changed\n").unwrap();

        let update = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let update = watch.recv().await.expect("worktree watch remains active");
                if let BackendFileUpdate::Changes(changes) = &update {
                    if changes
                        .iter()
                        .any(|change| change.path.to_string() == "external.txt")
                    {
                        break update;
                    }
                }
            }
        })
        .await
        .expect("native worktree notification");
        assert!(matches!(update, BackendFileUpdate::Changes(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_backend_follows_only_workspace_contained_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("inside")).unwrap();
        std::fs::write(root.path().join("inside/file.txt"), "inside").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        symlink(root.path().join("inside"), root.path().join("inside-link")).unwrap();
        symlink(outside.path(), root.path().join("outside-link")).unwrap();

        let backend = LocalBackend::open(root.path()).await.unwrap();
        let files = backend
            .list_files(ScanOptions {
                follow_symlinks: true,
                deduplicate_symlinks: false,
                ..ScanOptions::default()
            })
            .await
            .unwrap();
        let names = files.iter().map(ToString::to_string).collect::<Vec<_>>();

        assert!(names.contains(&"inside/file.txt".to_owned()));
        assert!(names.contains(&"inside-link/file.txt".to_owned()));
        assert!(!names.iter().any(|path| path.starts_with("outside-link/")));
    }

    #[tokio::test]
    async fn local_backend_filters_metadata_and_rejects_stale_saves() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::create_dir_all(root.path().join(".double-helix/state")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored.tmp\n").unwrap();
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.path().join(".github/workflows/ci.yml"), "name: ci\n").unwrap();
        std::fs::write(root.path().join("ignored.tmp"), "ignored\n").unwrap();
        std::fs::write(root.path().join(".git/config"), "private\n").unwrap();
        std::fs::write(root.path().join(".double-helix/state/data"), "private\n").unwrap();

        let backend = LocalBackend::open(root.path()).await.unwrap();
        let files = backend
            .list_files(ScanOptions {
                hidden: false,
                ..ScanOptions::default()
            })
            .await
            .unwrap();
        let names = files.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(names.contains(&"src/main.rs".to_owned()));
        assert!(names.contains(&".github/workflows/ci.yml".to_owned()));
        assert!(!names.iter().any(|path| path.starts_with(".git/")));
        assert!(!names.iter().any(|path| path.starts_with(".double-helix/")));
        assert!(!names.contains(&"ignored.tmp".to_owned()));

        let path = WorkspacePath::from_slash_path("src/main.rs").unwrap();
        let opened = backend.read_file(path.clone()).await.unwrap();
        let saved = backend
            .write_file(
                path.clone(),
                Some(opened.version.clone()),
                b"fn main() { println!(\"ok\"); }\n".to_vec(),
            )
            .await
            .unwrap();
        assert_ne!(saved, opened.version);

        std::fs::write(root.path().join("src/main.rs"), "external\n").unwrap();
        let stale = backend
            .write_file(path.clone(), Some(saved), b"lost edit\n".to_vec())
            .await
            .unwrap_err();
        assert!(matches!(stale, ProjectError::Conflict(_)));
        assert_eq!(
            std::fs::read_to_string(root.path().join("src/main.rs")).unwrap(),
            "external\n"
        );

        backend
            .write_file(path, None, b"intentional overwrite\n".to_vec())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("src/main.rs")).unwrap(),
            "intentional overwrite\n"
        );

        let created = WorkspacePath::from_slash_path("src/created.rs").unwrap();
        let transaction = backend
            .apply_file_transaction(FileTransaction {
                operations: vec![helix_workspace::FileOperation::CreateFile {
                    path: created,
                    overwrite: false,
                }],
            })
            .await
            .unwrap();
        assert!(root.path().join("src/created.rs").exists());
        backend.undo_file_transaction(transaction).await.unwrap();
        assert!(!root.path().join("src/created.rs").exists());
    }
}
