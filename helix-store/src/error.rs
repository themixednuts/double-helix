use std::path::PathBuf;

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

pub type Result<T> = std::result::Result<T, Error>;
