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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerIdentityMatch {
    Exact,
    Compatible,
    Drifted,
    Incompatible,
}

pub(crate) fn server_identity_match(expected: &str, actual: &str) -> ServerIdentityMatch {
    if expected == actual {
        return ServerIdentityMatch::Exact;
    }

    let (Some(expected), Some(actual)) = (
        parse_server_identity(expected),
        parse_server_identity(actual),
    ) else {
        return ServerIdentityMatch::Incompatible;
    };

    if expected.version != actual.version || expected.protocol != actual.protocol {
        return ServerIdentityMatch::Incompatible;
    }

    if expected.hash != actual.hash {
        ServerIdentityMatch::Drifted
    } else {
        ServerIdentityMatch::Compatible
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedServerIdentity<'a> {
    version: &'a str,
    hash: Option<&'a str>,
    protocol: u16,
}

fn parse_server_identity(identity: &str) -> Option<ParsedServerIdentity<'_>> {
    let identity = identity.trim();
    let rest = identity.strip_prefix("double-helix ")?;
    let (version, protocol) = rest.rsplit_once(" remote-protocol ")?;
    let protocol = protocol.parse::<u16>().ok()?;
    let version = version.trim();
    if version.is_empty() {
        return None;
    }

    let (version, hash) = match version.strip_suffix(')') {
        Some(version_and_hash) => {
            let (version, hash) = version_and_hash.rsplit_once(" (")?;
            if hash.is_empty() || hash.contains(['(', ')']) {
                return None;
            }
            (version.trim(), Some(hash))
        }
        None => {
            if version.contains(['(', ')']) {
                return None;
            }
            (version, None)
        }
    };

    (!version.is_empty()).then_some(ParsedServerIdentity {
        version,
        hash,
        protocol,
    })
}
