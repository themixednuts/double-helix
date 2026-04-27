//! Structured error types for the host-agnostic plugin contract.
//!
//! These errors are returned by capability trait methods and are designed to
//! be serializable for future transport compatibility. They are separate from
//! the runtime [`crate::error::PluginError`] which covers Lua/loading concerns.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Structured error for plugin API operations.
///
/// Each variant has named fields to keep error messages informative and
/// transport-safe. The `entity` / `reason` / `capability` strings should be
/// human-readable identifiers, not debug dumps of internal state.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum ContractError {
    /// The referenced resource does not exist.
    #[error("not found: {entity}")]
    NotFound { entity: String },

    /// The handle was valid at some point but the resource has since been closed.
    #[error("stale handle: {entity} no longer exists")]
    StaleHandle { entity: String },

    /// The request is structurally invalid (missing fields, out-of-range values, etc.).
    #[error("invalid request: {reason}")]
    InvalidRequest { reason: String },

    /// The plugin does not have permission for this operation.
    #[error("permission denied: {reason}")]
    PermissionDenied { reason: String },

    /// The host does not support the requested capability.
    #[error("unsupported capability: {capability}")]
    UnsupportedCapability { capability: String },

    /// The host is busy and cannot process the request right now.
    #[error("busy: {reason}")]
    Busy { reason: String },

    /// An unexpected internal error in the host.
    #[error("internal host error: {message}")]
    InternalError { message: String },
}

/// Convenience result alias for contract operations.
pub type ContractResult<T> = Result<T, ContractError>;

// ---- Construction helpers ----

impl ContractError {
    pub fn not_found(entity: impl Into<String>) -> Self {
        Self::NotFound {
            entity: entity.into(),
        }
    }

    pub fn stale_handle(entity: impl Into<String>) -> Self {
        Self::StaleHandle {
            entity: entity.into(),
        }
    }

    pub fn invalid_request(reason: impl Into<String>) -> Self {
        Self::InvalidRequest {
            reason: reason.into(),
        }
    }

    pub fn unsupported(capability: impl Into<String>) -> Self {
        Self::UnsupportedCapability {
            capability: capability.into(),
        }
    }

    pub fn permission_denied(reason: impl Into<String>) -> Self {
        Self::PermissionDenied {
            reason: reason.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::InternalError {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let e = ContractError::stale_handle("DocumentHandle(42)");
        assert_eq!(
            e.to_string(),
            "stale handle: DocumentHandle(42) no longer exists"
        );
    }

    #[test]
    fn error_serde_round_trip() {
        let e = ContractError::not_found("view 7");
        let bytes = super::super::codec::encode(&e).unwrap();
        let e2: ContractError = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(e, e2);
    }
}
