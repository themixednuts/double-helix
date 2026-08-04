//! Session snapshot for the file explorer panel.
//!
//! Closing the explorer (Esc, or opening a file when `sticky = false`) discards
//! the compositor layer. This module keeps the tree UI state — expanded dirs,
//! selection, and scroll offsets — so reopening the same root restores it.

use std::{collections::HashSet, sync::Mutex};

use helix_view::editor::FileExplorerConfig;

use super::source::{ExplorerPath, ExplorerSourceId};

#[derive(Debug, Clone)]
pub(crate) struct FileExplorerUiState {
    pub root: ExplorerPath,
    pub source_id: ExplorerSourceId,
    pub expanded_dirs: HashSet<ExplorerPath>,
    pub selected_path: Option<ExplorerPath>,
    pub scroll: usize,
    pub scroll_x: u16,
    pub config: FileExplorerConfig,
}

static SESSION: Mutex<Option<FileExplorerUiState>> = Mutex::new(None);

pub(crate) fn stash(state: FileExplorerUiState) {
    if let Ok(mut guard) = SESSION.lock() {
        *guard = Some(state);
    }
}

pub(crate) fn take_matching(
    root: &ExplorerPath,
    source_id: &ExplorerSourceId,
) -> Option<FileExplorerUiState> {
    let Ok(mut guard) = SESSION.lock() else {
        return None;
    };
    match guard.as_ref() {
        Some(state) if state.root == *root && state.source_id == *source_id => guard.take(),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn clear() {
    if let Ok(mut guard) = SESSION.lock() {
        *guard = None;
    }
}
