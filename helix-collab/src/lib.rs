//! Secure collaboration domain types and convergent replicated text.
//!
//! The host remains authoritative for project services. This crate deliberately
//! exposes no terminal, process, package, plugin, LSP, or debugger execution API.

pub mod backend;
pub mod client;
pub mod project;
pub mod protocol;
pub mod replica;
pub mod service;
pub mod session;
pub mod session_client;
pub mod text;
pub mod transport;
pub mod uri;

pub use backend::{LocalBackend, RemoteBackend};
pub use client::{Client, ClientError, ConnectionState};
pub use project::{
    Backend, BackendFileUpdate, BackendFileWatch, BackendFuture, BackendTransactionId, FileData,
    FileVersion, FileVersionError, Project, ProjectError, MAX_FILE_VERSION_BYTES,
};
pub use protocol::*;
pub use replica::{ReplicaError, ReplicaProject, ReplicaUpdate};
pub use service::{
    HostFileMutation, HostHandle, HostLanguageServerRequest, HostProjectPublisher, HostServiceError,
};
pub use session::{AuthError, Authenticated, HostSession, Invitation, Participant};
pub use session_client::{
    GuestSession, GuestSessionError, GuestSessionHandle, GuestSessionUpdate, LocalPresence,
    OpenedBuffer, ResolvedFollowLocation, ResolvedPresence,
};
pub use text::{Buffer, BufferError, TextChange};
pub use transport::{
    Accepted, ConnectCode, Connected, ConnectionReceiver, ConnectionSender, HostEndpoint,
    TransportError,
};
