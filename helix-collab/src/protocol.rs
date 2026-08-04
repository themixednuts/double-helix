use crate::FileVersion;
use helix_workspace::{
    ContentSearchPage, ContentSearchQuery, FileTransaction, ScanOptions, WorkspacePath,
};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::{fmt, str::FromStr};

pub const PROTOCOL_VERSION: u16 = 14;
pub const MAX_PARTICIPANTS: usize = 64;
pub const MAX_INVITES: usize = 128;
pub const MAX_PARTICIPANT_NAME_BYTES: usize = 128;
pub const MAX_SYNC_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SYNC_MESSAGE_CHUNK_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_SYNC_MESSAGE_TRANSFERS_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_BUFFER_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BUFFER_SNAPSHOT_CHUNK_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_BUFFER_SNAPSHOT_TRANSFERS_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_TRANSPORT_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_COLLABORATIVE_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PROJECT_FILES: usize = 1_000_000;
pub const MAX_OPEN_BUFFERS: usize = 128;
pub const MAX_FILE_PAGE_ENTRIES: u16 = 128;
pub const MAX_FILE_TRANSACTION_OPERATIONS: usize = 64;
pub const MAX_WORKTREE_CHANGES_PER_EVENT: usize = 128;
pub const MAX_LANGUAGE_SERVER_NAME_BYTES: usize = 256;
pub const MAX_LANGUAGE_SERVER_METHOD_BYTES: usize = 256;
pub const MAX_LANGUAGE_SERVER_PAYLOAD_BYTES: usize = 3 * 1024 * 1024;
pub const LANGUAGE_SERVER_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

macro_rules! byte_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub [u8; 16]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_id(value).map(Self)
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("identifier must contain exactly 32 hexadecimal characters")]
pub struct IdParseError;

fn parse_id(value: &str) -> Result<[u8; 16], IdParseError> {
    if value.len() != 32 || !value.is_ascii() {
        return Err(IdParseError);
    }
    let mut bytes = [0; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_digit(pair[0]).ok_or(IdParseError)?;
        let low = hex_digit(pair[1]).ok_or(IdParseError)?;
        bytes[index] = (high << 4) | low;
    }
    Ok(bytes)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

byte_id!(SessionId);
byte_id!(ParticipantId);
byte_id!(ProjectId);
byte_id!(ViewId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BufferId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotTransferId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SyncTransferId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotContinuation {
    pub transfer: SnapshotTransferId,
    pub offset: u64,
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretToken(pub(crate) [u8; 32]);

impl SecretToken {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken([REDACTED])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    Observe,
    Read,
    Write,
    Owner,
}

impl Role {
    pub const fn allows(self, required: Self) -> bool {
        self as u8 >= required as u8
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Credential {
    Invite(SecretToken),
    Resume(SecretToken),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authenticate {
    pub protocol: u16,
    pub session: SessionId,
    pub credential: Credential,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientFrame {
    Authenticate(Authenticate),
    Request { id: u64, request: Request },
    Cancel { id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostFrame {
    Authenticated {
        participant: ParticipantInfo,
        resume: SecretToken,
        resume_expires_unix_secs: u64,
    },
    Rejected(ProtocolError),
    Response {
        id: u64,
        result: Result<Response, ProtocolError>,
    },
    Event(Event),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Unit,
    FileTransaction {
        changed: bool,
    },
    PathExists(bool),
    Project(ProjectInfo),
    ProjectState(ProjectState),
    Files {
        entries: Vec<WorkspacePath>,
        next: Option<WorkspacePath>,
    },
    ContentSearch(ContentSearchPage),
    Buffer {
        buffer: BufferId,
        epoch: u64,
        total_bytes: u64,
        snapshot: ByteBuf,
        continuation: Option<SnapshotContinuation>,
    },
    BufferSnapshotChunk {
        transfer: SnapshotTransferId,
        offset: u64,
        snapshot: ByteBuf,
        continuation: Option<SnapshotContinuation>,
    },
    BufferSaved {
        version: FileVersion,
    },
    Following {
        location: Option<FollowLocation>,
    },
    Invitation(String),
    LanguageServer(LanguageServerResponse),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageServerResponse {
    pub result: Result<ByteBuf, LanguageServerError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageServerError {
    pub code: i64,
    pub message: String,
    pub data: Option<ByteBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageServerDiagnostics {
    pub path: WorkspacePath,
    pub server: String,
    pub params: ByteBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguageServerRefreshKind {
    WorkspaceDiagnostics,
    SemanticTokens,
    CodeLens,
    InlayHints,
    InlineValues,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageServerRefresh {
    pub server: String,
    pub kind: LanguageServerRefreshKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub name: String,
    pub owner: ParticipantId,
    pub participants: Vec<ParticipantInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectState {
    pub file_revision: u64,
    pub open_buffers: Vec<OpenBufferInfo>,
    pub participants: Vec<ParticipantInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenBufferInfo {
    pub buffer: BufferId,
    pub path: WorkspacePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    ProtocolMismatch,
    Unauthenticated,
    InvalidCredential,
    ExpiredCredential,
    Forbidden,
    NotFound,
    Conflict,
    InvalidRequest,
    ResourceExhausted,
    ResyncRequired,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    ProjectInfo,
    ProjectState,
    ListFiles {
        options: ScanOptions,
        after: Option<WorkspacePath>,
        limit: u16,
    },
    SearchContent(ContentSearchQuery),
    PathExists {
        path: WorkspacePath,
    },
    OpenBuffer {
        path: WorkspacePath,
    },
    ReadBuffer {
        buffer: BufferId,
    },
    CloseBuffer {
        buffer: BufferId,
    },
    ContinueBufferSnapshot {
        continuation: SnapshotContinuation,
    },
    SyncBuffer {
        buffer: BufferId,
        epoch: u64,
        message: ByteBuf,
    },
    StartBufferSync {
        transfer: SyncTransferId,
        buffer: BufferId,
        epoch: u64,
        total_bytes: u64,
        message: ByteBuf,
    },
    ContinueBufferSync {
        transfer: SyncTransferId,
        offset: u64,
        message: ByteBuf,
    },
    SaveBuffer {
        buffer: BufferId,
        overwrite: bool,
    },
    ApplyFileTransaction {
        transaction: FileTransaction,
    },
    ReplayFileTransaction {
        redo: bool,
    },
    LanguageServer {
        buffer: BufferId,
        server: String,
        method: String,
        params: ByteBuf,
    },
    PublishPresence(Presence),
    Follow {
        participant: ParticipantId,
    },
    Invite {
        role: Role,
        expires_unix_secs: u64,
    },
    SetRole {
        participant: ParticipantId,
        role: Role,
    },
    RemoveParticipant {
        participant: ParticipantId,
    },
    Leave,
}

impl Request {
    pub fn required_role(&self) -> Role {
        match self {
            Self::PublishPresence(_) | Self::Follow { .. } | Self::Leave => Role::Observe,
            Self::ProjectInfo
            | Self::ProjectState
            | Self::ListFiles { .. }
            | Self::SearchContent(_)
            | Self::PathExists { .. }
            | Self::OpenBuffer { .. }
            | Self::ReadBuffer { .. }
            | Self::CloseBuffer { .. }
            | Self::ContinueBufferSnapshot { .. } => Role::Read,
            Self::LanguageServer { method, .. } => language_server_required_role(method),
            Self::SyncBuffer { .. }
            | Self::StartBufferSync { .. }
            | Self::ContinueBufferSync { .. }
            | Self::SaveBuffer { .. }
            | Self::ApplyFileTransaction { .. }
            | Self::ReplayFileTransaction { .. } => Role::Write,
            Self::Invite { .. } | Self::SetRole { .. } | Self::RemoveParticipant { .. } => {
                Role::Owner
            }
        }
    }
}

fn language_server_required_role(method: &str) -> Role {
    match method {
        "initialize"
        | "textDocument/completion"
        | "completionItem/resolve"
        | "textDocument/hover"
        | "textDocument/signatureHelp"
        | "textDocument/declaration"
        | "textDocument/definition"
        | "textDocument/typeDefinition"
        | "textDocument/implementation"
        | "textDocument/references"
        | "textDocument/documentHighlight"
        | "textDocument/documentSymbol"
        | "textDocument/codeAction"
        | "codeAction/resolve"
        | "textDocument/codeLens"
        | "codeLens/resolve"
        | "textDocument/formatting"
        | "textDocument/rangeFormatting"
        | "textDocument/onTypeFormatting"
        | "textDocument/prepareRename"
        | "textDocument/rename"
        | "textDocument/linkedEditingRange"
        | "textDocument/documentLink"
        | "documentLink/resolve"
        | "textDocument/documentColor"
        | "textDocument/colorPresentation"
        | "textDocument/foldingRange"
        | "textDocument/selectionRange"
        | "textDocument/semanticTokens/full"
        | "textDocument/semanticTokens/full/delta"
        | "textDocument/semanticTokens/range"
        | "textDocument/inlayHint"
        | "inlayHint/resolve"
        | "textDocument/inlineCompletion"
        | "textDocument/inlineValue"
        | "textDocument/diagnostic"
        | "workspace/diagnostic"
        | "workspace/symbol"
        | "workspaceSymbol/resolve"
        | "textDocument/prepareCallHierarchy"
        | "callHierarchy/incomingCalls"
        | "callHierarchy/outgoingCalls"
        | "textDocument/prepareTypeHierarchy"
        | "typeHierarchy/supertypes"
        | "typeHierarchy/subtypes"
        | "textDocument/moniker" => Role::Read,
        "workspace/executeCommand" => Role::Owner,
        _ => Role::Owner,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub participant: ParticipantId,
    pub buffer: BufferId,
    pub cursor: Option<TextAnchor>,
    pub selection: Option<(TextAnchor, TextAnchor)>,
    pub viewport: Option<TextAnchor>,
    pub active_view: Option<ViewId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowLocation {
    pub path: WorkspacePath,
    pub presence: Presence,
}

/// Stable CRDT-relative position. The bytes are interpreted only by the text engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TextAnchor(pub ByteBuf);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorAffinity {
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    ProjectState(ProjectState),
    ParticipantJoined(ParticipantInfo),
    ParticipantLeft {
        participant: ParticipantId,
    },
    RoleChanged {
        participant: ParticipantId,
        role: Role,
    },
    Presence(Presence),
    PresenceCleared {
        participant: ParticipantId,
        buffer: BufferId,
    },
    FollowRequested {
        follower: ParticipantId,
        leader: ParticipantId,
    },
    BufferSync {
        buffer: BufferId,
        epoch: u64,
        message: ByteBuf,
    },
    BufferSaved {
        buffer: BufferId,
        version: FileVersion,
    },
    FilesChanged {
        file_revision: u64,
        transaction: FileTransaction,
        undone: bool,
    },
    WorktreeChanged {
        file_revision: u64,
        changes: Vec<helix_workspace::FileChange>,
        rescan: bool,
    },
    ResyncRequired {
        buffer: BufferId,
        epoch: u64,
    },
    LanguageServerDiagnostics(LanguageServerDiagnostics),
    LanguageServerRefresh(LanguageServerRefresh),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantInfo {
    pub id: ParticipantId,
    pub name: String,
    pub role: Role,
    pub incarnation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximum_file_page_fits_the_transport_frame() {
        let entries = (0..MAX_FILE_PAGE_ENTRIES)
            .map(|index| {
                WorkspacePath::new([
                    format!("{index:03}{}", "a".repeat(4093)),
                    "b".repeat(4096),
                    "c".repeat(4096),
                    "d".repeat(4000),
                ])
                .unwrap()
            })
            .collect::<Vec<_>>();
        let frame = HostFrame::Response {
            id: u64::MAX,
            result: Ok(Response::Files {
                next: entries.last().cloned(),
                entries,
            }),
        };
        let encoded = rmp_serde::to_vec_named(&frame).unwrap();
        assert!(
            encoded.len() <= MAX_TRANSPORT_FRAME_BYTES,
            "file page encoded to {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn maximum_project_state_fits_the_transport_frame() {
        let open_buffers = (0..MAX_OPEN_BUFFERS)
            .map(|index| OpenBufferInfo {
                buffer: BufferId(index as u64 + 1),
                path: WorkspacePath::new([
                    format!("{index:03}{}", "a".repeat(4093)),
                    "b".repeat(4096),
                    "c".repeat(4096),
                    "d".repeat(4000),
                ])
                .unwrap(),
            })
            .collect();
        let frame = HostFrame::Event(Event::ProjectState(ProjectState {
            file_revision: u64::MAX,
            open_buffers,
            participants: (0..MAX_PARTICIPANTS)
                .map(|index| ParticipantInfo {
                    id: ParticipantId([index as u8; 16]),
                    name: "x".repeat(MAX_PARTICIPANT_NAME_BYTES),
                    role: Role::Write,
                    incarnation: u64::MAX,
                })
                .collect(),
        }));
        let encoded = rmp_serde::to_vec_named(&frame).unwrap();
        assert!(
            encoded.len() <= MAX_TRANSPORT_FRAME_BYTES,
            "project state encoded to {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn maximum_worktree_change_batch_fits_the_transport_frame() {
        let changes = (0..MAX_WORKTREE_CHANGES_PER_EVENT)
            .map(|index| helix_workspace::FileChange {
                path: WorkspacePath::new([
                    format!("{index:03}{}", "a".repeat(4093)),
                    "b".repeat(4096),
                    "c".repeat(4096),
                    "d".repeat(4000),
                ])
                .unwrap(),
                kind: helix_workspace::FileChangeKind::Modified,
            })
            .collect();
        let frame = HostFrame::Event(Event::WorktreeChanged {
            file_revision: u64::MAX,
            changes,
            rescan: false,
        });
        let encoded = rmp_serde::to_vec_named(&frame).unwrap();
        assert!(
            encoded.len() <= MAX_TRANSPORT_FRAME_BYTES,
            "worktree change batch encoded to {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn maximum_snapshot_chunk_fits_the_transport_frame() {
        let frame = HostFrame::Response {
            id: u64::MAX,
            result: Ok(Response::BufferSnapshotChunk {
                transfer: SnapshotTransferId(u64::MAX),
                offset: MAX_BUFFER_SNAPSHOT_BYTES as u64,
                snapshot: ByteBuf::from(vec![0; MAX_BUFFER_SNAPSHOT_CHUNK_BYTES]),
                continuation: Some(SnapshotContinuation {
                    transfer: SnapshotTransferId(u64::MAX),
                    offset: MAX_BUFFER_SNAPSHOT_BYTES as u64,
                }),
            }),
        };
        let encoded = rmp_serde::to_vec_named(&frame).unwrap();
        assert!(
            encoded.len() <= MAX_TRANSPORT_FRAME_BYTES,
            "snapshot chunk encoded to {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn maximum_sync_chunks_fit_the_transport_frame() {
        let request = ClientFrame::Request {
            id: u64::MAX,
            request: Request::StartBufferSync {
                transfer: SyncTransferId(u64::MAX),
                buffer: BufferId(u64::MAX),
                epoch: u64::MAX,
                total_bytes: MAX_SYNC_MESSAGE_BYTES as u64,
                message: ByteBuf::from(vec![0; MAX_SYNC_MESSAGE_CHUNK_BYTES]),
            },
        };
        let request = rmp_serde::to_vec_named(&request).unwrap();
        assert!(
            request.len() <= MAX_TRANSPORT_FRAME_BYTES,
            "sync request chunk encoded to {} bytes",
            request.len()
        );

        let event = HostFrame::Event(Event::BufferSync {
            buffer: BufferId(u64::MAX),
            epoch: u64::MAX,
            message: ByteBuf::from(vec![0; MAX_SYNC_MESSAGE_CHUNK_BYTES]),
        });
        let event = rmp_serde::to_vec_named(&event).unwrap();
        assert!(
            event.len() <= MAX_TRANSPORT_FRAME_BYTES,
            "sync event chunk encoded to {} bytes",
            event.len()
        );
    }

    #[test]
    fn rejects_oversized_file_versions() {
        let bytes =
            rmp_serde::to_vec(&ByteBuf::from(vec![0; crate::MAX_FILE_VERSION_BYTES + 1])).unwrap();
        assert!(rmp_serde::from_slice::<crate::FileVersion>(&bytes).is_err());
    }

    #[test]
    fn language_server_permissions_default_host_effects_to_owner() {
        assert_eq!(
            language_server_required_role("textDocument/hover"),
            Role::Read
        );
        assert_eq!(
            language_server_required_role("textDocument/completion"),
            Role::Read
        );
        assert_eq!(
            language_server_required_role("workspace/executeCommand"),
            Role::Owner
        );
        assert_eq!(
            language_server_required_role("vendor/customMutation"),
            Role::Owner
        );
    }
}
