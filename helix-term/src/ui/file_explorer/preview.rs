use std::{collections::VecDeque, num::NonZeroUsize};

use helix_view::{editor::DetachedPreviewDocument, DocumentId};

use super::ExplorerPath;

const PREVIEW_DOCUMENT_CACHE_LIMIT: NonZeroUsize =
    NonZeroUsize::new(4).expect("preview cache limit must be non-zero");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ExplorerPreview {
    #[default]
    None,
    Owned(DocumentId),
}

#[derive(Default)]
pub(super) struct PreviewDocumentCache {
    entries: VecDeque<PreviewDocumentCacheEntry>,
}

struct PreviewDocumentCacheEntry {
    path: ExplorerPath,
    document: DetachedPreviewDocument,
}

impl PreviewDocumentCache {
    pub(super) fn take(&mut self, path: &ExplorerPath) -> Option<DetachedPreviewDocument> {
        let position = self.entries.iter().position(|entry| &entry.path == path)?;
        self.entries.remove(position).map(|entry| entry.document)
    }

    pub(super) fn insert(&mut self, path: ExplorerPath, document: DetachedPreviewDocument) {
        self.entries.retain(|entry| entry.path != path);
        self.entries
            .push_front(PreviewDocumentCacheEntry { path, document });
        self.truncate();
    }

    fn truncate(&mut self) {
        while self.entries.len() > PREVIEW_DOCUMENT_CACHE_LIMIT.get() {
            self.entries.pop_back();
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(super) fn contains_path(&self, path: &ExplorerPath) -> bool {
        self.entries.iter().any(|entry| &entry.path == path)
    }
}
