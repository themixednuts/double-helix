use arc_swap::ArcSwap;
use helix_vcs::{DiffHandle, FileBlame};
use std::sync::Arc;
use thiserror::Error;

use helix_core::Rope;

#[derive(Debug, Error)]
pub enum LineBlameError<'a> {
    #[error("Not committed yet")]
    NotCommittedYet,
    #[error("Unable to get blame for line {0}: {1}")]
    NoFileBlame(u32, &'a anyhow::Error),
    #[error("The blame for this file is not ready yet. Try again in a few seconds")]
    NotReadyYet,
}

#[derive(Debug, Default)]
pub struct VcsState {
    diff_handle: Option<DiffHandle>,
    version_control_head: Option<Arc<ArcSwap<Box<str>>>>,
    file_blame: Option<anyhow::Result<FileBlame>>,
    blame_outdated: bool,
}

impl VcsState {
    pub fn diff_handle(&self) -> Option<&DiffHandle> {
        self.diff_handle.as_ref()
    }

    pub fn set_diff_base(&mut self, diff_base: Rope, text: Rope) {
        if let Some(differ) = &self.diff_handle {
            differ.update_diff_base(diff_base);
            return;
        }
        self.diff_handle = Some(DiffHandle::new(diff_base, text));
    }

    pub fn clear_diff_base(&mut self) {
        self.diff_handle = None;
    }

    pub fn refresh_diff_document(&self, text: Rope) {
        if let Some(diff_handle) = &self.diff_handle {
            diff_handle.update_document(text);
        }
    }

    pub fn version_control_head(&self) -> Option<Arc<Box<str>>> {
        self.version_control_head
            .as_ref()
            .map(|head| head.load_full())
    }

    pub fn set_version_control_head(
        &mut self,
        version_control_head: Option<Arc<ArcSwap<Box<str>>>>,
    ) {
        self.version_control_head = version_control_head;
    }

    pub fn should_request_full_file_blame(&self, auto_fetch: bool) -> bool {
        auto_fetch || self.blame_outdated
    }

    pub fn is_blame_outdated(&self) -> bool {
        self.blame_outdated
    }

    pub fn mark_blame_outdated(&mut self) {
        self.blame_outdated = true;
    }

    pub fn clear_blame_outdated(&mut self) {
        self.blame_outdated = false;
    }

    pub fn set_file_blame(&mut self, result: anyhow::Result<FileBlame>) {
        self.file_blame = Some(result);
    }

    pub fn line_blame(&self, cursor_line: u32, format: &str) -> Result<String, LineBlameError<'_>> {
        let (inserted_lines, deleted_lines) = self
            .diff_handle()
            .map_or(Some((0, 0)), |diff_handle| {
                diff_handle
                    .load()
                    .hunks_intersecting_line_ranges(std::iter::once((0, cursor_line as usize)))
                    .try_fold(
                        (0, 0),
                        |(total_inserted_lines, total_deleted_lines), hunk| {
                            (hunk.after.start > cursor_line || hunk.after.end <= cursor_line)
                                .then_some((
                                    total_inserted_lines + (hunk.after.end - hunk.after.start),
                                    total_deleted_lines + (hunk.before.end - hunk.before.start),
                                ))
                        },
                    )
            })
            .ok_or(LineBlameError::NotCommittedYet)?;

        let file_blame = match &self.file_blame {
            None => return Err(LineBlameError::NotReadyYet),
            Some(result) => match result {
                Err(err) => {
                    return Err(LineBlameError::NoFileBlame(
                        cursor_line.saturating_add(1),
                        err,
                    ));
                }
                Ok(file_blame) => file_blame,
            },
        };

        Ok(file_blame
            .blame_for_line(cursor_line, inserted_lines, deleted_lines)
            .parse_format(format))
    }
}
