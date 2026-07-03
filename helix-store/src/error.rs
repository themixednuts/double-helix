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
}

pub type Result<T> = std::result::Result<T, Error>;
