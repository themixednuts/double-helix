use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use fff_search::{ByteSourceGrepCursor, ContentOverlay};
use helix_remote::{ScanOptions, WorkspacePath};
use helix_view::editor::{FilePickerConfig, WorkspaceBackend};
use tokio_util::sync::CancellationToken;

use super::{ExplorerPath, ExplorerSource};

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceContentOverlay {
    target: ExplorerPath,
    content: ContentOverlay,
}

impl WorkspaceContentOverlay {
    pub(crate) fn new(target: ExplorerPath, bytes: Arc<[u8]>, revision: u64) -> Self {
        Self {
            content: ContentOverlay {
                path: target.icon_path().into_owned(),
                bytes,
                revision,
            },
            target,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceContentMatch {
    pub(crate) path: ExplorerPath,
    pub(crate) line: usize,
}

#[derive(Debug, Default)]
pub(crate) struct WorkspaceContentPage {
    pub(crate) matches: Vec<WorkspaceContentMatch>,
    pub(crate) done: bool,
}

pub(crate) struct WorkspaceContentSearch {
    source: ExplorerSource,
    pattern: Arc<str>,
    smart_case: bool,
    picker_config: Arc<FilePickerConfig>,
    scan_options: ScanOptions,
    overlays: Arc<[WorkspaceContentOverlay]>,
    fff_overlays: Arc<[ContentOverlay]>,
    excluded_paths: Arc<[WorkspacePath]>,
    overlay_cursor: Option<ByteSourceGrepCursor>,
    cursor: helix_workspace::ContentSearchCursor,
    local_file_offset: usize,
    canceled: CancellationToken,
    abort: Arc<AtomicBool>,
    done: bool,
}

impl WorkspaceContentSearch {
    pub(crate) fn new(
        source: ExplorerSource,
        pattern: impl Into<Arc<str>>,
        smart_case: bool,
        picker_config: Arc<FilePickerConfig>,
        mut overlays: Vec<WorkspaceContentOverlay>,
        canceled: CancellationToken,
    ) -> anyhow::Result<Self> {
        let pattern = pattern.into();
        if pattern.is_empty() || pattern.len() > helix_workspace::MAX_CONTENT_SEARCH_QUERY_BYTES {
            anyhow::bail!("content search pattern is invalid");
        }
        if source.collaboration().is_some() && !overlays.is_empty() {
            anyhow::bail!("collaboration content overlays must be resolved by the host");
        }
        if let Some(root) = source.root().local_path() {
            overlays.retain(|overlay| {
                overlay
                    .target
                    .local_path()
                    .is_some_and(|path| path.starts_with(root))
            });
        }
        let mut unique = std::collections::HashSet::with_capacity(overlays.len());
        overlays.retain(|overlay| unique.insert(overlay.target.clone()));
        let excluded_paths =
            if source.remote().is_some() {
                overlays
                    .iter()
                    .map(|overlay| {
                        overlay.target.remote_path().cloned().ok_or_else(|| {
                            anyhow::anyhow!("remote content overlay has a local path")
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?
            } else {
                Vec::new()
            };
        if excluded_paths.len() > helix_workspace::MAX_CONTENT_SEARCH_EXCLUDED_PATHS {
            anyhow::bail!("too many modified remote buffers to search safely");
        }
        let scan_options = picker_config.workspace_scan_options();
        let overlays: Arc<[WorkspaceContentOverlay]> = overlays.into();
        let fff_overlays = overlays
            .iter()
            .map(|overlay| overlay.content.clone())
            .collect::<Vec<_>>()
            .into();
        let overlay_cursor = (!overlays.is_empty()).then(ByteSourceGrepCursor::default);

        Ok(Self {
            source,
            pattern,
            smart_case,
            picker_config,
            scan_options,
            overlays,
            fff_overlays,
            excluded_paths: excluded_paths.into(),
            overlay_cursor,
            cursor: helix_workspace::ContentSearchCursor::default(),
            local_file_offset: 0,
            canceled,
            abort: Arc::new(AtomicBool::new(false)),
            done: false,
        })
    }

    pub(crate) async fn next_page(
        &mut self,
        block: helix_runtime::Block,
        limit: usize,
    ) -> anyhow::Result<WorkspaceContentPage> {
        if self.done {
            return Ok(WorkspaceContentPage {
                done: true,
                ..WorkspaceContentPage::default()
            });
        }
        let limit = limit.min(helix_workspace::MAX_CONTENT_SEARCH_PAGE_RESULTS);
        if limit == 0 {
            anyhow::bail!("content search page limit is invalid");
        }

        loop {
            self.ensure_active()?;
            if let Some(cursor) = self.overlay_cursor {
                let pattern = self.pattern.clone();
                let overlays = self.fff_overlays.clone();
                let abort = self.abort.clone();
                let smart_case = self.smart_case;
                let worker = block.clone().spawn(move || {
                    crate::fff::grep_overlays_page(
                        &pattern, smart_case, &overlays, cursor, limit, abort,
                    )
                });
                let page = tokio::select! {
                    _ = self.canceled.cancelled() => {
                        self.abort.store(true, Ordering::Release);
                        anyhow::bail!("content search was canceled");
                    }
                    result = worker => result??,
                };
                self.overlay_cursor = page.next;
                let matches = page
                    .matches
                    .into_iter()
                    .map(|item| {
                        let overlay = self.overlays.get(item.source).ok_or_else(|| {
                            anyhow::anyhow!("content overlay result has an invalid source")
                        })?;
                        Ok(WorkspaceContentMatch {
                            path: overlay.target.clone(),
                            line: item.line_num,
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                if !matches.is_empty() {
                    return Ok(WorkspaceContentPage {
                        matches,
                        done: false,
                    });
                }
                if self.overlay_cursor.is_some() {
                    tokio::task::yield_now().await;
                    continue;
                }
            }

            match self.source.backend().clone() {
                WorkspaceBackend::Local => {
                    let root = self
                        .source
                        .root()
                        .local_path()
                        .expect("local workspace search has a non-local root")
                        .to_path_buf();
                    let previous = self.local_file_offset;
                    let pattern = self.pattern.clone();
                    let config = self.picker_config.clone();
                    let overlays = self.fff_overlays.clone();
                    let abort = self.abort.clone();
                    let smart_case = self.smart_case;
                    let worker = block.clone().spawn(move || {
                        crate::fff::grep_files_page(crate::fff::GrepFilesPageRequest {
                            root: &root,
                            query: &pattern,
                            smart_case,
                            config: &config,
                            content_overlays: &overlays,
                            file_offset: previous,
                            limit,
                            abort_signal: abort,
                        })
                    });
                    let page = tokio::select! {
                        _ = self.canceled.cancelled() => {
                            self.abort.store(true, Ordering::Release);
                            anyhow::bail!("content search was canceled");
                        }
                        result = worker => result??,
                    };
                    let next = validate_progress(
                        page.matches.is_empty(),
                        page.done,
                        page.next_file_offset,
                        previous,
                    )?;
                    self.done = page.done;
                    if let Some(next) = next {
                        self.local_file_offset = next;
                    }
                    let matches: Vec<_> = page
                        .matches
                        .into_iter()
                        .map(|item| WorkspaceContentMatch {
                            path: ExplorerPath::Local(item.path),
                            line: item.line_num,
                        })
                        .collect();
                    if matches.is_empty() && !self.done {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        continue;
                    }
                    return Ok(WorkspaceContentPage {
                        matches,
                        done: self.done,
                    });
                }
                WorkspaceBackend::Remote(remote) => {
                    let page = remote
                        .search_content_page(self.query(limit), self.canceled.child_token())
                        .await?;
                    if let Some(page) = self.apply_protocol_page(page, ExplorerPath::Remote)? {
                        return Ok(page);
                    }
                }
                WorkspaceBackend::Collaboration(session) => {
                    let project = session.project().id;
                    let page = session
                        .search_content_page(self.query(limit), self.canceled.child_token())
                        .await?;
                    if let Some(page) = self.apply_protocol_page(page, |path| {
                        ExplorerPath::Collaboration { project, path }
                    })? {
                        return Ok(page);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn query(&self, limit: usize) -> helix_workspace::ContentSearchQuery {
        helix_workspace::ContentSearchQuery {
            root: WorkspacePath::root(),
            pattern: self.pattern.to_string(),
            smart_case: self.smart_case,
            options: self.scan_options,
            excluded_paths: self.excluded_paths.to_vec(),
            cursor: self.cursor,
            limit: limit as u16,
        }
    }

    fn apply_protocol_page(
        &mut self,
        page: helix_workspace::ContentSearchPage,
        mut target: impl FnMut(WorkspacePath) -> ExplorerPath,
    ) -> anyhow::Result<Option<WorkspaceContentPage>> {
        let previous = self.cursor;
        let next = validate_progress(page.entries.is_empty(), page.done, page.next, previous)?;
        self.done = page.done;
        if let Some(next) = next {
            self.cursor = next;
        }
        let matches = page
            .entries
            .into_iter()
            .map(|entry| {
                Ok(WorkspaceContentMatch {
                    path: target(entry.path),
                    line: usize::try_from(entry.line)
                        .map_err(|_| anyhow::anyhow!("content match line is too large"))?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if matches.is_empty() && !self.done {
            return Ok(None);
        }
        Ok(Some(WorkspaceContentPage {
            matches,
            done: self.done,
        }))
    }

    fn ensure_active(&self) -> anyhow::Result<()> {
        if self.canceled.is_cancelled() || self.abort.load(Ordering::Acquire) {
            anyhow::bail!("content search was canceled");
        }
        Ok(())
    }
}

impl Drop for WorkspaceContentSearch {
    fn drop(&mut self) {
        self.abort.store(true, Ordering::Release);
    }
}

fn validate_progress<T: Copy + Eq>(
    empty: bool,
    done: bool,
    next: Option<T>,
    previous: T,
) -> anyhow::Result<Option<T>> {
    match (done, next) {
        (true, None) => Ok(None),
        (true, Some(_)) => anyhow::bail!("completed content search returned a continuation"),
        (false, None) => anyhow::bail!("incomplete content search omitted its continuation"),
        (false, Some(next)) if next == previous && !empty => {
            anyhow::bail!("content search continuation did not advance")
        }
        (false, Some(next)) => Ok(Some(next)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_validation_allows_waiting_but_rejects_duplicate_pages() {
        assert_eq!(validate_progress(true, false, Some(0), 0).unwrap(), Some(0));
        assert!(validate_progress(false, false, Some(0), 0).is_err());
        assert!(validate_progress::<u64>(true, false, None, 0).is_err());
        assert!(validate_progress(true, true, Some(1), 0).is_err());
        assert_eq!(validate_progress(true, true, None, 0).unwrap(), None);
    }

    #[test]
    fn local_search_session_uses_modified_overlay_instead_of_stale_disk_text() {
        let temp = tempfile::tempdir().unwrap();
        let root = helix_stdx::path::canonicalize(temp.path());
        let path = root.join("shared.txt");
        std::fs::write(&path, "stale_only\n").unwrap();
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut search = WorkspaceContentSearch::new(
                ExplorerSource::from_backend(
                    root.clone(),
                    &helix_view::editor::WorkspaceBackend::Local,
                ),
                "fresh_only",
                true,
                Arc::new(FilePickerConfig::default()),
                vec![WorkspaceContentOverlay::new(
                    ExplorerPath::Local(path.clone()),
                    Arc::from(b"fresh_only\n".as_slice()),
                    1,
                )],
                CancellationToken::new(),
            )
            .unwrap();
            let mut matches = Vec::new();
            for _ in 0..32 {
                let page = search
                    .next_page(runtime.runtime().block().clone(), 1)
                    .await
                    .unwrap();
                matches.extend(page.matches);
                if page.done {
                    break;
                }
            }

            assert_eq!(
                matches,
                vec![WorkspaceContentMatch {
                    path: ExplorerPath::Local(path),
                    line: 0,
                }]
            );
        });
    }
}
