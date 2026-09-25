use std::path::StripPrefixError;

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error("Thread panicked")]
    ThreadPanic,
    #[error("Invalid path {0}")]
    InvalidPath(std::path::PathBuf),
    #[error(
        "Can not run certain FFF features in a file system root or home directories. Consider smaller per-project directories."
    )]
    FilesystemRoot(std::path::PathBuf),
    #[error("File picker not initialized")]
    FilePickerMissing,
    #[error("Failed to acquire lock for frecency")]
    AcquireFrecencyLock,
    #[error("Failed to acquire lock for items by provider")]
    AcquireItemLock,
    #[error("Failed to acquire lock for path cache")]
    AcquirePathCacheLock,
    #[error("Failed to create directory: {0}")]
    CreateDir(#[from] std::io::Error),
    #[error("Failed to remove database directory {path}: {source}")]
    RemoveDbDir {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    // FFF_STORAGE_TRAITS_BLOCKER: storage errors come from the caller-provided
    // `FrecencyStore` / `QueryTrackerStore`; upstream heed/LMDB variants are stripped.
    #[error("Persistent picker cache error: {0}")]
    Persistence(String),
    #[error("Failed to start file system watcher: {0}")]
    FileSystemWatch(#[from] notify::Error),

    #[error("Expected a path to be child of another path: {0}")]
    StripPrefixError(#[from] StripPrefixError),

    #[error("libgit2 error occurred: {0}")]
    Git(#[from] git2::Error),

    #[error("Filesystem walk failed: {0}")]
    WalkFailed(String),

    #[error("Invalid glob pattern '{pattern}': {reason}")]
    InvalidGlobPattern { pattern: String, reason: String },

    #[error("File system watching is disabled for this picker")]
    WatcherDisabled,

    #[error("File system watcher is not ready")]
    WatcherNotReady,

    #[error("Indexed base path changed while creating the watch subscription")]
    WatchBaseChanged,

    #[error("Failed to start watch callback dispatcher: {0}")]
    WatchDispatcherStart(#[source] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
