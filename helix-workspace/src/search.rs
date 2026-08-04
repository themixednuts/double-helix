use crate::WorkspacePath;
use serde::{Deserialize, Serialize};

pub const MAX_CONTENT_SEARCH_QUERY_BYTES: usize = 16 * 1024;
pub const MAX_CONTENT_SEARCH_RESULTS: usize = 2_000;
/// FFF finishes the file that crosses this soft target. With its 200-match
/// per-file cap and maximum workspace-path size, a page remains below the
/// remote protocol's frame limit even for adversarial paths.
pub const MAX_CONTENT_SEARCH_PAGE_RESULTS: usize = 96;
pub const MAX_CONTENT_SEARCH_EXCLUDED_PATHS: usize = 128;

/// File discovery behavior shared by local, remote, and collaborative workspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScanOptions {
    pub hidden: bool,
    pub parents: bool,
    pub ignore: bool,
    pub git_ignore: bool,
    pub git_global: bool,
    pub git_exclude: bool,
    pub follow_symlinks: bool,
    pub deduplicate_symlinks: bool,
    pub max_depth: Option<u32>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            hidden: true,
            parents: true,
            ignore: true,
            git_ignore: true,
            git_global: true,
            git_exclude: true,
            follow_symlinks: false,
            deduplicate_symlinks: true,
            max_depth: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DirectoryOptions {
    pub scan: ScanOptions,
    pub flatten_dirs: bool,
}

/// A stateless continuation into the query's filtered file sequence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSearchCursor {
    /// Collaboration-host overlay buffer index.
    pub overlay: u16,
    /// Match offset within the current overlay buffer.
    pub overlay_match: u16,
    pub file_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSearchQuery {
    pub root: WorkspacePath,
    pub pattern: String,
    pub smart_case: bool,
    pub options: ScanOptions,
    /// Paths whose authoritative contents are overlaid by the caller.
    pub excluded_paths: Vec<WorkspacePath>,
    pub cursor: ContentSearchCursor,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSearchEntry {
    pub path: WorkspacePath,
    /// Zero-based line number.
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSearchPage {
    pub entries: Vec<ContentSearchEntry>,
    /// Present when more indexed files may be searched for this query.
    pub next: Option<ContentSearchCursor>,
    pub scanned: u64,
    /// True only when the index and this query are both exhausted.
    pub done: bool,
}

impl ContentSearchQuery {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.pattern.is_empty()
            || self.pattern.len() > MAX_CONTENT_SEARCH_QUERY_BYTES
            || self.pattern.contains('\0')
        {
            return Err("content search pattern is invalid");
        }
        if self.excluded_paths.len() > MAX_CONTENT_SEARCH_EXCLUDED_PATHS {
            return Err("content search excludes too many paths");
        }
        if usize::from(self.limit) > MAX_CONTENT_SEARCH_PAGE_RESULTS || self.limit == 0 {
            return Err("content search page limit is invalid");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_search_validation_is_bounded() {
        let mut query = ContentSearchQuery {
            root: WorkspacePath::root(),
            pattern: "needle".to_owned(),
            smart_case: true,
            options: ScanOptions::default(),
            excluded_paths: Vec::new(),
            cursor: ContentSearchCursor::default(),
            limit: MAX_CONTENT_SEARCH_PAGE_RESULTS as u16,
        };
        assert_eq!(query.validate(), Ok(()));

        query.pattern = "x".repeat(MAX_CONTENT_SEARCH_QUERY_BYTES + 1);
        assert!(query.validate().is_err());
        query.pattern = "needle".to_owned();
        query.limit = 0;
        assert!(query.validate().is_err());
    }
}
