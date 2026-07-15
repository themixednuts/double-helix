use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fs,
    hash::{Hash, Hasher},
    io::{self, ErrorKind, Read},
    path::{Path, PathBuf},
};

use helix_lsp::{jsonrpc, lsp, LanguageServerId, OffsetEncoding};
use helix_stdx::path::canonicalize;

use crate::handlers::workspace_edit::{
    WorkspaceEditExecution, WorkspaceEditExecutionStep, WorkspaceEditPlan,
};

/// Identity assigned by the editor-owned file-operation FIFO.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileOperationId(u64);

impl FileOperationId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The caller that submitted a mutation. Workspace edits deliberately do not
/// run `will*` requests: they are already server-originated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOperationOrigin {
    Explorer {
        root: PathBuf,
        cursor: u32,
        select_path: Option<PathBuf>,
    },
    Command,
    WorkspaceEdit {
        batch: Option<WorkspaceEditBatchId>,
        member: Option<usize>,
        failed_change_idx: Option<usize>,
    },
}

impl FileOperationOrigin {
    pub const fn requests_lsp_will(&self) -> bool {
        !matches!(self, Self::WorkspaceEdit { .. })
    }

    pub const fn workspace_edit() -> Self {
        Self::WorkspaceEdit {
            batch: None,
            member: None,
            failed_change_idx: None,
        }
    }
}

/// Identity for one asynchronous `workspace/applyEdit` or code-action edit batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceEditBatchId(u64);

/// A terminal-side action to run exactly once after an asynchronous workspace
/// edit has applied all of its resource operations successfully or failed.
#[derive(Clone, Debug)]
pub enum WorkspaceEditContinuation {
    ApplyEditReply {
        server_id: LanguageServerId,
        request_id: jsonrpc::Id,
    },
    ExecuteCommand {
        server_id: LanguageServerId,
        command: lsp::Command,
    },
    ResumeFileOperation {
        id: FileOperationId,
    },
}

#[derive(Clone, Debug)]
pub struct WorkspaceEditBatchCompletion {
    pub continuation: Option<WorkspaceEditContinuation>,
    pub result: Result<(), WorkspaceEditBatchError>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceEditBatchError {
    pub message: String,
    pub failed_change_idx: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOperationDeleteMode {
    Trash,
    Permanent,
}

impl FileOperationDeleteMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trash => "trash",
            Self::Permanent => "delete",
        }
    }
}

/// How a destination is chosen. `UniqueInDirectory` keeps clipboard paste
/// inspection off the main loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOperationDestination {
    Exact(PathBuf),
    PathOrDirectory(PathBuf),
    UniqueInDirectory(PathBuf),
}

/// A serializable file-system action. All inspection and mutation happens in
/// [`FileOperationWork::execute`] on a blocking worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOperation {
    Create {
        path: PathBuf,
        is_dir: bool,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Copy {
        source: PathBuf,
        destination: FileOperationDestination,
        overwrite: bool,
        create_parents: bool,
    },
    Move {
        source: PathBuf,
        destination: FileOperationDestination,
        overwrite: bool,
        ignore_if_exists: bool,
        create_parents: bool,
    },
    Delete {
        path: PathBuf,
        mode: FileOperationDeleteMode,
        recursive: bool,
        ignore_missing: bool,
    },
    Undo,
    Redo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOperationRequest {
    pub origin: FileOperationOrigin,
    pub operation: FileOperation,
}

impl FileOperationRequest {
    pub fn create(origin: FileOperationOrigin, path: PathBuf, is_dir: bool) -> Self {
        Self {
            origin,
            operation: FileOperation::Create {
                path,
                is_dir,
                overwrite: false,
                ignore_if_exists: false,
            },
        }
    }

    pub fn move_path(
        origin: FileOperationOrigin,
        source: PathBuf,
        destination: PathBuf,
        create_parents: bool,
    ) -> Self {
        Self::move_to_destination(
            origin,
            source,
            FileOperationDestination::Exact(destination),
            create_parents,
        )
    }

    pub fn move_to_destination(
        origin: FileOperationOrigin,
        source: PathBuf,
        destination: FileOperationDestination,
        create_parents: bool,
    ) -> Self {
        Self {
            origin,
            operation: FileOperation::Move {
                source,
                destination,
                overwrite: false,
                ignore_if_exists: false,
                create_parents,
            },
        }
    }

    pub fn copy_path(
        origin: FileOperationOrigin,
        source: PathBuf,
        destination: FileOperationDestination,
    ) -> Self {
        Self {
            origin,
            operation: FileOperation::Copy {
                source,
                destination,
                overwrite: false,
                create_parents: true,
            },
        }
    }

    pub fn trash(origin: FileOperationOrigin, path: PathBuf) -> Self {
        Self {
            origin,
            operation: FileOperation::Delete {
                path,
                mode: FileOperationDeleteMode::Trash,
                recursive: true,
                ignore_missing: false,
            },
        }
    }

    pub fn undo(origin: FileOperationOrigin) -> Self {
        Self {
            origin,
            operation: FileOperation::Undo,
        }
    }

    pub fn redo(origin: FileOperationOrigin) -> Self {
        Self {
            origin,
            operation: FileOperation::Redo,
        }
    }
}

/// Visible state of an operation while it is resident in the FIFO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOperationState {
    Queued,
    Inspecting,
    AwaitingWill,
    AwaitingWorkspaceEdit,
    AwaitingPrerequisite,
    Mutating,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOperationChange {
    Create {
        path: PathBuf,
        is_dir: bool,
    },
    Delete {
        path: PathBuf,
        is_dir: bool,
    },
    Move {
        from: PathBuf,
        to: PathBuf,
        is_dir: bool,
    },
}

impl FileOperationChange {
    pub fn affected_paths(&self) -> Box<dyn Iterator<Item = &Path> + '_> {
        match self {
            Self::Create { path, .. } | Self::Delete { path, .. } => {
                Box::new(std::iter::once(path.as_path()))
            }
            Self::Move { from, to, .. } => Box::new([from.as_path(), to.as_path()].into_iter()),
        }
    }

    fn inverse(&self) -> Self {
        match self {
            Self::Create { path, is_dir } => Self::Delete {
                path: path.clone(),
                is_dir: *is_dir,
            },
            Self::Delete { path, is_dir } => Self::Create {
                path: path.clone(),
                is_dir: *is_dir,
            },
            Self::Move { from, to, is_dir } => Self::Move {
                from: to.clone(),
                to: from.clone(),
                is_dir: *is_dir,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOperationApplied {
    pub changes: Box<[FileOperationChange]>,
    pub affected_paths: Box<[PathBuf]>,
}

impl FileOperationApplied {
    fn new(changes: Vec<FileOperationChange>, affected_paths: Vec<PathBuf>) -> Self {
        let mut seen = BTreeSet::new();
        let affected_paths = affected_paths
            .into_iter()
            .map(|path| canonicalize(&path))
            .filter(|path| seen.insert(path.clone()))
            .collect();
        Self {
            changes: changes.into_boxed_slice(),
            affected_paths,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOperationError {
    Io {
        kind: ErrorKind,
        message: String,
    },
    UnsupportedDirectoryCopy {
        path: PathBuf,
    },
    WorkspaceEdit {
        message: String,
    },
    DependencyFailed {
        operation: FileOperationId,
        message: String,
    },
    NoHistory,
}

impl std::fmt::Display for FileOperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { message, .. } => formatter.write_str(message),
            Self::UnsupportedDirectoryCopy { path } => write!(
                formatter,
                "copying directories is not supported: {}",
                path.display()
            ),
            Self::WorkspaceEdit { message } => {
                write!(formatter, "failed to apply workspace edit: {message}")
            }
            Self::DependencyFailed { operation, message } => write!(
                formatter,
                "file operation prerequisite {} failed: {message}",
                operation.get()
            ),
            Self::NoHistory => formatter.write_str("no file operation to replay"),
        }
    }
}

impl From<io::Error> for FileOperationError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

/// Blocking preparation result. It carries the exact destination selected for
/// a paste and the `will*` operation to request on the async runtime.
#[derive(Clone, Debug)]
pub struct FileOperationPrepared {
    id: FileOperationId,
    operation: PreparedFileOperation,
    will_change: Option<FileOperationChange>,
}

impl FileOperationPrepared {
    pub const fn id(&self) -> FileOperationId {
        self.id
    }

    pub fn will_change(&self) -> Option<&FileOperationChange> {
        self.will_change.as_ref()
    }
}

impl FileOperationInspection {
    pub const fn id(&self) -> FileOperationId {
        self.id
    }
}

#[derive(Clone, Debug)]
enum PreparedFileOperation {
    Create {
        path: PathBuf,
        is_dir: bool,
        created: bool,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Copy {
        source: PathBuf,
        destination: PathBuf,
        created: bool,
        overwrite: bool,
        create_parents: bool,
    },
    Move {
        source: PathBuf,
        destination: PathBuf,
        is_dir: bool,
        overwrite: bool,
        skip: bool,
        create_parents: bool,
    },
    Delete {
        path: PathBuf,
        is_dir: bool,
        mode: FileOperationDeleteMode,
        recursive: bool,
        ignore_missing: bool,
    },
    Replay {
        direction: ReplayDirection,
        change: FileOperationChange,
    },
    NoHistory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplayDirection {
    Undo,
    Redo,
}

/// Work handed to `spawn_blocking`. It owns the journal record only while the
/// worker is running, so the UI never traverses or decompresses snapshots.
#[derive(Debug)]
pub struct FileOperationWork {
    id: FileOperationId,
    operation: PreparedFileOperation,
    record_history: bool,
    record: Option<FileOperationRecord>,
}

#[derive(Debug)]
pub struct FileOperationOutcome {
    pub id: FileOperationId,
    pub result: Result<FileOperationApplied, FileOperationError>,
    record: Option<FileOperationRecord>,
}

impl FileOperationOutcome {
    pub fn task_failed(id: FileOperationId, message: String) -> Self {
        Self {
            id,
            result: Err(FileOperationError::Io {
                kind: ErrorKind::Other,
                message,
            }),
            record: None,
        }
    }
}

impl FileOperationWork {
    pub const fn id(&self) -> FileOperationId {
        self.id
    }

    pub fn execute(self) -> FileOperationOutcome {
        let Self {
            id,
            operation,
            record_history,
            record,
        } = self;
        let result = match &operation {
            PreparedFileOperation::NoHistory => Err(FileOperationError::NoHistory),
            PreparedFileOperation::Replay { direction, change } => {
                let record = record.as_ref().expect("replay work owns a journal record");
                let result = match direction {
                    ReplayDirection::Undo => record.undo(),
                    ReplayDirection::Redo => record.redo(),
                };
                result.map(|()| {
                    let change = if *direction == ReplayDirection::Undo {
                        change.inverse()
                    } else {
                        change.clone()
                    };
                    (
                        FileOperationApplied::new(
                            vec![change],
                            record.paths().map(Path::to_path_buf).collect(),
                        ),
                        None,
                    )
                })
            }
            _ => execute_new_operation(&operation, record_history),
        };

        match result {
            Ok((applied, created_record)) => FileOperationOutcome {
                id,
                result: Ok(applied),
                record: created_record.or(record),
            },
            Err(error) => FileOperationOutcome {
                id,
                result: Err(error),
                record,
            },
        }
    }
}

fn execute_new_operation(
    operation: &PreparedFileOperation,
    record_history: bool,
) -> Result<(FileOperationApplied, Option<FileOperationRecord>), FileOperationError> {
    if let PreparedFileOperation::Move {
        source,
        destination,
        overwrite: false,
        ..
    } = operation
    {
        if record_history {
            let before = MoveFingerprint::capture(source, destination)?;
            let (kind, applied) = execute_mutation(operation)?;
            let after = MoveFingerprint::capture(source, destination)?;
            let record = (before != after).then_some(FileOperationRecord {
                kind,
                history: FileOperationHistory::Move { before, after },
            });
            return Ok((applied, record));
        }
    }

    let paths = history_paths(operation)?;
    let before = record_history
        .then(|| capture_images(paths.clone()))
        .transpose()?;
    let (kind, applied) = execute_mutation(operation)?;
    let after = record_history.then(|| capture_images(paths)).transpose()?;
    let record = before.zip(after).and_then(|(before, after)| {
        (before != after).then_some(FileOperationRecord {
            kind,
            history: FileOperationHistory::Snapshots { before, after },
        })
    });
    Ok((applied, record))
}

fn execute_mutation(
    operation: &PreparedFileOperation,
) -> Result<(FileOperationKind, FileOperationApplied), FileOperationError> {
    match operation {
        PreparedFileOperation::Create {
            path,
            is_dir,
            created,
            overwrite,
            ignore_if_exists,
        } => {
            if !created && *ignore_if_exists {
                return Ok((
                    FileOperationKind::Create {
                        path: path.clone(),
                        is_dir: *is_dir,
                    },
                    FileOperationApplied::new(Vec::new(), Vec::new()),
                ));
            }
            if *is_dir {
                if !created && !*overwrite {
                    return Err(already_exists(path));
                }
                fs::create_dir_all(path)?;
            } else {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut options = fs::OpenOptions::new();
                options.write(true);
                if *created {
                    options.create_new(true);
                } else if *overwrite {
                    options.create(true).truncate(true);
                } else {
                    return Err(already_exists(path));
                }
                options.open(path)?;
            }
            Ok((
                FileOperationKind::Create {
                    path: path.clone(),
                    is_dir: *is_dir,
                },
                FileOperationApplied::new(
                    (*created)
                        .then(|| FileOperationChange::Create {
                            path: path.clone(),
                            is_dir: *is_dir,
                        })
                        .into_iter()
                        .collect(),
                    vec![path.clone()],
                ),
            ))
        }
        PreparedFileOperation::Copy {
            source,
            destination,
            created,
            overwrite,
            create_parents,
        } => {
            if !*created && !*overwrite {
                return Err(already_exists(destination));
            }
            if *create_parents {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::copy(source, destination)?;
            Ok((
                FileOperationKind::Copy {
                    destination: destination.clone(),
                },
                FileOperationApplied::new(
                    (*created)
                        .then(|| FileOperationChange::Create {
                            path: destination.clone(),
                            is_dir: false,
                        })
                        .into_iter()
                        .collect(),
                    vec![destination.clone()],
                ),
            ))
        }
        PreparedFileOperation::Move {
            source,
            destination,
            is_dir,
            overwrite,
            skip,
            create_parents,
        } => {
            if *skip {
                return Ok((
                    FileOperationKind::Move {
                        source: source.clone(),
                        destination: destination.clone(),
                        is_dir: *is_dir,
                    },
                    FileOperationApplied::new(Vec::new(), vec![source.clone()]),
                ));
            }
            if *create_parents {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
            }
            if symlink_metadata_optional(destination)?.is_some() {
                if !*overwrite {
                    return Err(already_exists(destination));
                }
                remove_path(destination)?;
            }
            fs::rename(source, destination)?;
            Ok((
                FileOperationKind::Move {
                    source: source.clone(),
                    destination: destination.clone(),
                    is_dir: *is_dir,
                },
                FileOperationApplied::new(
                    vec![FileOperationChange::Move {
                        from: source.clone(),
                        to: destination.clone(),
                        is_dir: *is_dir,
                    }],
                    vec![source.clone(), destination.clone()],
                ),
            ))
        }
        PreparedFileOperation::Delete {
            path,
            is_dir,
            mode,
            recursive,
            ignore_missing,
        } => {
            if symlink_metadata_optional(path)?.is_none() {
                if *ignore_missing {
                    return Ok((
                        FileOperationKind::Delete {
                            path: path.clone(),
                            is_dir: *is_dir,
                        },
                        FileOperationApplied::new(Vec::new(), Vec::new()),
                    ));
                }
                return Err(not_found(path));
            }
            match mode {
                FileOperationDeleteMode::Trash => {
                    trash::delete(path).map_err(|error| io::Error::other(error.to_string()))?
                }
                FileOperationDeleteMode::Permanent if *is_dir && *recursive => {
                    fs::remove_dir_all(path)?
                }
                FileOperationDeleteMode::Permanent if *is_dir => fs::remove_dir(path)?,
                FileOperationDeleteMode::Permanent => fs::remove_file(path)?,
            }
            Ok((
                FileOperationKind::Delete {
                    path: path.clone(),
                    is_dir: *is_dir,
                },
                FileOperationApplied::new(
                    vec![FileOperationChange::Delete {
                        path: path.clone(),
                        is_dir: *is_dir,
                    }],
                    vec![path.clone()],
                ),
            ))
        }
        PreparedFileOperation::Replay { .. } | PreparedFileOperation::NoHistory => {
            unreachable!("replay and no-history work is handled before mutation")
        }
    }
}

fn history_paths(operation: &PreparedFileOperation) -> Result<Vec<PathBuf>, FileOperationError> {
    let mut paths = match operation {
        PreparedFileOperation::Create { path, .. } | PreparedFileOperation::Delete { path, .. } => {
            vec![path.clone()]
        }
        PreparedFileOperation::Copy { destination, .. } => vec![destination.clone()],
        PreparedFileOperation::Move {
            source,
            destination,
            ..
        } => vec![source.clone(), destination.clone()],
        PreparedFileOperation::Replay { .. } | PreparedFileOperation::NoHistory => Vec::new(),
    };
    let destination = match operation {
        PreparedFileOperation::Create { path, .. } => Some(path),
        PreparedFileOperation::Copy { destination, .. }
        | PreparedFileOperation::Move { destination, .. } => Some(destination),
        _ => None,
    };
    if let Some(destination) = destination {
        let mut parent = destination.parent();
        while let Some(path) = parent {
            if symlink_metadata_optional(path)?.is_some() {
                break;
            }
            paths.push(path.to_path_buf());
            parent = path.parent();
        }
    }
    Ok(paths)
}

fn already_exists(path: &Path) -> FileOperationError {
    FileOperationError::Io {
        kind: ErrorKind::AlreadyExists,
        message: format!("path already exists: {}", path.display()),
    }
}

fn not_found(path: &Path) -> FileOperationError {
    FileOperationError::Io {
        kind: ErrorKind::NotFound,
        message: format!("path does not exist: {}", path.display()),
    }
}

#[derive(Default)]
pub(crate) struct FileOperationJournal {
    next_id: u64,
    next_workspace_edit_batch: u64,
    queued: VecDeque<QueuedFileOperation>,
    active: Option<ActiveFileOperation>,
    dependencies: Vec<FileOperationDependencyFrame>,
    workspace_edit_batches: HashMap<WorkspaceEditBatchId, WorkspaceEditBatch>,
    undo: Vec<FileOperationRecord>,
    redo: Vec<FileOperationRecord>,
}

struct QueuedFileOperation {
    id: FileOperationId,
    request: FileOperationRequest,
}

struct ActiveFileOperation {
    id: FileOperationId,
    request: FileOperationRequest,
    state: FileOperationState,
    record: Option<FileOperationRecord>,
    prepared: Option<PreparedFileOperation>,
    workspace_edits: Option<PendingWorkspaceEdits>,
}

struct PendingWorkspaceEdits {
    edits: VecDeque<(OffsetEncoding, lsp::WorkspaceEdit)>,
    in_flight: bool,
    prerequisites: Vec<FileOperationRequest>,
}

struct FileOperationDependencyFrame {
    parent: ActiveFileOperation,
    prerequisites: VecDeque<QueuedFileOperation>,
    resume: DependencyResume,
}

struct WorkspaceEditBatch {
    execution: WorkspaceEditExecution,
    waiting_member: Option<usize>,
    continuation: Option<WorkspaceEditContinuation>,
    parent: Option<FileOperationId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DependencyResume {
    MutateParent,
    ResumeWorkspaceEdit {
        batch: WorkspaceEditBatchId,
        member: usize,
    },
}

/// What the terminal coordinator should do after all `will*` workspace edits
/// have been prepared and applied on the UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOperationWorkspaceEditAction {
    Mutate,
    Drive,
}

/// The next action selected by an editor-owned WorkspaceEdit execution cursor.
#[derive(Debug)]
pub enum WorkspaceEditExecutionDispatch {
    EnqueueResource(FileOperationRequest),
    Drive,
    Advance(WorkspaceEditBatchId),
    Complete(WorkspaceEditBatchCompletion),
}

/// An execution cursor can abort the file-operation parent that was waiting
/// for a `will*` edit. The terminal applies this completion after it has
/// processed the resource completion that caused the abort.
#[derive(Debug)]
pub struct WorkspaceEditExecutionUpdate {
    pub dispatch: WorkspaceEditExecutionDispatch,
    pub parent_completion: Option<FileOperationCompletion>,
}

/// A small job that only probes filesystem metadata and resolves a paste
/// destination. It is safe to run before the `will*` chain.
#[derive(Clone, Debug)]
pub struct FileOperationInspection {
    id: FileOperationId,
    operation: FileOperation,
    replay_change: Option<FileOperationChange>,
}

/// The next unit of work selected by the editor-owned file-operation journal.
/// Inspection and mutation both remain off the main loop; only the selection is
/// made on the UI thread.
#[derive(Debug)]
pub enum FileOperationDispatch {
    Inspect(FileOperationInspection),
    Mutate(FileOperationWork),
}

impl FileOperationInspection {
    pub fn execute(self) -> Result<FileOperationPrepared, FileOperationError> {
        let operation = match self.operation {
            FileOperation::Create {
                path,
                is_dir,
                overwrite,
                ignore_if_exists,
            } => {
                let path = canonicalize(&path);
                let created = symlink_metadata_optional(&path)?.is_none();
                PreparedFileOperation::Create {
                    path: path.clone(),
                    is_dir,
                    created,
                    overwrite,
                    ignore_if_exists,
                }
            }
            FileOperation::Copy {
                source,
                destination,
                overwrite,
                create_parents,
            } => {
                let source = canonicalize(&source);
                let source_metadata = fs::symlink_metadata(&source)?;
                if source_metadata.is_dir() {
                    return Err(FileOperationError::UnsupportedDirectoryCopy { path: source });
                }
                let destination = resolve_destination(destination, &source)?;
                let created = symlink_metadata_optional(&destination)?.is_none();
                PreparedFileOperation::Copy {
                    source,
                    destination,
                    created,
                    overwrite,
                    create_parents,
                }
            }
            FileOperation::Move {
                source,
                destination,
                overwrite,
                ignore_if_exists,
                create_parents,
            } => {
                let source = canonicalize(&source);
                let metadata = fs::symlink_metadata(&source)?;
                let destination = resolve_destination(destination, &source)?;
                let skip = source == destination
                    || (ignore_if_exists && symlink_metadata_optional(&destination)?.is_some());
                PreparedFileOperation::Move {
                    source,
                    destination,
                    is_dir: metadata.is_dir(),
                    overwrite,
                    skip,
                    create_parents,
                }
            }
            FileOperation::Delete {
                path,
                mode,
                recursive,
                ignore_missing,
            } => {
                let path = canonicalize(&path);
                let metadata = symlink_metadata_optional(&path)?;
                let Some(metadata) = metadata else {
                    if ignore_missing {
                        return Ok(FileOperationPrepared {
                            id: self.id,
                            operation: PreparedFileOperation::Delete {
                                path,
                                is_dir: false,
                                mode,
                                recursive,
                                ignore_missing,
                            },
                            will_change: None,
                        });
                    }
                    return Err(not_found(&path));
                };
                PreparedFileOperation::Delete {
                    path,
                    is_dir: metadata.is_dir(),
                    mode,
                    recursive,
                    ignore_missing,
                }
            }
            FileOperation::Undo | FileOperation::Redo => match self.replay_change {
                Some(change) => PreparedFileOperation::Replay {
                    direction: if matches!(self.operation, FileOperation::Undo) {
                        ReplayDirection::Undo
                    } else {
                        ReplayDirection::Redo
                    },
                    change,
                },
                None => PreparedFileOperation::NoHistory,
            },
        };
        let will_change = match &operation {
            PreparedFileOperation::Create {
                path,
                is_dir,
                created,
                ..
            } if *created => Some(FileOperationChange::Create {
                path: path.clone(),
                is_dir: *is_dir,
            }),
            PreparedFileOperation::Copy {
                destination,
                created,
                ..
            } if *created => Some(FileOperationChange::Create {
                path: destination.clone(),
                is_dir: false,
            }),
            PreparedFileOperation::Move {
                source,
                destination,
                is_dir,
                skip,
                ..
            } if !skip => Some(FileOperationChange::Move {
                from: source.clone(),
                to: destination.clone(),
                is_dir: *is_dir,
            }),
            PreparedFileOperation::Delete { path, is_dir, .. } => {
                Some(FileOperationChange::Delete {
                    path: path.clone(),
                    is_dir: *is_dir,
                })
            }
            PreparedFileOperation::Replay { direction, change } => {
                Some(if *direction == ReplayDirection::Undo {
                    change.inverse()
                } else {
                    change.clone()
                })
            }
            PreparedFileOperation::NoHistory
            | PreparedFileOperation::Create { .. }
            | PreparedFileOperation::Copy { .. }
            | PreparedFileOperation::Move { .. } => None,
        };
        Ok(FileOperationPrepared {
            id: self.id,
            operation,
            will_change,
        })
    }
}

impl FileOperationJournal {
    pub(crate) fn enqueue(&mut self, request: FileOperationRequest) -> FileOperationId {
        let id = self.next_id();
        self.queued.push_back(QueuedFileOperation { id, request });
        id
    }

    fn next_id(&mut self) -> FileOperationId {
        self.next_id = self.next_id.saturating_add(1);
        FileOperationId(self.next_id)
    }

    fn begin_workspace_edit_execution(
        &mut self,
        execution: WorkspaceEditExecution,
        continuation: Option<WorkspaceEditContinuation>,
        parent: Option<FileOperationId>,
    ) -> WorkspaceEditBatchId {
        self.next_workspace_edit_batch = self.next_workspace_edit_batch.saturating_add(1);
        let batch_id = WorkspaceEditBatchId(self.next_workspace_edit_batch);
        self.workspace_edit_batches.insert(
            batch_id,
            WorkspaceEditBatch {
                execution,
                waiting_member: None,
                continuation,
                parent,
            },
        );
        batch_id
    }

    fn take_workspace_edit_batch(
        &mut self,
        batch_id: WorkspaceEditBatchId,
    ) -> Option<WorkspaceEditBatch> {
        let batch = self.workspace_edit_batches.remove(&batch_id)?;
        if batch.waiting_member.is_some() {
            self.workspace_edit_batches.insert(batch_id, batch);
            return None;
        }
        Some(batch)
    }

    fn take_workspace_edit_batch_for_completion(
        &mut self,
        completion: &FileOperationCompletion,
    ) -> Option<(WorkspaceEditBatchId, usize, WorkspaceEditBatch)> {
        let FileOperationOrigin::WorkspaceEdit {
            batch: Some(batch_id),
            member: Some(member),
            ..
        } = &completion.request.origin
        else {
            return None;
        };
        let batch = self.workspace_edit_batches.remove(batch_id)?;
        if batch.waiting_member != Some(*member) {
            self.workspace_edit_batches.insert(*batch_id, batch);
            return None;
        }
        Some((*batch_id, *member, batch))
    }

    fn suspend_for_workspace_edit_resource(
        &mut self,
        parent_id: FileOperationId,
        batch_id: WorkspaceEditBatchId,
        member: usize,
        request: FileOperationRequest,
    ) -> bool {
        let Some(mut parent) = self.active.take() else {
            return false;
        };
        if parent.id != parent_id || parent.state != FileOperationState::AwaitingWorkspaceEdit {
            self.active = Some(parent);
            return false;
        }
        parent.state = FileOperationState::AwaitingPrerequisite;
        let child = QueuedFileOperation {
            id: self.next_id(),
            request,
        };
        self.dependencies.push(FileOperationDependencyFrame {
            parent,
            prerequisites: VecDeque::from([child]),
            resume: DependencyResume::ResumeWorkspaceEdit {
                batch: batch_id,
                member,
            },
        });
        true
    }

    fn resume_workspace_edit_parent(
        &mut self,
        parent_id: FileOperationId,
        batch_id: WorkspaceEditBatchId,
        member: usize,
    ) -> bool {
        let Some(dependency) = self.dependencies.last() else {
            return false;
        };
        if dependency.resume
            != (DependencyResume::ResumeWorkspaceEdit {
                batch: batch_id,
                member,
            })
            || dependency.parent.id != parent_id
        {
            return false;
        }
        let mut dependency = self
            .dependencies
            .pop()
            .expect("workspace edit dependency should still be present");
        dependency.parent.state = FileOperationState::AwaitingWorkspaceEdit;
        self.active = Some(dependency.parent);
        true
    }

    fn abort_workspace_edit_parent(
        &mut self,
        parent_id: FileOperationId,
        batch_id: WorkspaceEditBatchId,
        member: usize,
        error: &FileOperationError,
    ) -> Option<FileOperationCompletion> {
        let dependency = self.dependencies.last()?;
        if dependency.resume
            != (DependencyResume::ResumeWorkspaceEdit {
                batch: batch_id,
                member,
            })
            || dependency.parent.id != parent_id
        {
            return None;
        }
        let mut dependency = self
            .dependencies
            .pop()
            .expect("workspace edit dependency should still be present");
        self.restore_replay_record(&mut dependency.parent);
        Some(FileOperationCompletion {
            id: dependency.parent.id,
            request: dependency.parent.request,
            result: Err(FileOperationError::WorkspaceEdit {
                message: error.to_string(),
            }),
        })
    }

    pub(crate) fn next_dispatch(&mut self) -> Option<FileOperationDispatch> {
        if self.active.is_some() {
            return None;
        }

        if let Some(dependency) = self.dependencies.last_mut() {
            if let Some(queued) = dependency.prerequisites.pop_front() {
                return Some(FileOperationDispatch::Inspect(self.activate(queued)));
            }

            if dependency.resume != DependencyResume::MutateParent {
                return None;
            }

            let mut dependency = self
                .dependencies
                .pop()
                .expect("dependency frame should still be present");
            dependency.parent.state = FileOperationState::AwaitingWill;
            let id = dependency.parent.id;
            self.active = Some(dependency.parent);
            return self.begin_mutation(id).map(FileOperationDispatch::Mutate);
        }

        self.queued
            .pop_front()
            .map(|queued| FileOperationDispatch::Inspect(self.activate(queued)))
    }

    fn activate(
        &mut self,
        QueuedFileOperation { id, request }: QueuedFileOperation,
    ) -> FileOperationInspection {
        let record = match request.operation {
            FileOperation::Undo => self.undo.pop(),
            FileOperation::Redo => self.redo.pop(),
            _ => None,
        };
        let replay_change = record.as_ref().map(FileOperationRecord::change);
        self.active = Some(ActiveFileOperation {
            id,
            request: request.clone(),
            state: FileOperationState::Inspecting,
            record,
            prepared: None,
            workspace_edits: None,
        });
        FileOperationInspection {
            id,
            operation: request.operation,
            replay_change,
        }
    }

    pub(crate) fn accept_prepared(
        &mut self,
        prepared: FileOperationPrepared,
    ) -> Option<(FileOperationRequest, FileOperationPrepared)> {
        let active = self.active.as_mut()?;
        if active.id != prepared.id || active.state != FileOperationState::Inspecting {
            return None;
        }
        active.state = FileOperationState::AwaitingWill;
        active.prepared = Some(prepared.clone().operation);
        Some((active.request.clone(), prepared))
    }

    pub(crate) fn begin_mutation(&mut self, id: FileOperationId) -> Option<FileOperationWork> {
        let active = self.active.as_mut()?;
        if active.id != id || active.state != FileOperationState::AwaitingWill {
            return None;
        }
        active.state = FileOperationState::Mutating;
        let record_history = !matches!(
            active.request.origin,
            FileOperationOrigin::WorkspaceEdit { .. }
        ) && !matches!(
            active.request.operation,
            FileOperation::Undo | FileOperation::Redo
        );
        Some(FileOperationWork {
            id,
            operation: active.prepared.take()?,
            record_history,
            record: active.record.take(),
        })
    }

    pub(crate) fn accepts_will_completion(&self, id: FileOperationId) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.id == id && active.state == FileOperationState::AwaitingWill
        })
    }

    pub(crate) fn begin_workspace_edits(
        &mut self,
        id: FileOperationId,
        edits: Vec<(OffsetEncoding, lsp::WorkspaceEdit)>,
    ) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if active.id != id || active.state != FileOperationState::AwaitingWill {
            return false;
        }
        active.state = FileOperationState::AwaitingWorkspaceEdit;
        active.workspace_edits = Some(PendingWorkspaceEdits {
            edits: edits.into(),
            in_flight: false,
            prerequisites: Vec::new(),
        });
        true
    }

    pub(crate) fn next_workspace_edit(
        &mut self,
        id: FileOperationId,
    ) -> Option<(OffsetEncoding, lsp::WorkspaceEdit)> {
        let active = self.active.as_mut()?;
        if active.id != id || active.state != FileOperationState::AwaitingWorkspaceEdit {
            return None;
        }
        let pending = active.workspace_edits.as_mut()?;
        if pending.in_flight {
            return None;
        }
        let edit = pending.edits.pop_front()?;
        pending.in_flight = true;
        Some(edit)
    }

    pub(crate) fn accepts_workspace_edit_preparation(&mut self, id: FileOperationId) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if active.id != id || active.state != FileOperationState::AwaitingWorkspaceEdit {
            return false;
        }
        let Some(pending) = active.workspace_edits.as_mut() else {
            return false;
        };
        if !pending.in_flight {
            return false;
        }
        pending.in_flight = false;
        true
    }

    pub(crate) fn add_workspace_edit_prerequisites(
        &mut self,
        id: FileOperationId,
        requests: impl IntoIterator<Item = FileOperationRequest>,
    ) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if active.id != id || active.state != FileOperationState::AwaitingWorkspaceEdit {
            return false;
        }
        let Some(pending) = active.workspace_edits.as_mut() else {
            return false;
        };
        if pending.in_flight {
            return false;
        }
        pending.prerequisites.extend(requests);
        true
    }

    pub(crate) fn finish_workspace_edits(
        &mut self,
        id: FileOperationId,
    ) -> Option<FileOperationWorkspaceEditAction> {
        let active = self.active.as_ref()?;
        if active.id != id || active.state != FileOperationState::AwaitingWorkspaceEdit {
            return None;
        }
        let pending = active.workspace_edits.as_ref()?;
        if pending.in_flight || !pending.edits.is_empty() {
            return None;
        }

        if pending.prerequisites.is_empty() {
            let active = self.active.as_mut()?;
            active.workspace_edits = None;
            active.state = FileOperationState::AwaitingWill;
            return Some(FileOperationWorkspaceEditAction::Mutate);
        }

        let mut parent = self.active.take()?;
        let pending = parent
            .workspace_edits
            .take()
            .expect("workspace edit state should be present");
        parent.state = FileOperationState::AwaitingPrerequisite;
        let prerequisites = pending
            .prerequisites
            .into_iter()
            .map(|request| QueuedFileOperation {
                id: self.next_id(),
                request,
            })
            .collect();
        self.dependencies.push(FileOperationDependencyFrame {
            parent,
            prerequisites,
            resume: DependencyResume::MutateParent,
        });
        Some(FileOperationWorkspaceEditAction::Drive)
    }

    pub(crate) fn finish(
        &mut self,
        outcome: FileOperationOutcome,
    ) -> Option<Vec<FileOperationCompletion>> {
        let active = self.active.take()?;
        if active.id != outcome.id || active.state != FileOperationState::Mutating {
            self.active = Some(active);
            return None;
        }
        let result = outcome.result;
        let record = outcome.record;
        self.store_record(&active, &result, record);
        let mut completions = vec![FileOperationCompletion {
            id: active.id,
            request: active.request,
            result,
        }];

        if self
            .dependencies
            .last()
            .is_some_and(|dependency| dependency.resume == DependencyResume::MutateParent)
        {
            if let Some(error) = completions[0].result.as_ref().err() {
                if let Some(completion) = self.fail_dependency_parent(completions[0].id, error) {
                    completions.push(completion);
                }
            }
        }

        Some(completions)
    }

    fn store_record(
        &mut self,
        active: &ActiveFileOperation,
        result: &Result<FileOperationApplied, FileOperationError>,
        record: Option<FileOperationRecord>,
    ) {
        match (&active.request.operation, &result) {
            (FileOperation::Undo, Ok(_)) => self.redo.extend(record),
            (FileOperation::Redo, Ok(_)) => self.undo.extend(record),
            (FileOperation::Undo, Err(_)) => self.undo.extend(record),
            (FileOperation::Redo, Err(_)) => self.redo.extend(record),
            (_, Ok(_)) => {
                if let Some(record) = record {
                    self.undo.push(record);
                    self.redo.clear();
                }
            }
            (_, Err(_)) => {}
        }
    }

    fn restore_replay_record(&mut self, active: &mut ActiveFileOperation) {
        let record = active.record.take();
        match active.request.operation {
            FileOperation::Undo => self.undo.extend(record),
            FileOperation::Redo => self.redo.extend(record),
            _ => {}
        }
    }

    fn fail_dependency_parent(
        &mut self,
        prerequisite: FileOperationId,
        error: &FileOperationError,
    ) -> Option<FileOperationCompletion> {
        let dependency = self.dependencies.last()?;
        if dependency.resume != DependencyResume::MutateParent {
            return None;
        }
        let mut dependency = self.dependencies.pop()?;
        self.restore_replay_record(&mut dependency.parent);
        Some(FileOperationCompletion {
            id: dependency.parent.id,
            request: dependency.parent.request,
            result: Err(FileOperationError::DependencyFailed {
                operation: prerequisite,
                message: error.to_string(),
            }),
        })
    }

    pub(crate) fn fail_preparation(
        &mut self,
        id: FileOperationId,
        error: FileOperationError,
    ) -> Option<Vec<FileOperationCompletion>> {
        let mut active = self.active.take()?;
        if active.id != id || active.state != FileOperationState::Inspecting {
            self.active = Some(active);
            return None;
        }
        self.restore_replay_record(&mut active);
        let mut completions = vec![FileOperationCompletion {
            id,
            request: active.request,
            result: Err(error),
        }];
        if self
            .dependencies
            .last()
            .is_some_and(|dependency| dependency.resume == DependencyResume::MutateParent)
        {
            if let Some(error) = completions[0].result.as_ref().err() {
                if let Some(completion) = self.fail_dependency_parent(completions[0].id, error) {
                    completions.push(completion);
                }
            }
        }
        Some(completions)
    }

    pub(crate) fn fail_waiting(
        &mut self,
        id: FileOperationId,
        error: FileOperationError,
    ) -> Option<FileOperationCompletion> {
        let mut active = self.active.take()?;
        if active.id != id
            || !matches!(
                active.state,
                FileOperationState::AwaitingWill | FileOperationState::AwaitingWorkspaceEdit
            )
        {
            self.active = Some(active);
            return None;
        }
        self.restore_replay_record(&mut active);
        Some(FileOperationCompletion {
            id,
            request: active.request,
            result: Err(error),
        })
    }

    #[cfg(test)]
    fn state(&self, id: FileOperationId) -> Option<FileOperationState> {
        self.active
            .as_ref()
            .filter(|active| active.id == id)
            .map(|active| active.state)
            .or_else(|| {
                self.queued
                    .iter()
                    .find(|queued| queued.id == id)
                    .map(|_| FileOperationState::Queued)
            })
            .or_else(|| {
                self.dependencies
                    .iter()
                    .find(|dependency| dependency.parent.id == id)
                    .map(|dependency| dependency.parent.state)
            })
    }
}

#[derive(Debug)]
pub struct FileOperationCompletion {
    pub id: FileOperationId,
    pub request: FileOperationRequest,
    pub result: Result<FileOperationApplied, FileOperationError>,
}

#[derive(Clone, Debug)]
struct FileOperationRecord {
    kind: FileOperationKind,
    history: FileOperationHistory,
}

#[derive(Clone, Debug)]
enum FileOperationHistory {
    Snapshots {
        before: Box<[PathImage]>,
        after: Box<[PathImage]>,
    },
    Move {
        before: MoveFingerprint,
        after: MoveFingerprint,
    },
}

/// A compact, content-sensitive record for reversible moves. Unlike a backup
/// snapshot, it does not retain a second compressed copy of every moved file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MoveFingerprint {
    source: FingerprintedPath,
    destination: FingerprintedPath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FingerprintedPath {
    path: PathBuf,
    state: FingerprintState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FingerprintState {
    Missing,
    Present(u64),
}

impl MoveFingerprint {
    fn capture(source: &Path, destination: &Path) -> Result<Self, FileOperationError> {
        Ok(Self {
            source: FingerprintedPath::capture(source)?,
            destination: FingerprintedPath::capture(destination)?,
        })
    }

    fn validate(&self) -> Result<(), FileOperationError> {
        self.source.validate()?;
        self.destination.validate()
    }
}

impl FingerprintedPath {
    fn capture(path: &Path) -> Result<Self, FileOperationError> {
        let path = canonicalize(path);
        Ok(Self {
            state: FingerprintState::capture(&path)?,
            path,
        })
    }

    fn validate(&self) -> Result<(), FileOperationError> {
        if FingerprintState::capture(&self.path)? == self.state {
            return Ok(());
        }
        Err(FileOperationError::Io {
            kind: ErrorKind::Other,
            message: format!(
                "refusing to modify {}; it changed after the file operation",
                self.path.display()
            ),
        })
    }
}

impl FingerprintState {
    fn capture(path: &Path) -> Result<Self, FileOperationError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(FileOperationError::Io {
                kind: ErrorKind::Unsupported,
                message: format!(
                    "file operation history does not support symlinks: {}",
                    path.display()
                ),
            }),
            Ok(metadata) if metadata.is_file() || metadata.is_dir() => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                fingerprint_entry(path, path, &mut hasher)?;
                Ok(Self::Present(hasher.finish()))
            }
            Ok(_) => Err(FileOperationError::Io {
                kind: ErrorKind::Unsupported,
                message: format!(
                    "file operation history does not support special files: {}",
                    path.display()
                ),
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::Missing),
            Err(error) => Err(error.into()),
        }
    }
}

fn fingerprint_entry(root: &Path, path: &Path, hasher: &mut impl Hasher) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            format!(
                "file operation history does not support symlinks: {}",
                path.display()
            ),
        ));
    }

    let relative_path = path.strip_prefix(root).unwrap_or_else(|_| Path::new(""));
    relative_path.hash(hasher);
    hasher.write_u8(metadata.is_dir().into());
    hasher.write_u64(metadata.len());
    hasher.write_u8(metadata.permissions().readonly().into());

    if metadata.is_file() {
        let mut file = fs::File::open(path)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.write(&buffer[..read]);
        }
        return Ok(());
    }

    if metadata.is_dir() {
        let mut children: Vec<_> = fs::read_dir(path)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            fingerprint_entry(root, &child.path(), hasher)?;
        }
        return Ok(());
    }

    Err(io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "file operation history does not support special files: {}",
            path.display()
        ),
    ))
}

fn move_path(from: &Path, to: &Path) -> Result<(), FileOperationError> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)?;
    Ok(())
}

#[derive(Clone, Debug)]
enum FileOperationKind {
    Create {
        path: PathBuf,
        is_dir: bool,
    },
    Copy {
        destination: PathBuf,
    },
    Move {
        source: PathBuf,
        destination: PathBuf,
        is_dir: bool,
    },
    Delete {
        path: PathBuf,
        is_dir: bool,
    },
}

impl FileOperationRecord {
    fn undo(&self) -> Result<(), FileOperationError> {
        match &self.history {
            FileOperationHistory::Snapshots { before, after } => {
                self.validate(after)?;
                apply_images(before)?;
                Ok(())
            }
            FileOperationHistory::Move { after, .. } => {
                after.validate()?;
                move_path(&after.destination.path, &after.source.path)
            }
        }
    }

    fn redo(&self) -> Result<(), FileOperationError> {
        match &self.history {
            FileOperationHistory::Snapshots { before, after } => {
                self.validate(before)?;
                apply_images(after)?;
                Ok(())
            }
            FileOperationHistory::Move { before, .. } => {
                before.validate()?;
                move_path(&before.source.path, &before.destination.path)
            }
        }
    }

    fn validate(&self, expected: &[PathImage]) -> Result<(), FileOperationError> {
        for image in expected {
            if !image.state.matches_path(&image.path)? {
                return Err(FileOperationError::Io {
                    kind: ErrorKind::Other,
                    message: format!(
                        "refusing to modify {}; it changed after the file operation",
                        image.path.display()
                    ),
                });
            }
        }
        Ok(())
    }

    fn paths(&self) -> Box<dyn Iterator<Item = &Path> + '_> {
        match &self.history {
            FileOperationHistory::Snapshots { before, after } => Box::new(
                before
                    .iter()
                    .chain(after.iter())
                    .map(|image| image.path.as_path()),
            ),
            FileOperationHistory::Move { before, after } => Box::new(
                [
                    before.source.path.as_path(),
                    before.destination.path.as_path(),
                ]
                .into_iter()
                .chain(
                    [
                        after.source.path.as_path(),
                        after.destination.path.as_path(),
                    ]
                    .into_iter(),
                ),
            ),
        }
    }

    fn change(&self) -> FileOperationChange {
        match &self.kind {
            FileOperationKind::Create { path, is_dir } => FileOperationChange::Create {
                path: path.clone(),
                is_dir: *is_dir,
            },
            FileOperationKind::Copy { destination, .. } => FileOperationChange::Create {
                path: destination.clone(),
                is_dir: false,
            },
            FileOperationKind::Move {
                source,
                destination,
                is_dir,
            } => FileOperationChange::Move {
                from: source.clone(),
                to: destination.clone(),
                is_dir: *is_dir,
            },
            FileOperationKind::Delete { path, is_dir, .. } => FileOperationChange::Delete {
                path: path.clone(),
                is_dir: *is_dir,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PathImage {
    path: PathBuf,
    state: PathState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PathState {
    Missing,
    Present(BackupSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BackupSnapshot {
    entries: Box<[BackupEntry]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BackupEntry {
    Directory {
        relative_path: PathBuf,
    },
    File {
        relative_path: PathBuf,
        compressed_data: Box<[u8]>,
        readonly: bool,
    },
}

fn symlink_metadata_optional(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn resolve_destination(
    destination: FileOperationDestination,
    source: &Path,
) -> Result<PathBuf, FileOperationError> {
    match destination {
        FileOperationDestination::Exact(path) => Ok(canonicalize(&path)),
        FileOperationDestination::PathOrDirectory(path) => {
            let path = canonicalize(&path);
            if symlink_metadata_optional(&path)?.is_some_and(|metadata| metadata.is_dir()) {
                let name = source.file_name().ok_or_else(|| FileOperationError::Io {
                    kind: ErrorKind::InvalidInput,
                    message: format!("path has no file name: {}", source.display()),
                })?;
                Ok(path.join(name))
            } else {
                Ok(path)
            }
        }
        FileOperationDestination::UniqueInDirectory(directory) => {
            unique_destination(&directory, source)
        }
    }
}

fn unique_destination(directory: &Path, source: &Path) -> Result<PathBuf, FileOperationError> {
    let directory = canonicalize(directory);
    let name = source.file_name().ok_or_else(|| FileOperationError::Io {
        kind: ErrorKind::InvalidInput,
        message: format!("path has no file name: {}", source.display()),
    })?;
    let candidate = directory.join(name);
    if symlink_metadata_optional(&candidate)?.is_none() {
        return Ok(candidate);
    }
    let stem = source.file_stem().unwrap_or(name).to_string_lossy();
    let extension = source
        .extension()
        .map(|extension| extension.to_string_lossy());
    for index in 1_u64.. {
        let mut file_name = format!("{stem} ({index})");
        if let Some(extension) = &extension {
            file_name.push('.');
            file_name.push_str(extension);
        }
        let candidate = directory.join(file_name);
        if symlink_metadata_optional(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    unreachable!("unbounded unique destination loop")
}

fn capture_images(paths: impl IntoIterator<Item = PathBuf>) -> io::Result<Box<[PathImage]>> {
    let mut seen = BTreeSet::new();
    let mut images = Vec::new();
    for path in paths {
        let path = canonicalize(&path);
        if seen.insert(path.clone()) {
            images.push(PathImage::capture(path)?);
        }
    }
    Ok(images.into_boxed_slice())
}

impl PathImage {
    fn capture(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = canonicalize(path.as_ref());
        Ok(Self {
            state: PathState::capture(&path)?,
            path,
        })
    }
}

impl PathState {
    fn capture(path: &Path) -> io::Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "file operation history does not support symlinks: {}",
                    path.display()
                ),
            )),
            Ok(metadata) if metadata.is_file() || metadata.is_dir() => {
                Ok(Self::Present(BackupSnapshot::capture(path)?))
            }
            Ok(_) => Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "file operation history does not support special files: {}",
                    path.display()
                ),
            )),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::Missing),
            Err(error) => Err(error),
        }
    }

    fn matches_path(&self, path: &Path) -> io::Result<bool> {
        Ok(Self::capture(path)? == *self)
    }
}

impl BackupSnapshot {
    fn capture(path: &Path) -> io::Result<Self> {
        let mut entries = Vec::new();
        capture_entry(path, path, &mut entries)?;
        entries.sort_by(|left, right| entry_relative_path(left).cmp(entry_relative_path(right)));
        Ok(Self {
            entries: entries.into_boxed_slice(),
        })
    }

    fn restore_to(&self, path: &Path) -> io::Result<()> {
        remove_path(path)?;
        let mut directories: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                BackupEntry::Directory { relative_path } => Some(relative_path),
                BackupEntry::File { .. } => None,
            })
            .collect();
        directories.sort_by_key(|path| path.components().count());
        for relative_path in directories {
            fs::create_dir_all(snapshot_target(path, relative_path))?;
        }
        let mut files: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                BackupEntry::File {
                    relative_path,
                    compressed_data,
                    readonly,
                } => Some((relative_path, compressed_data, *readonly)),
                BackupEntry::Directory { .. } => None,
            })
            .collect();
        files.sort_by_key(|(relative_path, ..)| *relative_path);
        for (relative_path, compressed_data, readonly) in files {
            let target = snapshot_target(path, relative_path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let data = zstd::stream::decode_all(&compressed_data[..])?;
            fs::write(&target, data)?;
            set_readonly(&target, readonly)?;
        }
        Ok(())
    }
}

fn capture_entry(root: &Path, path: &Path, entries: &mut Vec<BackupEntry>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            format!(
                "file operation history does not support symlinks: {}",
                path.display()
            ),
        ));
    }
    let relative_path = path
        .strip_prefix(root)
        .unwrap_or_else(|_| Path::new(""))
        .to_path_buf();
    if metadata.is_dir() {
        entries.push(BackupEntry::Directory { relative_path });
        let mut children = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(|entry| entry.path());
        for child in children {
            capture_entry(root, &child.path(), entries)?;
        }
        return Ok(());
    }
    if metadata.is_file() {
        let data = fs::read(path)?;
        entries.push(BackupEntry::File {
            relative_path,
            compressed_data: zstd::stream::encode_all(&data[..], 0)?.into_boxed_slice(),
            readonly: metadata.permissions().readonly(),
        });
        return Ok(());
    }
    Err(io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "file operation history does not support special files: {}",
            path.display()
        ),
    ))
}

fn entry_relative_path(entry: &BackupEntry) -> &Path {
    match entry {
        BackupEntry::Directory { relative_path } | BackupEntry::File { relative_path, .. } => {
            relative_path
        }
    }
}

fn snapshot_target(root: &Path, relative_path: &Path) -> PathBuf {
    if relative_path.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative_path)
    }
}

fn apply_images(images: &[PathImage]) -> io::Result<()> {
    let mut missing: Vec<_> = images
        .iter()
        .filter(|image| matches!(image.state, PathState::Missing))
        .collect();
    missing.sort_by_key(|image| std::cmp::Reverse(image.path.components().count()));
    for image in missing {
        remove_path(&image.path)?;
    }
    let mut present: Vec<_> = images
        .iter()
        .filter_map(|image| match &image.state {
            PathState::Present(snapshot) => Some((&image.path, snapshot)),
            PathState::Missing => None,
        })
        .collect();
    present.sort_by_key(|(path, _)| path.components().count());
    for (path, snapshot) in present {
        snapshot.restore_to(path)?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            make_writable_recursive(path)?;
            fs::remove_dir_all(path)
        }
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            set_readonly(path, false)?;
            fs::remove_file(path)
        }
        Ok(_) => Err(io::Error::new(
            ErrorKind::Unsupported,
            format!(
                "file operation history does not support special files: {}",
                path.display()
            ),
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn make_writable_recursive(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() || metadata.file_type().is_symlink() {
        return set_readonly(path, false);
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    for child in fs::read_dir(path)? {
        make_writable_recursive(&child?.path())?;
    }
    set_readonly(path, false)
}

#[cfg(windows)]
fn set_readonly(path: &Path, readonly: bool) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(readonly);
    fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn set_readonly(path: &Path, readonly: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    permissions.set_mode(if readonly {
        mode & !0o222
    } else {
        mode | 0o200
    });
    fs::set_permissions(path, permissions)
}

impl super::Editor {
    pub fn enqueue_file_operation(&mut self, request: FileOperationRequest) -> FileOperationId {
        self.file_operations.enqueue(request)
    }

    pub fn start_workspace_edit_execution(
        &mut self,
        plan: WorkspaceEditPlan,
        continuation: Option<WorkspaceEditContinuation>,
        parent: Option<FileOperationId>,
    ) -> WorkspaceEditExecutionUpdate {
        if let Err(error) = self.validate_workspace_edit_plan(&plan) {
            return WorkspaceEditExecutionUpdate {
                dispatch: WorkspaceEditExecutionDispatch::Complete(WorkspaceEditBatchCompletion {
                    continuation,
                    result: Err(WorkspaceEditBatchError {
                        message: error.kind.to_string(),
                        failed_change_idx: Some(error.failed_change_idx),
                    }),
                }),
                parent_completion: None,
            };
        }
        let batch_id = self.file_operations.begin_workspace_edit_execution(
            plan.into_execution(),
            continuation,
            parent,
        );
        self.advance_workspace_edit_execution_batch(batch_id)
    }

    pub fn resume_workspace_edit_execution(
        &mut self,
        completion: &FileOperationCompletion,
    ) -> Option<WorkspaceEditExecutionUpdate> {
        let (batch_id, member, mut batch) = self
            .file_operations
            .take_workspace_edit_batch_for_completion(completion)?;
        let failed_change_idx = match &completion.request.origin {
            FileOperationOrigin::WorkspaceEdit {
                failed_change_idx, ..
            } => *failed_change_idx,
            _ => return None,
        };
        if let Err(error) = &completion.result {
            let parent_completion = batch.parent.and_then(|parent| {
                self.file_operations
                    .abort_workspace_edit_parent(parent, batch_id, member, error)
            });
            return Some(WorkspaceEditExecutionUpdate {
                dispatch: WorkspaceEditExecutionDispatch::Complete(WorkspaceEditBatchCompletion {
                    continuation: batch.continuation.take(),
                    result: Err(WorkspaceEditBatchError {
                        message: error.to_string(),
                        failed_change_idx,
                    }),
                }),
                parent_completion,
            });
        }

        if let Some(parent) = batch.parent {
            if !self
                .file_operations
                .resume_workspace_edit_parent(parent, batch_id, member)
            {
                return Some(WorkspaceEditExecutionUpdate {
                    dispatch: WorkspaceEditExecutionDispatch::Complete(
                        WorkspaceEditBatchCompletion {
                            continuation: batch.continuation.take(),
                            result: Err(WorkspaceEditBatchError {
                                message: "workspace edit parent is no longer waiting".to_owned(),
                                failed_change_idx,
                            }),
                        },
                    ),
                    parent_completion: None,
                });
            }
        }

        batch.waiting_member = None;
        self.file_operations
            .workspace_edit_batches
            .insert(batch_id, batch);
        Some(WorkspaceEditExecutionUpdate {
            dispatch: WorkspaceEditExecutionDispatch::Advance(batch_id),
            parent_completion: None,
        })
    }

    pub fn advance_workspace_edit_execution_batch(
        &mut self,
        batch_id: WorkspaceEditBatchId,
    ) -> WorkspaceEditExecutionUpdate {
        let Some(mut batch) = self.file_operations.take_workspace_edit_batch(batch_id) else {
            return WorkspaceEditExecutionUpdate {
                dispatch: WorkspaceEditExecutionDispatch::Complete(WorkspaceEditBatchCompletion {
                    continuation: None,
                    result: Err(WorkspaceEditBatchError {
                        message: "workspace edit execution is no longer active".to_owned(),
                        failed_change_idx: None,
                    }),
                }),
                parent_completion: None,
            };
        };

        match self.advance_workspace_edit_execution(&mut batch.execution) {
            Ok(WorkspaceEditExecutionStep::Complete) => WorkspaceEditExecutionUpdate {
                dispatch: WorkspaceEditExecutionDispatch::Complete(WorkspaceEditBatchCompletion {
                    continuation: batch.continuation.take(),
                    result: Ok(()),
                }),
                parent_completion: None,
            },
            Ok(WorkspaceEditExecutionStep::Resource {
                member,
                failed_change_idx,
                mut request,
            }) => {
                let FileOperationOrigin::WorkspaceEdit {
                    batch: request_batch,
                    member: request_member,
                    failed_change_idx: request_change_idx,
                } = &mut request.origin
                else {
                    return WorkspaceEditExecutionUpdate {
                        dispatch: WorkspaceEditExecutionDispatch::Complete(
                            WorkspaceEditBatchCompletion {
                                continuation: batch.continuation.take(),
                                result: Err(WorkspaceEditBatchError {
                                    message: "workspace edit resource has an invalid origin"
                                        .to_owned(),
                                    failed_change_idx: Some(failed_change_idx),
                                }),
                            },
                        ),
                        parent_completion: None,
                    };
                };
                *request_batch = Some(batch_id);
                *request_member = Some(member);
                *request_change_idx = Some(failed_change_idx);
                batch.waiting_member = Some(member);
                let parent = batch.parent;
                self.file_operations
                    .workspace_edit_batches
                    .insert(batch_id, batch);

                let dispatch = match parent {
                    Some(parent) => {
                        if self
                            .file_operations
                            .suspend_for_workspace_edit_resource(parent, batch_id, member, request)
                        {
                            WorkspaceEditExecutionDispatch::Drive
                        } else {
                            let batch = self
                                .file_operations
                                .workspace_edit_batches
                                .remove(&batch_id)
                                .expect("workspace edit batch should still be present");
                            return WorkspaceEditExecutionUpdate {
                                dispatch: WorkspaceEditExecutionDispatch::Complete(
                                    WorkspaceEditBatchCompletion {
                                        continuation: batch.continuation,
                                        result: Err(WorkspaceEditBatchError {
                                            message: "workspace edit parent is no longer waiting"
                                                .to_owned(),
                                            failed_change_idx: Some(failed_change_idx),
                                        }),
                                    },
                                ),
                                parent_completion: None,
                            };
                        }
                    }
                    None => WorkspaceEditExecutionDispatch::EnqueueResource(request),
                };
                WorkspaceEditExecutionUpdate {
                    dispatch,
                    parent_completion: None,
                }
            }
            Err(error) => WorkspaceEditExecutionUpdate {
                dispatch: WorkspaceEditExecutionDispatch::Complete(WorkspaceEditBatchCompletion {
                    continuation: batch.continuation.take(),
                    result: Err(WorkspaceEditBatchError {
                        message: error.kind.to_string(),
                        failed_change_idx: Some(error.failed_change_idx),
                    }),
                }),
                parent_completion: None,
            },
        }
    }

    pub fn next_file_operation_dispatch(&mut self) -> Option<FileOperationDispatch> {
        self.file_operations.next_dispatch()
    }

    pub fn accept_file_operation_preparation(
        &mut self,
        prepared: FileOperationPrepared,
    ) -> Option<(FileOperationRequest, FileOperationPrepared)> {
        self.file_operations.accept_prepared(prepared)
    }

    pub fn begin_file_operation_mutation(
        &mut self,
        id: FileOperationId,
    ) -> Option<FileOperationWork> {
        self.file_operations.begin_mutation(id)
    }

    pub fn file_operation_accepts_will_completion(&self, id: FileOperationId) -> bool {
        self.file_operations.accepts_will_completion(id)
    }

    pub fn begin_file_operation_workspace_edits(
        &mut self,
        id: FileOperationId,
        edits: Vec<(OffsetEncoding, lsp::WorkspaceEdit)>,
    ) -> bool {
        self.file_operations.begin_workspace_edits(id, edits)
    }

    pub fn next_file_operation_workspace_edit(
        &mut self,
        id: FileOperationId,
    ) -> Option<(OffsetEncoding, lsp::WorkspaceEdit)> {
        self.file_operations.next_workspace_edit(id)
    }

    pub fn file_operation_accepts_workspace_edit_preparation(
        &mut self,
        id: FileOperationId,
    ) -> bool {
        self.file_operations.accepts_workspace_edit_preparation(id)
    }

    pub fn add_file_operation_workspace_edit_prerequisites(
        &mut self,
        id: FileOperationId,
        requests: impl IntoIterator<Item = FileOperationRequest>,
    ) -> bool {
        self.file_operations
            .add_workspace_edit_prerequisites(id, requests)
    }

    pub fn finish_file_operation_workspace_edits(
        &mut self,
        id: FileOperationId,
    ) -> Option<FileOperationWorkspaceEditAction> {
        self.file_operations.finish_workspace_edits(id)
    }

    pub fn finish_file_operation(
        &mut self,
        outcome: FileOperationOutcome,
    ) -> Option<Vec<FileOperationCompletion>> {
        self.file_operations.finish(outcome)
    }

    pub fn fail_file_operation_preparation(
        &mut self,
        id: FileOperationId,
        error: FileOperationError,
    ) -> Option<Vec<FileOperationCompletion>> {
        self.file_operations.fail_preparation(id, error)
    }

    pub fn fail_file_operation_waiting(
        &mut self,
        id: FileOperationId,
        error: FileOperationError,
    ) -> Option<FileOperationCompletion> {
        self.file_operations.fail_waiting(id, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_journal_operation(
        journal: &mut FileOperationJournal,
        request: FileOperationRequest,
    ) -> FileOperationCompletion {
        let id = journal.enqueue(request);
        let FileOperationDispatch::Inspect(inspection) = journal
            .next_dispatch()
            .expect("operation should start inspection")
        else {
            panic!("operation should start with inspection");
        };
        assert_eq!(inspection.id(), id);
        let prepared = inspection.execute().expect("inspection should complete");
        journal
            .accept_prepared(prepared)
            .expect("prepared operation should be accepted");
        let work = journal.begin_mutation(id).expect("mutation should start");
        journal
            .finish(work.execute())
            .expect("completion should be accepted")
            .into_iter()
            .next()
            .expect("completion should be present")
    }

    #[test]
    fn journal_keeps_multi_level_undo_and_redo_history() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.txt");
        let second = temp.path().join("second.txt");
        let mut journal = FileOperationJournal::default();

        for path in [&first, &second] {
            let completion = run_journal_operation(
                &mut journal,
                FileOperationRequest::create(FileOperationOrigin::Command, path.clone(), false),
            );
            assert!(completion.result.is_ok());
        }
        assert_eq!(journal.undo.len(), 2);
        assert!(journal.redo.is_empty());

        for path in [&second, &first] {
            let completion = run_journal_operation(
                &mut journal,
                FileOperationRequest::undo(FileOperationOrigin::Command),
            );
            assert!(completion.result.is_ok());
            assert!(!path.exists());
        }
        assert!(journal.undo.is_empty());
        assert_eq!(journal.redo.len(), 2);

        for path in [&first, &second] {
            let completion = run_journal_operation(
                &mut journal,
                FileOperationRequest::redo(FileOperationOrigin::Command),
            );
            assert!(completion.result.is_ok());
            assert!(path.exists());
        }
        assert_eq!(journal.undo.len(), 2);
        assert!(journal.redo.is_empty());
    }

    #[test]
    fn failed_replay_returns_record_to_its_history_stack() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("changed.txt");
        let mut journal = FileOperationJournal::default();
        let completion = run_journal_operation(
            &mut journal,
            FileOperationRequest::create(FileOperationOrigin::Command, path.clone(), false),
        );
        assert!(completion.result.is_ok());
        fs::write(&path, "external change").unwrap();

        let completion = run_journal_operation(
            &mut journal,
            FileOperationRequest::undo(FileOperationOrigin::Command),
        );
        assert!(completion.result.is_err());
        assert_eq!(journal.undo.len(), 1);
        assert!(journal.redo.is_empty());
        assert_eq!(fs::read_to_string(path).unwrap(), "external change");
    }

    #[test]
    fn new_mutation_after_undo_discards_redo_branch_only() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.txt");
        let replacement = temp.path().join("replacement.txt");
        let mut journal = FileOperationJournal::default();
        assert!(run_journal_operation(
            &mut journal,
            FileOperationRequest::create(FileOperationOrigin::Command, first, false),
        )
        .result
        .is_ok());
        assert!(run_journal_operation(
            &mut journal,
            FileOperationRequest::undo(FileOperationOrigin::Command),
        )
        .result
        .is_ok());
        assert_eq!(journal.redo.len(), 1);

        assert!(run_journal_operation(
            &mut journal,
            FileOperationRequest::create(FileOperationOrigin::Command, replacement, false),
        )
        .result
        .is_ok());
        assert_eq!(journal.undo.len(), 1);
        assert!(journal.redo.is_empty());
    }

    #[test]
    fn directory_copy_is_a_single_typed_unsupported_result() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let inspection = FileOperationInspection {
            id: FileOperationId(1),
            operation: FileOperation::Copy {
                source: source.clone(),
                destination: FileOperationDestination::Exact(temp.path().join("copy")),
                overwrite: true,
                create_parents: true,
            },
            replay_change: None,
        };
        assert_eq!(
            inspection.execute().unwrap_err(),
            FileOperationError::UnsupportedDirectoryCopy { path: source }
        );
    }

    #[test]
    fn unique_destination_is_resolved_off_thread_work() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("file.txt");
        fs::write(&source, "source").unwrap();
        let destination = temp.path().join("destination");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("file.txt"), "existing").unwrap();
        let resolved = unique_destination(&destination, &source).unwrap();
        assert_eq!(resolved.file_name().unwrap(), "file (1).txt");
    }

    #[test]
    fn ignored_workspace_rename_is_a_typed_noop() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("destination.txt");
        fs::write(&source, "source").unwrap();
        fs::write(&destination, "destination").unwrap();
        let prepared = FileOperationInspection {
            id: FileOperationId(1),
            operation: FileOperation::Move {
                source: source.clone(),
                destination: FileOperationDestination::Exact(destination.clone()),
                overwrite: false,
                ignore_if_exists: true,
                create_parents: true,
            },
            replay_change: None,
        }
        .execute()
        .unwrap();

        assert!(prepared.will_change().is_none());
        let outcome = FileOperationWork {
            id: FileOperationId(1),
            operation: prepared.operation,
            record_history: false,
            record: None,
        }
        .execute();
        assert!(outcome.result.unwrap().changes.is_empty());
        assert_eq!(fs::read_to_string(source).unwrap(), "source");
        assert_eq!(fs::read_to_string(destination).unwrap(), "destination");
    }

    #[test]
    fn journal_rejects_stale_completion_and_keeps_fifo_order() {
        let mut journal = FileOperationJournal::default();
        let first = journal.enqueue(FileOperationRequest::create(
            FileOperationOrigin::Command,
            PathBuf::from("one"),
            false,
        ));
        let second = journal.enqueue(FileOperationRequest::create(
            FileOperationOrigin::Command,
            PathBuf::from("two"),
            false,
        ));
        let FileOperationDispatch::Inspect(inspection) = journal.next_dispatch().unwrap() else {
            panic!("expected inspection");
        };
        assert_eq!(inspection.id, first);
        assert_eq!(journal.state(second), Some(FileOperationState::Queued));
        let prepared = inspection.execute().unwrap();
        journal.accept_prepared(prepared.clone()).unwrap();
        let work = journal.begin_mutation(first).unwrap();
        let stale = FileOperationOutcome {
            id: second,
            result: Err(FileOperationError::NoHistory),
            record: None,
        };
        assert!(journal.finish(stale).is_none());
        assert_eq!(journal.state(first), Some(FileOperationState::Mutating));
        let _ = journal.finish(work.execute()).unwrap();
        let FileOperationDispatch::Inspect(inspection) = journal.next_dispatch().unwrap() else {
            panic!("expected second inspection");
        };
        assert_eq!(inspection.id, second);
    }

    #[test]
    fn workspace_edit_prerequisites_mutate_before_their_parent_and_reject_stale_completion() {
        let temp = tempfile::tempdir().unwrap();
        let prerequisite_path = temp.path().join("prerequisite");
        let parent_path = temp.path().join("parent");
        let mut journal = FileOperationJournal::default();
        let parent = journal.enqueue(FileOperationRequest::create(
            FileOperationOrigin::Command,
            parent_path.clone(),
            false,
        ));

        let FileOperationDispatch::Inspect(inspection) = journal.next_dispatch().unwrap() else {
            panic!("expected parent inspection");
        };
        let prepared = inspection.execute().unwrap();
        journal.accept_prepared(prepared).unwrap();
        assert!(journal.begin_workspace_edits(parent, Vec::new()));
        assert!(journal.add_workspace_edit_prerequisites(
            parent,
            [FileOperationRequest::create(
                FileOperationOrigin::workspace_edit(),
                prerequisite_path.clone(),
                false,
            )],
        ));
        assert_eq!(
            journal.finish_workspace_edits(parent),
            Some(FileOperationWorkspaceEditAction::Drive)
        );

        let FileOperationDispatch::Inspect(inspection) = journal.next_dispatch().unwrap() else {
            panic!("expected prerequisite inspection");
        };
        let prerequisite = inspection.id;
        let prepared = inspection.execute().unwrap();
        journal.accept_prepared(prepared).unwrap();
        let work = journal.begin_mutation(prerequisite).unwrap();

        assert!(journal
            .finish(FileOperationOutcome {
                id: parent,
                result: Err(FileOperationError::NoHistory),
                record: None,
            })
            .is_none());
        assert_eq!(
            journal.state(prerequisite),
            Some(FileOperationState::Mutating)
        );

        journal.finish(work.execute()).unwrap();
        assert!(prerequisite_path.exists());
        assert!(!parent_path.exists());
        let FileOperationDispatch::Mutate(work) = journal.next_dispatch().unwrap() else {
            panic!("expected resumed parent mutation");
        };
        journal.finish(work.execute()).unwrap();
        assert!(parent_path.exists());
    }

    #[test]
    fn workspace_edit_failure_completes_parent_and_keeps_fifo_live() {
        let mut journal = FileOperationJournal::default();
        let parent = journal.enqueue(FileOperationRequest::create(
            FileOperationOrigin::Command,
            PathBuf::from("parent"),
            false,
        ));
        let next = journal.enqueue(FileOperationRequest::create(
            FileOperationOrigin::Command,
            PathBuf::from("next"),
            false,
        ));
        let FileOperationDispatch::Inspect(inspection) = journal.next_dispatch().unwrap() else {
            panic!("expected parent inspection");
        };
        journal
            .accept_prepared(inspection.execute().unwrap())
            .unwrap();
        assert!(journal.begin_workspace_edits(parent, Vec::new()));

        let completion = journal
            .fail_waiting(
                parent,
                FileOperationError::WorkspaceEdit {
                    message: "document changed".to_owned(),
                },
            )
            .unwrap();
        assert!(matches!(
            completion.result,
            Err(FileOperationError::WorkspaceEdit { .. })
        ));
        let FileOperationDispatch::Inspect(inspection) = journal.next_dispatch().unwrap() else {
            panic!("expected next FIFO inspection");
        };
        assert_eq!(inspection.id, next);
    }

    #[test]
    fn prerequisite_preparation_failure_aborts_parent_and_keeps_fifo_live() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("directory");
        fs::create_dir_all(&directory).unwrap();
        let mut journal = FileOperationJournal::default();
        let parent = journal.enqueue(FileOperationRequest::create(
            FileOperationOrigin::Command,
            temp.path().join("parent"),
            false,
        ));
        let next = journal.enqueue(FileOperationRequest::create(
            FileOperationOrigin::Command,
            temp.path().join("next"),
            false,
        ));

        let FileOperationDispatch::Inspect(inspection) = journal.next_dispatch().unwrap() else {
            panic!("expected parent inspection");
        };
        journal
            .accept_prepared(inspection.execute().unwrap())
            .unwrap();
        assert!(journal.begin_workspace_edits(parent, Vec::new()));
        assert!(journal.add_workspace_edit_prerequisites(
            parent,
            [FileOperationRequest::copy_path(
                FileOperationOrigin::workspace_edit(),
                directory,
                FileOperationDestination::Exact(temp.path().join("copy")),
            )],
        ));
        assert_eq!(
            journal.finish_workspace_edits(parent),
            Some(FileOperationWorkspaceEditAction::Drive)
        );

        let FileOperationDispatch::Inspect(inspection) = journal.next_dispatch().unwrap() else {
            panic!("expected prerequisite inspection");
        };
        let prerequisite = inspection.id;
        let error = inspection.execute().unwrap_err();
        assert!(matches!(
            error,
            FileOperationError::UnsupportedDirectoryCopy { .. }
        ));
        let completions = journal.fail_preparation(prerequisite, error).unwrap();
        assert!(matches!(
            completions.as_slice(),
            [
                FileOperationCompletion {
                    result: Err(FileOperationError::UnsupportedDirectoryCopy { .. }),
                    ..
                },
                FileOperationCompletion {
                    id,
                    result: Err(FileOperationError::DependencyFailed { operation, .. }),
                    ..
                }
            ] if *id == parent && *operation == prerequisite
        ));

        let FileOperationDispatch::Inspect(inspection) = journal.next_dispatch().unwrap() else {
            panic!("expected next FIFO inspection");
        };
        assert_eq!(inspection.id, next);
        assert_eq!(journal.state(parent), None);
    }

    #[test]
    fn work_captures_and_replays_a_file_without_the_main_loop() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        let prepared = FileOperationInspection {
            id: FileOperationId(1),
            operation: FileOperation::Create {
                path: path.clone(),
                is_dir: false,
                overwrite: true,
                ignore_if_exists: false,
            },
            replay_change: None,
        }
        .execute()
        .unwrap();
        let outcome = FileOperationWork {
            id: FileOperationId(1),
            operation: prepared.operation,
            record_history: true,
            record: None,
        }
        .execute();
        assert!(path.exists());
        let record = outcome.record.unwrap();
        let replay = FileOperationWork {
            id: FileOperationId(2),
            operation: PreparedFileOperation::Replay {
                direction: ReplayDirection::Undo,
                change: record.change(),
            },
            record_history: false,
            record: Some(record),
        }
        .execute();
        assert!(replay.result.is_ok());
        assert!(!path.exists());
    }

    #[test]
    fn directory_move_history_uses_fingerprint_and_rejects_nested_stale_changes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let nested = source.join("nested.txt");
        fs::create_dir_all(&source).unwrap();
        fs::write(&nested, "before").unwrap();

        let prepared = FileOperationInspection {
            id: FileOperationId(1),
            operation: FileOperation::Move {
                source: source.clone(),
                destination: FileOperationDestination::Exact(destination.clone()),
                overwrite: false,
                ignore_if_exists: false,
                create_parents: true,
            },
            replay_change: None,
        }
        .execute()
        .unwrap();
        let outcome = FileOperationWork {
            id: FileOperationId(1),
            operation: prepared.operation,
            record_history: true,
            record: None,
        }
        .execute();
        let record = outcome.record.unwrap();
        assert!(matches!(record.history, FileOperationHistory::Move { .. }));
        assert!(destination.join("nested.txt").exists());

        record.undo().unwrap();
        assert!(nested.exists());
        record.redo().unwrap();
        fs::write(destination.join("nested.txt"), "changed").unwrap();
        assert!(record.undo().is_err());
    }
}
