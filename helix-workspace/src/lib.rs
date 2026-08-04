//! Backend-neutral workspace identities and validated relative paths.

mod fs;
mod index;
mod path;
mod search;
mod transaction;

pub use fs::{
    atomic_replace, is_internal_path, relative_workspace_path, sync_parent_directory,
    RootedWorkspace, WorkspaceFsError, WorkspaceFsErrorKind,
};
pub use index::{
    FileSearchEntry, FileSearchSnapshot, WorkspaceSearchIndex, WorkspaceSearchIndexError,
};
pub use path::{
    WorkspacePath, WorkspacePathError, MAX_WORKSPACE_PATH_BYTES, MAX_WORKSPACE_PATH_SEGMENTS,
    MAX_WORKSPACE_PATH_SEGMENT_BYTES,
};
pub use search::{
    ContentSearchCursor, ContentSearchEntry, ContentSearchPage, ContentSearchQuery,
    DirectoryOptions, ScanOptions, MAX_CONTENT_SEARCH_EXCLUDED_PATHS,
    MAX_CONTENT_SEARCH_PAGE_RESULTS, MAX_CONTENT_SEARCH_QUERY_BYTES, MAX_CONTENT_SEARCH_RESULTS,
};
pub use transaction::{
    FileChange, FileChangeKind, FileOperation, FileTransaction, FileTransactionError,
    FileTransactionErrorKind, FileTransactionId, FileTransactionReceipt, FileTransactionStore,
    MAX_TRANSACTION_HISTORY, MAX_TRANSACTION_OPERATIONS,
};
