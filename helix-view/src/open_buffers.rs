//! The text of open documents that changed since they were loaded or last saved, readable off the
//! main thread. Assistant agents read files through it, so they see the user's unsaved edits
//! rather than the stale file on disk.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use helix_core::Rope;
use parking_lot::RwLock;

#[derive(Clone, Default)]
pub struct OpenBuffers {
    texts: Arc<RwLock<HashMap<PathBuf, Rope>>>,
}

impl std::fmt::Debug for OpenBuffers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenBuffers")
            .field("len", &self.texts.read().len())
            .finish()
    }
}

impl OpenBuffers {
    /// The unsaved text of `path`, if an open document holds edits to it.
    pub fn text(&self, path: &Path) -> Option<Rope> {
        let path = helix_stdx::path::canonicalize(path);
        self.texts.read().get(&path).cloned()
    }

    /// Record `text` as the current content of the document at `path`. Ropes share structure,
    /// so this is cheap enough to run on every edit.
    pub(crate) fn changed(&self, path: &Path, text: &Rope) {
        self.texts.write().insert(path.to_path_buf(), text.clone());
    }

    /// The document at `path` matches the file on disk again (saved) or is gone (closed).
    pub(crate) fn forget(&self, path: &Path) {
        self.texts.write().remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_forgets_unsaved_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = helix_stdx::path::canonicalize(dir.path().join("a.rs"));
        let buffers = OpenBuffers::default();
        assert!(buffers.text(&path).is_none());

        buffers.changed(&path, &Rope::from("unsaved"));
        assert_eq!(buffers.text(&path).unwrap(), "unsaved");
        // Lookups normalize the path the way documents store theirs.
        let spelled = dir.path().join(".").join("a.rs");
        assert_eq!(buffers.text(&spelled).unwrap(), "unsaved");

        buffers.forget(&path);
        assert!(buffers.text(&path).is_none());
    }
}
