use crate::{
    ContentSearchPage, ContentSearchQuery, ErrorCode, RemoteError, ScanOptions, SearchEntry,
};
use helix_workspace::{WorkspaceSearchIndex, WorkspaceSearchIndexError};
use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

pub(crate) struct SearchIndex(WorkspaceSearchIndex);

pub(crate) struct SearchSnapshot {
    pub(crate) entries: Vec<SearchEntry>,
    pub(crate) scanned: u64,
    pub(crate) scan_complete: bool,
}

impl SearchIndex {
    pub(crate) fn new(
        workspace_root: PathBuf,
        index_root: PathBuf,
        options: ScanOptions,
    ) -> Result<Self, RemoteError> {
        WorkspaceSearchIndex::new(workspace_root, index_root, options)
            .map(Self)
            .map_err(search_error)
    }

    pub(crate) fn matches(&self, index_root: &Path, options: ScanOptions) -> bool {
        self.0.matches(index_root, options)
    }

    pub(crate) fn snapshot(
        &self,
        query: &str,
        limit: usize,
        wait: Duration,
        canceled: &AtomicBool,
    ) -> Result<SearchSnapshot, RemoteError> {
        let snapshot = self
            .0
            .file_snapshot(query, limit, wait, canceled)
            .map_err(search_error)?;
        Ok(SearchSnapshot {
            entries: snapshot
                .entries
                .into_iter()
                .map(|entry| SearchEntry {
                    path: entry.path,
                    score: entry.score,
                })
                .collect(),
            scanned: snapshot.scanned,
            scan_complete: snapshot.scan_complete,
        })
    }

    pub(crate) fn content_page(
        &self,
        query: &ContentSearchQuery,
        wait: Duration,
        canceled: Arc<AtomicBool>,
    ) -> Result<ContentSearchPage, RemoteError> {
        self.0
            .content_page(query, wait, canceled)
            .map_err(search_error)
    }
}

fn search_error(error: WorkspaceSearchIndexError) -> RemoteError {
    let code = match error {
        WorkspaceSearchIndexError::InvalidQuery(_) | WorkspaceSearchIndexError::InvalidRegex(_) => {
            ErrorCode::InvalidRequest
        }
        WorkspaceSearchIndexError::Path(_) => ErrorCode::InvalidPath,
        WorkspaceSearchIndexError::Initialize(_)
        | WorkspaceSearchIndexError::Lock(_)
        | WorkspaceSearchIndexError::Unavailable => ErrorCode::Io,
    };
    RemoteError::new(code, error.to_string())
}
