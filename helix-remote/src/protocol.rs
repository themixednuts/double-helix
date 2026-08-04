use crate::WorkspacePath;
pub use helix_workspace::{
    ContentSearchCursor, ContentSearchEntry, ContentSearchPage, ContentSearchQuery,
    DirectoryOptions, FileChange, FileChangeKind, FileOperation, FileTransaction, ScanOptions,
    MAX_TRANSACTION_HISTORY, MAX_TRANSACTION_OPERATIONS,
};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: u16 = 8;
pub const MAX_REMOTE_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_FILE_CHUNK_BYTES: u32 = 1024 * 1024;
pub const MAX_SEARCH_BATCH: u16 = 256;
pub const MAX_IN_FLIGHT_REQUESTS: u16 = 256;
pub const MAX_ACTIVE_SEARCHES: usize = 8;
pub const MAX_PENDING_WRITES: usize = 16;
pub const MAX_ACTIVE_PROCESSES: usize = 32;
pub const MAX_ACTIVE_WATCHES: usize = 32;
pub const MAX_REQUESTED_CAPABILITIES: usize = 32;
pub const MAX_PROCESS_ARGUMENTS: usize = 512;
pub const MAX_PROCESS_ENVIRONMENT: usize = 512;
pub const MAX_PROCESS_SPEC_BYTES: usize = 1024 * 1024;
pub const MAX_PROCESS_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_WRITE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const MAX_CLIENT_VERSION_BYTES: usize = 512;
pub const MAX_WORKSPACE_ROOT_BYTES: usize = 64 * 1024;
pub const MAX_SEARCH_QUERY_BYTES: usize = 16 * 1024;
pub const MAX_LANGUAGE_SERVER_ROOT_PATTERNS: usize = 256;
pub const MAX_LANGUAGE_SERVER_ROOT_PATTERN_BYTES: usize = 64 * 1024;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);
    };
}

id_type!(RequestId);
id_type!(SessionId);
id_type!(OperationId);
id_type!(WriteId);
id_type!(ProcessId);
id_type!(WatchId);
id_type!(TransactionId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientFrame {
    Request {
        id: RequestId,
        request: ClientRequest,
    },
    Cancel {
        id: RequestId,
    },
    ProcessInput {
        process: ProcessId,
        bytes: ByteBuf,
    },
    CloseProcessInput {
        process: ProcessId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerFrame {
    Response {
        id: RequestId,
        result: Result<ServerResponse, RemoteError>,
    },
    Event(ServerEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientRequest {
    Hello(ClientHello),
    OpenWorkspace(OpenWorkspace),
    CloseWorkspace,
    Stat {
        path: WorkspacePath,
    },
    ReadDir(ReadDir),
    ReadFile(ReadFile),
    BeginWrite(BeginWrite),
    WriteChunk(WriteChunk),
    CommitWrite {
        write: WriteId,
    },
    AbortWrite {
        write: WriteId,
    },
    SearchFiles(SearchFiles),
    SearchContent(ContentSearchQuery),
    CancelOperation {
        operation: OperationId,
    },
    Watch(Watch),
    Unwatch {
        watch: WatchId,
    },
    ApplyFileTransaction(FileTransaction),
    UndoFileTransaction {
        transaction: TransactionId,
    },
    ResolveLanguageServerWorkspace(ResolveLanguageServerWorkspace),
    StartProcess(ProcessSpec),
    ResizeProcess {
        process: ProcessId,
        size: TerminalSize,
    },
    KillProcess {
        process: ProcessId,
    },
    Ping {
        nonce: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerResponse {
    Hello(ServerHello),
    WorkspaceOpened(WorkspaceInfo),
    Unit,
    Metadata(Option<FileMetadata>),
    Directory(Vec<DirectoryEntry>),
    FileChunk(FileChunk),
    ContentSearch(ContentSearchPage),
    WriteStarted { write: WriteId },
    WriteCommitted { metadata: FileMetadata },
    FileTransactionApplied(FileTransactionReceipt),
    FileTransactionUndone,
    LanguageServerWorkspace(Option<LanguageServerWorkspace>),
    Pong { nonce: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerEvent {
    SearchBatch(SearchBatch),
    FileChanges(FileChanges),
    ProcessOutput(ProcessOutput),
    ProcessExited(ProcessExit),
    WorkspaceInvalidated { reason: String },
    Log(RemoteLog),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol: u16,
    pub client_version: String,
    pub requested: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol: u16,
    pub server_version: String,
    pub platform: Platform,
    pub capabilities: Vec<Capability>,
    pub limits: ProtocolLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    FileSystem,
    FileSearch,
    FileWatch,
    FileTransactions,
    Processes,
    Pseudoterminals,
    Vcs,
    Packages,
    Plugins,
    LanguageServers,
    DebugAdapters,
    PortForwarding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub family: String,
    pub path_separator: char,
    pub home: Option<String>,
    pub shell: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolLimits {
    pub max_frame_bytes: u32,
    pub max_file_chunk_bytes: u32,
    pub max_write_bytes: u64,
    pub max_search_batch: u16,
    pub max_in_flight_requests: u16,
    pub max_active_searches: u16,
    pub max_pending_writes: u16,
    pub max_active_processes: u16,
    pub max_active_watches: u16,
    pub max_transaction_operations: u16,
    pub max_process_spec_bytes: u32,
    pub max_process_input_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenWorkspace {
    /// Absolute path interpreted by the remote operating system.
    pub root: String,
    /// Stable identity chosen by the client for this logical workspace session.
    /// Reusing it does not imply that server-side ephemeral work survived a
    /// transport reconnect.
    pub session: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub session: SessionId,
    pub root: String,
    pub display_name: String,
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadDir {
    pub path: WorkspacePath,
    pub options: DirectoryOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFile {
    pub path: WorkspacePath,
    pub offset: u64,
    pub max_bytes: u32,
    pub expected: Option<ContentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunk {
    pub metadata: FileMetadata,
    pub offset: u64,
    pub bytes: ByteBuf,
    pub eof: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeginWrite {
    pub path: WorkspacePath,
    pub expected: Option<ContentId>,
    pub create_parents: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteChunk {
    pub write: WriteId,
    pub offset: u64,
    pub bytes: ByteBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchFiles {
    pub operation: OperationId,
    pub root: WorkspacePath,
    pub query: String,
    pub options: ScanOptions,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchBatch {
    pub operation: OperationId,
    /// A newer revision replaces all results from older revisions.
    pub revision: u64,
    /// Index of this chunk within the revision.
    pub batch: u32,
    pub entries: Vec<SearchEntry>,
    pub scanned: u64,
    /// True on the final chunk for this revision.
    pub revision_done: bool,
    /// True when the workspace scan has completed and no newer revision follows.
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchEntry {
    pub path: WorkspacePath,
    pub score: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub watch: WatchId,
    pub path: WorkspacePath,
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChanges {
    pub watch: WatchId,
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub kind: FileKind,
    pub len: u64,
    pub modified_unix_nanos: Option<u64>,
    pub readonly: bool,
    pub executable: bool,
    pub content: Option<ContentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub path: WorkspacePath,
    pub name: String,
    pub metadata: FileMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentId {
    pub len: u64,
    pub modified_unix_nanos: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTransactionReceipt {
    pub transaction: TransactionId,
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveLanguageServerWorkspace {
    pub document: WorkspacePath,
    pub root_markers: Vec<String>,
    pub root_dirs: Vec<WorkspacePath>,
    pub required_root_patterns: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageServerWorkspace {
    pub root: WorkspacePath,
    pub absolute_path: String,
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSpec {
    pub process: ProcessId,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: WorkspacePath,
    pub env: BTreeMap<String, String>,
    pub kind: ProcessKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessKind {
    Pipes,
    Pty { size: TerminalSize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessOutput {
    pub process: ProcessId,
    pub stream: ProcessStream,
    pub bytes: ByteBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessStream {
    Stdout,
    Stderr,
    Pty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessExit {
    pub process: ProcessId,
    pub code: Option<i32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteLog {
    pub level: RemoteLogLevel,
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteLogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct RemoteError {
    pub code: ErrorCode,
    pub message: String,
    pub path: Option<WorkspacePath>,
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    ProtocolMismatch,
    HandshakeRequired,
    CapabilityUnavailable,
    WorkspaceNotOpen,
    WorkspaceOutsideRoot,
    InvalidPath,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    Conflict,
    InvalidRequest,
    ResourceExhausted,
    Canceled,
    ProcessUnavailable,
    Io,
    Internal,
}

impl RemoteError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: None,
            retryable: false,
        }
    }

    pub fn at(mut self, path: WorkspacePath) -> Self {
        self.path = Some(path);
        self
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_ipc::FrameCodec;
    use std::io::Cursor;

    #[test]
    fn protocol_round_trip_preserves_binary_chunks() {
        let frame = ServerFrame::Response {
            id: RequestId(9),
            result: Ok(ServerResponse::FileChunk(FileChunk {
                metadata: FileMetadata {
                    kind: FileKind::File,
                    len: 4,
                    modified_unix_nanos: Some(7),
                    readonly: false,
                    executable: false,
                    content: Some(ContentId {
                        len: 4,
                        modified_unix_nanos: Some(7),
                    }),
                },
                offset: 0,
                bytes: ByteBuf::from(vec![0, 1, 2, 255]),
                eof: true,
            })),
        };
        let mut codec = FrameCodec::new();
        let mut bytes = Vec::new();
        codec.write_sync(&mut bytes, &frame).unwrap();
        let decoded: ServerFrame = codec.read_sync(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded, frame);
    }
}
