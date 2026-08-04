use crate::WorkspacePath;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_TRANSACTION_OPERATIONS: usize = 256;
pub const MAX_TRANSACTION_HISTORY: usize = 256;
const JOURNAL_VERSION: u16 = 1;
const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
static NEXT_JOURNAL_TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileTransactionId(pub u64);

type TransactionId = FileTransactionId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTransaction {
    pub operations: Vec<FileOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileOperation {
    CreateFile {
        path: WorkspacePath,
        overwrite: bool,
    },
    CreateDirectory {
        path: WorkspacePath,
    },
    Copy {
        from: WorkspacePath,
        to: WorkspacePath,
        overwrite: bool,
    },
    Rename {
        from: WorkspacePath,
        to: WorkspacePath,
        overwrite: bool,
    },
    Remove {
        path: WorkspacePath,
        recursive: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTransactionReceipt {
    pub transaction: FileTransactionId,
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: WorkspacePath,
    pub kind: FileChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    Created,
    Modified,
    Removed,
    Renamed,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTransactionErrorKind {
    NotFound,
    AlreadyExists,
    PermissionDenied,
    WorkspaceOutsideRoot,
    InvalidPath,
    InvalidRequest,
    ResourceExhausted,
    Io,
}

type ErrorCode = FileTransactionErrorKind;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FileTransactionError {
    kind: FileTransactionErrorKind,
    path: Option<WorkspacePath>,
    message: String,
    source: Option<std::io::Error>,
}

type RemoteError = FileTransactionError;

impl FileTransactionError {
    pub fn kind(&self) -> FileTransactionErrorKind {
        self.kind
    }

    pub fn path(&self) -> Option<&WorkspacePath> {
        self.path.as_ref()
    }

    pub fn is_retryable(&self) -> bool {
        self.source.as_ref().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            )
        })
    }

    fn new(kind: FileTransactionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            path: None,
            message: message.into(),
            source: None,
        }
    }

    fn at(mut self, path: WorkspacePath) -> Self {
        self.path = Some(path);
        self
    }
}

pub struct FileTransactionStore {
    root: PathBuf,
    storage_root: PathBuf,
    journal: Option<PathBuf>,
    journal_lock: Option<JournalLock>,
    records: HashMap<TransactionId, TransactionRecord>,
}

struct JournalLock {
    _file: fs::File,
}

#[derive(Serialize, Deserialize)]
struct TransactionRecord {
    undo: Vec<UndoAction>,
    backup_root: PathBuf,
    #[serde(default)]
    undoing: bool,
}

#[derive(Clone, Serialize, Deserialize)]
enum UndoAction {
    RemoveCreated {
        path: PathBuf,
    },
    RestoreBackup {
        path: PathBuf,
        backup: PathBuf,
    },
    RenameBack {
        source: PathBuf,
        destination: PathBuf,
        overwritten: Option<PathBuf>,
        created_parent: Option<PathBuf>,
    },
}

impl FileTransactionStore {
    pub fn new(root: &Path) -> Self {
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Self {
            storage_root: root.join(".double-helix").join("transactions"),
            root,
            journal: None,
            journal_lock: None,
            records: HashMap::new(),
        }
    }

    pub fn open_persistent(root: &Path, session: u64) -> Result<Self, FileTransactionError> {
        if session == 0 {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "transaction session must be nonzero",
            ));
        }
        let root = fs::canonicalize(root).map_err(|error| io_error(error, None))?;
        let storage_root = root
            .join(".double-helix")
            .join("transactions")
            .join("sessions")
            .join(format!("{session:016x}"));
        ensure_secure_directory(&root, &storage_root)?;
        let journal_lock = acquire_journal_lock(&storage_root)?;
        let journal = storage_root.join("journal.rmp");
        let records = load_journal(&journal)?;
        let mut store = Self {
            root,
            storage_root,
            journal: Some(journal),
            journal_lock: Some(journal_lock),
            records,
        };
        store.validate_records()?;
        let interrupted = store
            .records
            .iter()
            .filter_map(|(id, record)| record.undoing.then_some(*id))
            .collect::<Vec<_>>();
        for id in interrupted {
            store.finish_undo(id)?;
        }
        Ok(store)
    }

    pub fn next_id(&self) -> Result<FileTransactionId, FileTransactionError> {
        let next = self
            .records
            .keys()
            .map(|id| id.0)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                RemoteError::new(
                    ErrorCode::ResourceExhausted,
                    "file transaction identifier space is exhausted",
                )
            })?;
        Ok(FileTransactionId(next))
    }

    pub fn apply(
        &mut self,
        id: TransactionId,
        transaction: FileTransaction,
    ) -> Result<FileTransactionReceipt, RemoteError> {
        if transaction.operations.is_empty() {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "file transaction must contain at least one operation",
            ));
        }
        if transaction.operations.len() > MAX_TRANSACTION_OPERATIONS {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                format!("file transaction exceeds {MAX_TRANSACTION_OPERATIONS} operations"),
            ));
        }
        if self.records.len() >= MAX_TRANSACTION_HISTORY {
            return Err(RemoteError::new(
                ErrorCode::ResourceExhausted,
                "remote transaction history is full; undo or close the workspace before continuing",
            ));
        }
        if self.records.contains_key(&id) {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "file transaction identifier is already in use",
            ));
        }
        ensure_secure_directory(&self.root, &self.storage_root)?;
        let backup_root = self.storage_root.join(id.0.to_string());
        fs::create_dir_all(&backup_root).map_err(|error| io_error(error, None))?;

        let mut record = TransactionRecord {
            undo: Vec::with_capacity(transaction.operations.len()),
            backup_root,
            undoing: false,
        };
        let mut changes = Vec::with_capacity(transaction.operations.len() * 2);
        for (index, operation) in transaction.operations.into_iter().enumerate() {
            if let Err(error) = self.apply_operation(&mut record, index, operation, &mut changes) {
                let _ = rollback(&mut record);
                let _ = remove_if_exists(&record.backup_root);
                return Err(error);
            }
        }
        self.records.insert(id, record);
        if let Err(error) = self.persist() {
            if let Some(mut record) = self.records.remove(&id) {
                let _ = rollback(&mut record);
                let _ = remove_if_exists(&record.backup_root);
            }
            return Err(error);
        }
        Ok(FileTransactionReceipt {
            transaction: id,
            changes,
        })
    }

    pub fn undo(&mut self, id: TransactionId) -> Result<(), RemoteError> {
        let record = self.records.get_mut(&id).ok_or_else(|| {
            RemoteError::new(
                ErrorCode::NotFound,
                "remote file transaction is not undoable",
            )
        })?;
        record.undoing = true;
        self.persist()?;
        self.finish_undo(id)
    }

    pub fn clear(&mut self) {
        for (_, record) in self.records.drain() {
            let _ = remove_if_exists(&record.backup_root);
        }
        if self.journal.is_some() {
            self.journal_lock.take();
            let _ = remove_if_exists(&self.storage_root);
        }
    }

    fn finish_undo(&mut self, id: TransactionId) -> Result<(), RemoteError> {
        loop {
            let action = self
                .records
                .get(&id)
                .and_then(|record| record.undo.last())
                .cloned();
            let Some(action) = action else {
                break;
            };
            self.validate_undo_action(&id, &action)?;
            apply_undo(&action).map_err(|error| io_error(error, None))?;
            self.records
                .get_mut(&id)
                .expect("transaction disappeared during undo")
                .undo
                .pop();
            self.persist()?;
        }
        let record = self.records.remove(&id).expect("transaction disappeared");
        remove_if_exists(&record.backup_root).map_err(|error| io_error(error, None))?;
        self.persist()
    }

    fn persist(&self) -> Result<(), RemoteError> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        ensure_secure_directory(&self.root, &self.storage_root)?;
        let bytes = rmp_serde::to_vec_named(&(JOURNAL_VERSION, &self.records)).map_err(|_| {
            RemoteError::new(ErrorCode::Io, "failed to encode file transaction journal")
        })?;
        if bytes.len() as u64 > MAX_JOURNAL_BYTES {
            return Err(RemoteError::new(
                ErrorCode::ResourceExhausted,
                "file transaction journal exceeds its size limit",
            ));
        }
        let nonce = NEXT_JOURNAL_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = self
            .storage_root
            .join(format!(".journal-{}-{nonce}.tmp", std::process::id()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            crate::fs::atomic_replace_blocking(&temp, journal)
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temp);
            return Err(io_error(error, None));
        }
        Ok(())
    }

    fn validate_records(&self) -> Result<(), RemoteError> {
        if self.records.len() > MAX_TRANSACTION_HISTORY {
            return Err(RemoteError::new(
                ErrorCode::ResourceExhausted,
                "file transaction journal exceeds its history limit",
            ));
        }
        for (id, record) in &self.records {
            if record.backup_root != self.storage_root.join(id.0.to_string())
                || record.undo.len() > MAX_TRANSACTION_OPERATIONS
            {
                return Err(invalid_journal());
            }
            for action in &record.undo {
                self.validate_undo_action(id, action)?;
            }
        }
        Ok(())
    }

    fn validate_undo_action(
        &self,
        id: &TransactionId,
        action: &UndoAction,
    ) -> Result<(), RemoteError> {
        let backup_root = self.storage_root.join(id.0.to_string());
        match action {
            UndoAction::RemoveCreated { path } => validate_workspace_path(&self.root, path),
            UndoAction::RestoreBackup { path, backup } => {
                validate_workspace_path(&self.root, path)?;
                validate_backup_path(&backup_root, backup)
            }
            UndoAction::RenameBack {
                source,
                destination,
                overwritten,
                created_parent,
            } => {
                validate_workspace_path(&self.root, source)?;
                validate_workspace_path(&self.root, destination)?;
                if let Some(overwritten) = overwritten {
                    validate_backup_path(&backup_root, overwritten)?;
                }
                if let Some(created_parent) = created_parent {
                    validate_workspace_path(&self.root, created_parent)?;
                }
                Ok(())
            }
        }
    }

    fn apply_operation(
        &self,
        record: &mut TransactionRecord,
        index: usize,
        operation: FileOperation,
        changes: &mut Vec<FileChange>,
    ) -> Result<(), RemoteError> {
        match operation {
            FileOperation::CreateFile { path, overwrite } => {
                let target = self.entry_path(&path)?;
                let existing = target
                    .try_exists()
                    .map_err(|error| io_error(error, Some(path.clone())))?;
                let backup = existing.then(|| record.backup_root.join(format!("{index}-replaced")));
                if existing && !overwrite {
                    return Err(
                        RemoteError::new(ErrorCode::AlreadyExists, "file already exists").at(path),
                    );
                }
                if let Some(backup) = &backup {
                    move_entry(&target, backup)
                        .map_err(|error| io_error(error, Some(path.clone())))?;
                }
                let created_parent =
                    create_parents(&target).map_err(|error| io_error(error, Some(path.clone())))?;
                if let Err(error) = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&target)
                {
                    let _ = restore_optional_backup(&target, backup.as_deref());
                    if let Some(parent) = created_parent {
                        let _ = remove_if_exists(&parent);
                    }
                    return Err(io_error(error, Some(path)));
                }
                record.undo.push(match backup {
                    Some(backup) => UndoAction::RestoreBackup {
                        path: target,
                        backup,
                    },
                    None => UndoAction::RemoveCreated {
                        path: created_parent.unwrap_or(target),
                    },
                });
                changes.push(FileChange {
                    path,
                    kind: FileChangeKind::Created,
                });
            }
            FileOperation::CreateDirectory { path } => {
                let target = self.entry_path(&path)?;
                if target
                    .try_exists()
                    .map_err(|error| io_error(error, Some(path.clone())))?
                {
                    return Err(RemoteError::new(
                        ErrorCode::AlreadyExists,
                        "directory already exists",
                    )
                    .at(path));
                }
                let first_created = first_missing_ancestor(&target);
                fs::create_dir_all(&target).map_err(|error| io_error(error, Some(path.clone())))?;
                record.undo.push(UndoAction::RemoveCreated {
                    path: first_created.unwrap_or(target),
                });
                changes.push(FileChange {
                    path,
                    kind: FileChangeKind::Created,
                });
            }
            FileOperation::Copy {
                from,
                to,
                overwrite,
            } => {
                reject_nested_destination(&from, &to, "copy")?;
                let source = self.existing_entry_path(&from)?;
                let target = self.entry_path(&to)?;
                let existing = target
                    .try_exists()
                    .map_err(|error| io_error(error, Some(to.clone())))?;
                if existing && !overwrite {
                    return Err(RemoteError::new(
                        ErrorCode::AlreadyExists,
                        "copy destination already exists",
                    )
                    .at(to));
                }
                let backup = existing.then(|| record.backup_root.join(format!("{index}-replaced")));
                if let Some(backup) = &backup {
                    move_entry(&target, backup)
                        .map_err(|error| io_error(error, Some(to.clone())))?;
                }
                let created_parent =
                    create_parents(&target).map_err(|error| io_error(error, Some(to.clone())))?;
                if let Err(error) = copy_entry(&source, &target) {
                    let _ = remove_if_exists(&target);
                    let _ = restore_optional_backup(&target, backup.as_deref());
                    if let Some(parent) = created_parent {
                        let _ = remove_if_exists(&parent);
                    }
                    return Err(io_error(error, Some(to)));
                }
                record.undo.push(match backup {
                    Some(backup) => UndoAction::RestoreBackup {
                        path: target,
                        backup,
                    },
                    None => UndoAction::RemoveCreated {
                        path: created_parent.unwrap_or(target),
                    },
                });
                changes.push(FileChange {
                    path: to,
                    kind: FileChangeKind::Created,
                });
            }
            FileOperation::Rename {
                from,
                to,
                overwrite,
            } => {
                reject_nested_destination(&from, &to, "rename")?;
                let source = self.existing_entry_path(&from)?;
                let target = self.entry_path(&to)?;
                let existing = target
                    .try_exists()
                    .map_err(|error| io_error(error, Some(to.clone())))?;
                if existing && !overwrite {
                    return Err(RemoteError::new(
                        ErrorCode::AlreadyExists,
                        "rename destination already exists",
                    )
                    .at(to));
                }
                let backup = existing.then(|| record.backup_root.join(format!("{index}-replaced")));
                if let Some(backup) = &backup {
                    move_entry(&target, backup)
                        .map_err(|error| io_error(error, Some(to.clone())))?;
                }
                let created_parent =
                    create_parents(&target).map_err(|error| io_error(error, Some(to.clone())))?;
                if let Err(error) = fs::rename(&source, &target) {
                    let _ = restore_optional_backup(&target, backup.as_deref());
                    if let Some(parent) = created_parent {
                        let _ = remove_if_exists(&parent);
                    }
                    return Err(io_error(error, Some(from)));
                }
                record.undo.push(UndoAction::RenameBack {
                    source,
                    destination: target,
                    overwritten: backup,
                    created_parent,
                });
                changes.push(FileChange {
                    path: from,
                    kind: FileChangeKind::Removed,
                });
                changes.push(FileChange {
                    path: to,
                    kind: FileChangeKind::Renamed,
                });
            }
            FileOperation::Remove { path, recursive } => {
                let target = self.existing_entry_path(&path)?;
                let metadata = fs::symlink_metadata(&target)
                    .map_err(|error| io_error(error, Some(path.clone())))?;
                if metadata.is_dir() && !recursive {
                    return Err(RemoteError::new(
                        ErrorCode::InvalidRequest,
                        "directory removal must be recursive",
                    )
                    .at(path));
                }
                let backup = record.backup_root.join(format!("{index}-removed"));
                move_entry(&target, &backup)
                    .map_err(|error| io_error(error, Some(path.clone())))?;
                record.undo.push(UndoAction::RestoreBackup {
                    path: target,
                    backup,
                });
                changes.push(FileChange {
                    path,
                    kind: FileChangeKind::Removed,
                });
            }
        }
        Ok(())
    }

    fn entry_path(&self, path: &WorkspacePath) -> Result<PathBuf, RemoteError> {
        if path.is_root() {
            return Err(RemoteError::new(
                ErrorCode::InvalidPath,
                "workspace root cannot be mutated",
            )
            .at(path.clone()));
        }
        if crate::is_internal_path(path) {
            return Err(RemoteError::new(
                ErrorCode::InvalidPath,
                "the .double-helix workspace namespace is reserved",
            )
            .at(path.clone()));
        }
        let candidate = self
            .root
            .join(crate::fs::native_path(path).map_err(|error| {
                RemoteError::new(ErrorCode::InvalidPath, error.to_string()).at(path.clone())
            })?);
        let parent = candidate.parent().ok_or_else(|| {
            RemoteError::new(ErrorCode::InvalidPath, "path has no parent").at(path.clone())
        })?;
        let existing_parent =
            nearest_existing(parent).map_err(|error| io_error(error, Some(path.clone())))?;
        let resolved = fs::canonicalize(existing_parent)
            .map_err(|error| io_error(error, Some(path.clone())))?;
        if !resolved.starts_with(&self.root) {
            return Err(RemoteError::new(
                ErrorCode::WorkspaceOutsideRoot,
                "path parent resolves outside the remote workspace",
            )
            .at(path.clone()));
        }
        Ok(candidate)
    }

    fn existing_entry_path(&self, path: &WorkspacePath) -> Result<PathBuf, RemoteError> {
        let candidate = self.entry_path(path)?;
        fs::symlink_metadata(&candidate).map_err(|error| io_error(error, Some(path.clone())))?;
        Ok(candidate)
    }
}

impl Drop for FileTransactionStore {
    fn drop(&mut self) {
        if self.journal.is_none() {
            self.clear();
        }
    }
}

fn load_journal(journal: &Path) -> Result<HashMap<TransactionId, TransactionRecord>, RemoteError> {
    let metadata = match fs::symlink_metadata(journal) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(io_error(error, None)),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_JOURNAL_BYTES
    {
        return Err(invalid_journal());
    }
    let bytes = fs::read(journal).map_err(|error| io_error(error, None))?;
    let (version, records): (u16, HashMap<TransactionId, TransactionRecord>) =
        rmp_serde::from_slice(&bytes).map_err(|_| invalid_journal())?;
    if version != JOURNAL_VERSION {
        return Err(invalid_journal());
    }
    Ok(records)
}

fn ensure_secure_directory(root: &Path, directory: &Path) -> Result<(), RemoteError> {
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| invalid_journal())?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(invalid_journal());
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(RemoteError::new(
                        ErrorCode::InvalidPath,
                        "transaction storage contains a symlink or non-directory entry",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = fs::symlink_metadata(&current)
                            .map_err(|error| io_error(error, None))?;
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            return Err(RemoteError::new(
                                ErrorCode::InvalidPath,
                                "transaction storage was replaced while being created",
                            ));
                        }
                    }
                    Err(error) => return Err(io_error(error, None)),
                }
            }
            Err(error) => return Err(io_error(error, None)),
        }
        let resolved = fs::canonicalize(&current).map_err(|error| io_error(error, None))?;
        if !resolved.starts_with(root) {
            return Err(RemoteError::new(
                ErrorCode::WorkspaceOutsideRoot,
                "transaction storage resolves outside the workspace",
            ));
        }
        set_private_directory_permissions(&current).map_err(|error| io_error(error, None))?;
    }
    Ok(())
}

#[cfg(unix)]
fn acquire_journal_lock(storage_root: &Path) -> Result<JournalLock, RemoteError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    let path = storage_root.join("lease.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| io_error(error, None))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|error| io_error(error, None))?;
    // SAFETY: the descriptor belongs to `file`, remains open in JournalLock,
    // and flock does not retain the pointer or access Rust-managed memory.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        return Err(
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                io_error(
                    std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "file transaction session is already active",
                    ),
                    None,
                )
            } else {
                io_error(error, None)
            },
        );
    }
    Ok(JournalLock { _file: file })
}

#[cfg(windows)]
fn acquire_journal_lock(storage_root: &Path) -> Result<JournalLock, RemoteError> {
    use std::os::windows::fs::OpenOptionsExt;

    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .share_mode(0)
        .open(storage_root.join("lease.lock"))
        .map_err(|error| {
            if matches!(error.raw_os_error(), Some(32) | Some(33)) {
                io_error(
                    std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "file transaction session is already active",
                    ),
                    None,
                )
            } else {
                io_error(error, None)
            }
        })?;
    Ok(JournalLock { _file: file })
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn validate_workspace_path(root: &Path, path: &Path) -> Result<(), RemoteError> {
    let relative = path.strip_prefix(root).map_err(|_| invalid_journal())?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(invalid_journal());
    }
    let parent = path.parent().ok_or_else(invalid_journal)?;
    let existing = nearest_existing(parent).map_err(|error| io_error(error, None))?;
    let resolved = fs::canonicalize(existing).map_err(|error| io_error(error, None))?;
    if !resolved.starts_with(root) {
        return Err(RemoteError::new(
            ErrorCode::WorkspaceOutsideRoot,
            "transaction journal path resolves outside the workspace",
        ));
    }
    Ok(())
}

fn validate_backup_path(backup_root: &Path, path: &Path) -> Result<(), RemoteError> {
    let relative = path
        .strip_prefix(backup_root)
        .map_err(|_| invalid_journal())?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(invalid_journal());
    }
    Ok(())
}

fn invalid_journal() -> RemoteError {
    RemoteError::new(
        ErrorCode::InvalidRequest,
        "file transaction journal is invalid or unsafe",
    )
}

fn rollback(record: &mut TransactionRecord) -> Result<(), RemoteError> {
    while let Some(action) = record.undo.last() {
        apply_undo(action).map_err(|error| io_error(error, None))?;
        record.undo.pop();
    }
    Ok(())
}

fn apply_undo(action: &UndoAction) -> std::io::Result<()> {
    match action {
        UndoAction::RemoveCreated { path } => remove_if_exists(path),
        UndoAction::RestoreBackup { path, backup } => {
            if !backup.try_exists()? {
                return if path.try_exists()? {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "transaction backup and restored path are both missing",
                    ))
                };
            }
            remove_if_exists(path)?;
            create_parents(path)?;
            fs::rename(backup, path)
        }
        UndoAction::RenameBack {
            source,
            destination,
            overwritten,
            created_parent,
        } => {
            let source_exists = source.try_exists()?;
            let destination_exists = destination.try_exists()?;
            if !source_exists {
                if !destination_exists {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "renamed source and destination are both missing",
                    ));
                }
                create_parents(source)?;
                fs::rename(destination, source)?;
            } else if destination_exists && overwritten.is_none() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "rename undo destination was recreated",
                ));
            }
            if let Some(backup) = overwritten {
                let backup_exists = backup.try_exists()?;
                let destination_exists = destination.try_exists()?;
                match (backup_exists, destination_exists) {
                    (true, false) => fs::rename(backup, destination)?,
                    (false, true) => {}
                    _ => return Err(std::io::Error::other("rename backup state is ambiguous")),
                }
            }
            if let Some(parent) = created_parent {
                remove_if_exists(parent)?;
            }
            Ok(())
        }
    }
}

fn reject_nested_destination(
    source: &WorkspacePath,
    destination: &WorkspacePath,
    operation: &str,
) -> Result<(), RemoteError> {
    if destination.starts_with(source) {
        Err(RemoteError::new(
            ErrorCode::InvalidRequest,
            format!("cannot {operation} a path onto or inside itself"),
        )
        .at(destination.clone()))
    } else {
        Ok(())
    }
}

fn nearest_existing(mut path: &Path) -> std::io::Result<&Path> {
    loop {
        match fs::symlink_metadata(path) {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                path = path.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn first_missing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut missing = None;
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            break;
        }
        missing = Some(candidate.to_path_buf());
        current = candidate.parent();
    }
    missing
}

fn create_parents(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    let first_created = first_missing_ancestor(parent);
    fs::create_dir_all(parent)?;
    Ok(first_created)
}

fn move_entry(source: &Path, destination: &Path) -> std::io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(source, destination)
}

fn restore_optional_backup(path: &Path, backup: Option<&Path>) -> std::io::Result<()> {
    if let Some(backup) = backup {
        remove_if_exists(path)?;
        fs::rename(backup, path)?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn copy_entry(source: &Path, destination: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return copy_symlink(source, destination);
    }
    if metadata.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
        fs::set_permissions(destination, metadata.permissions())?;
    } else {
        fs::copy(source, destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, destination)
}

#[cfg(windows)]
fn copy_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    let target = fs::read_link(source)?;
    if fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
}

fn io_error(error: std::io::Error, path: Option<WorkspacePath>) -> RemoteError {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::NotFound,
        std::io::ErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
        std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        _ => ErrorCode::Io,
    };
    RemoteError {
        kind,
        path,
        message: error.to_string(),
        source: Some(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> WorkspacePath {
        WorkspacePath::from_slash_path(value).unwrap()
    }

    fn transaction(operations: Vec<FileOperation>) -> FileTransaction {
        FileTransaction { operations }
    }

    #[test]
    fn create_and_remove_are_undoable() {
        let root = tempfile::tempdir().unwrap();
        let mut store = FileTransactionStore::new(root.path());

        store
            .apply(
                FileTransactionId(1),
                transaction(vec![FileOperation::CreateFile {
                    path: path("nested/new.txt"),
                    overwrite: false,
                }]),
            )
            .unwrap();
        assert!(root.path().join("nested/new.txt").exists());
        store.undo(FileTransactionId(1)).unwrap();
        assert!(!root.path().join("nested").exists());

        std::fs::write(root.path().join("existing.txt"), b"original").unwrap();
        store
            .apply(
                FileTransactionId(2),
                transaction(vec![FileOperation::Remove {
                    path: path("existing.txt"),
                    recursive: false,
                }]),
            )
            .unwrap();
        assert!(!root.path().join("existing.txt").exists());
        store.undo(FileTransactionId(2)).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("existing.txt")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn failed_batch_rolls_back_completed_operations() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("collision.txt"), b"keep").unwrap();
        let mut store = FileTransactionStore::new(root.path());

        let error = store
            .apply(
                FileTransactionId(3),
                transaction(vec![
                    FileOperation::CreateFile {
                        path: path("created.txt"),
                        overwrite: false,
                    },
                    FileOperation::CreateFile {
                        path: path("collision.txt"),
                        overwrite: false,
                    },
                ]),
            )
            .unwrap_err();

        assert_eq!(error.kind(), ErrorCode::AlreadyExists);
        assert!(!root.path().join("created.txt").exists());
        assert_eq!(
            std::fs::read(root.path().join("collision.txt")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn rename_overwrite_restores_both_paths() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("from.txt"), b"from").unwrap();
        std::fs::write(root.path().join("to.txt"), b"to").unwrap();
        let mut store = FileTransactionStore::new(root.path());

        store
            .apply(
                FileTransactionId(4),
                transaction(vec![FileOperation::Rename {
                    from: path("from.txt"),
                    to: path("to.txt"),
                    overwrite: true,
                }]),
            )
            .unwrap();
        assert!(!root.path().join("from.txt").exists());
        assert_eq!(std::fs::read(root.path().join("to.txt")).unwrap(), b"from");
        store.undo(FileTransactionId(4)).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("from.txt")).unwrap(),
            b"from"
        );
        assert_eq!(std::fs::read(root.path().join("to.txt")).unwrap(), b"to");
    }

    #[test]
    fn persistent_history_survives_reopen_and_remains_undoable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("important.txt"), b"keep me").unwrap();
        {
            let mut store = FileTransactionStore::open_persistent(root.path(), 42).unwrap();
            store
                .apply(
                    FileTransactionId(1),
                    transaction(vec![FileOperation::Remove {
                        path: path("important.txt"),
                        recursive: false,
                    }]),
                )
                .unwrap();
        }
        assert!(!root.path().join("important.txt").exists());

        let mut reopened = FileTransactionStore::open_persistent(root.path(), 42).unwrap();
        assert_eq!(reopened.next_id().unwrap(), FileTransactionId(2));
        reopened.undo(FileTransactionId(1)).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("important.txt")).unwrap(),
            b"keep me"
        );
        reopened.clear();
    }

    #[test]
    fn persistent_session_has_a_single_writer_lease() {
        let root = tempfile::tempdir().unwrap();
        let first = FileTransactionStore::open_persistent(root.path(), 43).unwrap();
        let error = FileTransactionStore::open_persistent(root.path(), 43)
            .err()
            .expect("second writer should not acquire the same session");
        assert!(matches!(
            error.source.as_ref().map(std::io::Error::kind),
            Some(std::io::ErrorKind::WouldBlock)
        ));
        drop(first);

        let mut reopened = FileTransactionStore::open_persistent(root.path(), 43).unwrap();
        reopened.clear();
    }

    #[test]
    fn persistent_journal_rejects_paths_outside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = FileTransactionStore::open_persistent(root.path(), 7).unwrap();
        let backup_root = store.storage_root.join("1");
        std::fs::create_dir_all(&backup_root).unwrap();
        let records = HashMap::from([(
            FileTransactionId(1),
            TransactionRecord {
                undo: vec![UndoAction::RemoveCreated {
                    path: outside.path().join("victim"),
                }],
                backup_root,
                undoing: false,
            },
        )]);
        let bytes = rmp_serde::to_vec_named(&(JOURNAL_VERSION, records)).unwrap();
        std::fs::write(store.journal.as_ref().unwrap(), bytes).unwrap();
        drop(store);

        let error = FileTransactionStore::open_persistent(root.path(), 7)
            .err()
            .expect("unsafe journal should be rejected");
        assert_eq!(error.kind(), ErrorCode::InvalidRequest);
    }

    #[test]
    fn transaction_storage_rejects_symlink_redirection() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let internal = root.path().join(".double-helix");

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &internal).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(outside.path(), &internal) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("failed to create test symlink: {error}");
        }

        let error = FileTransactionStore::open_persistent(root.path(), 9)
            .err()
            .expect("symlinked transaction storage should be rejected");
        assert_eq!(error.kind(), ErrorCode::InvalidPath);
    }

    #[test]
    fn rejects_reserved_and_nested_destinations() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("source")).unwrap();
        let mut store = FileTransactionStore::new(root.path());

        let reserved = store
            .apply(
                FileTransactionId(5),
                transaction(vec![FileOperation::CreateDirectory {
                    path: path(".double-helix/user"),
                }]),
            )
            .unwrap_err();
        assert_eq!(reserved.kind(), ErrorCode::InvalidPath);

        let nested = store
            .apply(
                FileTransactionId(6),
                transaction(vec![FileOperation::Copy {
                    from: path("source"),
                    to: path("source/child"),
                    overwrite: false,
                }]),
            )
            .unwrap_err();
        assert_eq!(nested.kind(), ErrorCode::InvalidRequest);
    }
}
