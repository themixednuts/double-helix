use std::path::PathBuf;

use rusqlite::ErrorCode;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to prepare database directory {path}")]
    PrepareDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sqlite error")]
    Sqlite(#[from] rusqlite::Error),
    #[error("drizzle error")]
    Drizzle(#[from] drizzle::error::DrizzleError),
    #[error("json error")]
    Json(#[from] serde_json::Error),
    #[error(
        "runtime asset key collision for {asset_kind} '{asset_key}': package '{requested_package}' conflicts with '{existing_package}'"
    )]
    RuntimeAssetCollision {
        asset_kind: String,
        asset_key: String,
        existing_package: String,
        requested_package: String,
    },
    #[error("invalid runtime asset: {0}")]
    InvalidRuntimeAsset(String),
    #[error("unknown runtime asset kind '{0}' in the database")]
    UnknownRuntimeAssetKind(String),
    #[error("runtime activation history for '{package}' no longer matches the active snapshot")]
    RuntimeHistoryDiverged { package: String },
    #[error("invalid package state: {0}")]
    InvalidPackageState(String),
    #[error("invalid runtime generation {0}")]
    InvalidRuntimeGeneration(i64),
}

impl Error {
    pub(crate) fn is_busy(&self) -> bool {
        match self {
            Self::Sqlite(err) => sqlite_is_busy(err),
            Self::Drizzle(err) => drizzle_is_busy(err),
            _ => false,
        }
    }
}

fn sqlite_is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn drizzle_is_busy(err: &drizzle::error::DrizzleError) -> bool {
    let mut current: Option<&dyn std::error::Error> = Some(err);
    while let Some(err) = current {
        if let Some(sqlite) = err.downcast_ref::<rusqlite::Error>() {
            return sqlite_is_busy(sqlite);
        }
        current = err.source();
    }
    false
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::ffi;

    #[test]
    fn detects_sqlite_busy_and_locked() {
        let busy = Error::Sqlite(rusqlite::Error::SqliteFailure(
            ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        ));
        let locked = Error::Sqlite(rusqlite::Error::SqliteFailure(
            ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        ));
        let other = Error::InvalidRuntimeGeneration(1);

        assert!(busy.is_busy());
        assert!(locked.is_busy());
        assert!(!other.is_busy());
    }
}
