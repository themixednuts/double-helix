//! Local-client and headless-server support for remote Helix workspaces.
//!
//! Document buffers, syntax, views, and input stay in the local editor. The
//! remote session owns capabilities that must observe the remote machine:
//! files, search, watches, processes, language services, packages, and VCS.

pub mod backend;
pub mod client;
mod language_server;
mod process;
pub mod protocol;
mod search;
pub mod server;
pub mod ssh;
mod transaction;
pub mod uri;
mod watch;
mod workspace;

pub use helix_workspace::{WorkspacePath, WorkspacePathError};
pub use protocol::*;

pub fn server_identity(version: &str) -> String {
    format!("double-helix {version} remote-protocol {PROTOCOL_VERSION}")
}
