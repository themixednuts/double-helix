use crate::{
    is_internal_path, relative_workspace_path, ContentSearchCursor, ContentSearchEntry,
    ContentSearchPage, ContentSearchQuery, ScanOptions, WorkspacePath,
};
use fff_search::{
    FFFMode, FilePicker, FilePickerOptions, FilePickerScanOptions, FileSearchConfig,
    FuzzySearchOptions, GrepConfig, GrepMode, GrepSearchOptions, PaginationArgs, QueryParser,
    SharedFrecency, SharedPicker, SymlinkTargetScope,
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

pub struct WorkspaceSearchIndex {
    workspace_root: PathBuf,
    index_root: PathBuf,
    options: ScanOptions,
    picker: SharedPicker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchEntry {
    pub path: WorkspacePath,
    pub score: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchSnapshot {
    pub entries: Vec<FileSearchEntry>,
    pub scanned: u64,
    pub scan_complete: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceSearchIndexError {
    #[error("failed to initialize workspace search index: {0}")]
    Initialize(String),
    #[error("workspace search index lock failed: {0}")]
    Lock(String),
    #[error("workspace search index is unavailable")]
    Unavailable,
    #[error("workspace search result has an invalid path: {0}")]
    Path(#[from] crate::WorkspaceFsError),
    #[error("{0}")]
    InvalidQuery(&'static str),
    #[error("failed to compile content search pattern: {0}")]
    InvalidRegex(String),
}

impl WorkspaceSearchIndex {
    pub fn new(
        workspace_root: PathBuf,
        index_root: PathBuf,
        options: ScanOptions,
    ) -> Result<Self, WorkspaceSearchIndexError> {
        let workspace_root = search_path(workspace_root);
        let index_root = search_path(index_root);
        let picker = SharedPicker::default();
        FilePicker::new_with_shared_state(
            picker.clone(),
            SharedFrecency::noop(),
            FilePickerOptions {
                base_path: index_root.to_string_lossy().into_owned(),
                enable_mmap_cache: false,
                enable_content_indexing: false,
                mode: FFFMode::Neovim,
                cache_budget: None,
                watch: true,
                follow_symlinks: options.follow_symlinks,
                enable_fs_root_scanning: true,
                enable_home_dir_scanning: true,
                scan: FilePickerScanOptions {
                    hidden: options.hidden,
                    parents: options.parents,
                    ignore: options.ignore,
                    git_ignore: options.git_ignore,
                    git_global: options.git_global,
                    git_exclude: options.git_exclude,
                    follow_links: options.follow_symlinks,
                    max_depth: options.max_depth.map(|depth| depth as usize),
                    custom_ignore_files: Box::default(),
                    deduplicate_links: options.deduplicate_symlinks,
                    symlink_target_scope: SymlinkTargetScope::BaseDirectory,
                },
            },
        )
        .map_err(|error| WorkspaceSearchIndexError::Initialize(error.to_string()))?;
        Ok(Self {
            workspace_root,
            index_root,
            options,
            picker,
        })
    }

    pub fn matches(&self, index_root: &Path, options: ScanOptions) -> bool {
        self.index_root == search_path(index_root.to_path_buf()) && self.options == options
    }

    pub fn file_snapshot(
        &self,
        query: &str,
        limit: usize,
        wait: Duration,
        canceled: &AtomicBool,
    ) -> Result<FileSearchSnapshot, WorkspaceSearchIndexError> {
        let scan_complete = self.picker.wait_for_scan(wait);
        let guard = self
            .picker
            .read()
            .map_err(|error| WorkspaceSearchIndexError::Lock(error.to_string()))?;
        let picker = guard
            .as_ref()
            .ok_or(WorkspaceSearchIndexError::Unavailable)?;
        let scanned = picker.get_files().len() as u64;
        let entries = if query.trim().is_empty() {
            picker
                .get_files()
                .iter()
                .filter(|file| !file.is_deleted())
                .take(limit)
                .filter_map(|file| {
                    let absolute = file.absolute_path(picker, picker.base_path());
                    match self.public_path(&absolute) {
                        Ok(Some(path)) => Some(Ok(FileSearchEntry { path, score: 0 })),
                        Ok(None) => None,
                        Err(error) => Some(Err(error)),
                    }
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let parser: QueryParser<FileSearchConfig> = QueryParser::default();
            let parsed = parser.parse(query);
            let results = picker.fuzzy_search(
                &parsed,
                None,
                FuzzySearchOptions {
                    max_threads: responsive_search_threads(),
                    abort_signal: Some(canceled),
                    current_file: None,
                    project_path: Some(&self.index_root),
                    combo_boost_score_multiplier: 20_000,
                    min_combo_count: 2,
                    pagination: PaginationArgs { offset: 0, limit },
                },
            );
            results
                .items
                .into_iter()
                .zip(results.scores)
                .filter_map(|(file, score)| {
                    let absolute = file.absolute_path(picker, picker.base_path());
                    match self.public_path(&absolute) {
                        Ok(Some(path)) => Some(Ok(FileSearchEntry {
                            path,
                            score: i64::from(score.total),
                        })),
                        Ok(None) => None,
                        Err(error) => Some(Err(error)),
                    }
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(FileSearchSnapshot {
            entries,
            scanned,
            scan_complete,
        })
    }

    pub fn content_page(
        &self,
        query: &ContentSearchQuery,
        wait: Duration,
        canceled: Arc<AtomicBool>,
    ) -> Result<ContentSearchPage, WorkspaceSearchIndexError> {
        query
            .validate()
            .map_err(WorkspaceSearchIndexError::InvalidQuery)?;
        let scan_complete = self.picker.wait_for_scan(wait);
        let guard = self
            .picker
            .read()
            .map_err(|error| WorkspaceSearchIndexError::Lock(error.to_string()))?;
        let picker = guard
            .as_ref()
            .ok_or(WorkspaceSearchIndexError::Unavailable)?;
        let scanned = picker.get_files().len() as u64;
        let file_offset = usize::try_from(query.cursor.file_offset).map_err(|_| {
            WorkspaceSearchIndexError::InvalidQuery("content search cursor is invalid")
        })?;
        let parser = QueryParser::new(GrepConfig);
        let parsed = parser.parse(&query.pattern);
        let result = picker.grep(
            &parsed,
            &GrepSearchOptions {
                smart_case: query.smart_case,
                mode: GrepMode::Regex,
                file_offset,
                page_limit: usize::from(query.limit),
                time_budget_ms: 40,
                abort_signal: Some(canceled),
                ..GrepSearchOptions::default()
            },
        );
        if let Some(error) = result.regex_fallback_error {
            return Err(WorkspaceSearchIndexError::InvalidRegex(error));
        }

        let excluded: HashSet<_> = query.excluded_paths.iter().collect();
        let mut entries = Vec::with_capacity(result.matches.len());
        for item in result.matches {
            let Some(file) = result.files.get(item.file_index) else {
                continue;
            };
            let absolute = file.absolute_path(picker, picker.base_path());
            let Some(path) = self.public_path(&absolute)? else {
                continue;
            };
            if excluded.contains(&path) {
                continue;
            }
            entries.push(ContentSearchEntry {
                path,
                line: item.line_number.saturating_sub(1),
            });
        }

        let next_offset = if result.next_file_offset != 0 {
            Some(result.next_file_offset)
        } else if scan_complete {
            None
        } else {
            Some(result.filtered_file_count.max(file_offset))
        };
        Ok(ContentSearchPage {
            entries,
            next: next_offset.map(|file_offset| ContentSearchCursor {
                file_offset: file_offset as u64,
                ..query.cursor
            }),
            scanned,
            done: scan_complete && next_offset.is_none(),
        })
    }

    fn public_path(
        &self,
        absolute: &Path,
    ) -> Result<Option<WorkspacePath>, WorkspaceSearchIndexError> {
        if self.options.follow_symlinks {
            let Ok(target) = std::fs::canonicalize(absolute).map(search_path) else {
                return Ok(None);
            };
            if !target.starts_with(&self.workspace_root) {
                return Ok(None);
            }
        }
        let path = match relative_workspace_path(&self.workspace_root, absolute) {
            Ok(path) => path,
            Err(error) if error.kind() == crate::WorkspaceFsErrorKind::OutsideRoot => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        Ok((!is_internal_path(&path)).then_some(path))
    }
}

fn responsive_search_threads() -> usize {
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    available.saturating_sub((available / 4).max(1)).max(1)
}

fn search_path(path: PathBuf) -> PathBuf {
    fff_search::path_utils::canonicalize(&path).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_paths_use_the_picker_path_identity() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let aliased = nested.join("..");

        assert_eq!(
            search_path(aliased),
            fff_search::path_utils::canonicalize(root.path()).unwrap()
        );
    }

    #[test]
    fn search_results_outside_the_workspace_are_filtered() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let index = WorkspaceSearchIndex::new(
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            ScanOptions::default(),
        )
        .unwrap();

        assert!(matches!(index.public_path(outside.path()), Ok(None)));
    }

    #[test]
    fn content_pages_respect_ignores_exclusions_and_zero_based_lines() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("visible.txt"), "first\nneedle\nneedle\n").unwrap();
        std::fs::write(root.path().join("excluded.txt"), "needle\n").unwrap();
        std::fs::write(root.path().join("ignored.txt"), "needle\n").unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
        let index = WorkspaceSearchIndex::new(
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            ScanOptions::default(),
        )
        .unwrap();
        let excluded = WorkspacePath::from_slash_path("excluded.txt").unwrap();
        let mut query = ContentSearchQuery {
            root: WorkspacePath::root(),
            pattern: "needle".to_owned(),
            smart_case: true,
            options: ScanOptions::default(),
            excluded_paths: vec![excluded],
            cursor: ContentSearchCursor::default(),
            limit: 1,
        };
        let mut entries = Vec::new();
        for _ in 0..32 {
            let page = index
                .content_page(
                    &query,
                    Duration::from_secs(10),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            entries.extend(page.entries);
            if page.done {
                break;
            }
            query.cursor = page.next.expect("unfinished search must advance");
        }

        assert_eq!(
            entries,
            vec![
                ContentSearchEntry {
                    path: WorkspacePath::from_slash_path("visible.txt").unwrap(),
                    line: 1,
                },
                ContentSearchEntry {
                    path: WorkspacePath::from_slash_path("visible.txt").unwrap(),
                    line: 2,
                },
            ]
        );
    }

    #[test]
    fn invalid_regex_is_not_silently_treated_as_literal_text() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("visible.txt"), "text\n").unwrap();
        let index = WorkspaceSearchIndex::new(
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            ScanOptions::default(),
        )
        .unwrap();
        let error = index
            .content_page(
                &ContentSearchQuery {
                    root: WorkspacePath::root(),
                    pattern: "[".to_owned(),
                    smart_case: true,
                    options: ScanOptions::default(),
                    excluded_paths: Vec::new(),
                    cursor: ContentSearchCursor::default(),
                    limit: 1,
                },
                Duration::from_secs(10),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap_err();
        assert!(matches!(error, WorkspaceSearchIndexError::InvalidRegex(_)));
    }

    #[cfg(unix)]
    #[test]
    fn content_index_does_not_follow_symlinks_outside_workspace() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside_secret\n").unwrap();
        symlink(outside.path(), root.path().join("outside-link")).unwrap();
        let options = ScanOptions {
            follow_symlinks: true,
            deduplicate_symlinks: false,
            ..ScanOptions::default()
        };
        let index = WorkspaceSearchIndex::new(
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            options,
        )
        .unwrap();
        let page = index
            .content_page(
                &ContentSearchQuery {
                    root: WorkspacePath::root(),
                    pattern: "outside_secret".to_owned(),
                    smart_case: true,
                    options,
                    excluded_paths: Vec::new(),
                    cursor: ContentSearchCursor::default(),
                    limit: 1,
                },
                Duration::from_secs(10),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();

        assert!(page.entries.is_empty());
        assert!(page.done);
    }
}
