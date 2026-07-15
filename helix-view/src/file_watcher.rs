use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::DocumentId;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;

/// Tracks which directories are being watched and which files map to which documents.
///
/// Uses directory-level watches (non-recursive) since we only care about specific files.
/// Multiple documents in the same directory share one watch.
#[derive(Debug)]
struct WatchState {
    /// file path → set of document IDs with that path open
    file_to_docs: HashMap<PathBuf, Vec<DocumentId>>,
    /// directory → number of watched files in that directory
    dir_refcounts: HashMap<PathBuf, usize>,
}

impl WatchState {
    fn new() -> Self {
        Self {
            file_to_docs: HashMap::new(),
            dir_refcounts: HashMap::new(),
        }
    }

    fn all_documents(&self) -> Vec<DocumentId> {
        self.file_to_docs
            .values()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    state: Arc<Mutex<WatchState>>,
}

impl std::fmt::Debug for FileWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileWatcher")
            .field("state", &self.state)
            .finish()
    }
}

/// Semantically reduced events produced by the native watcher.
#[derive(Debug)]
pub enum FileWatcherEvent {
    Changed {
        path: PathBuf,
        doc_ids: Vec<DocumentId>,
    },
    /// Native watcher overflow or failure invalidates every watched document.
    Rescan { doc_ids: Vec<DocumentId> },
}

impl FileWatcher {
    /// Create a new file watcher and publish reduced events directly to its owner.
    ///
    /// The watcher uses OS-native mechanisms (ReadDirectoryChangesW on Windows,
    /// inotify on Linux, kqueue on macOS).
    pub fn new(publish: impl Fn(FileWatcherEvent) + Send + Sync + 'static) -> anyhow::Result<Self> {
        let state = Arc::new(Mutex::new(WatchState::new()));
        let callback_state = Arc::clone(&state);

        let watcher =
            notify::recommended_watcher(move |result: Result<notify::Event, notify::Error>| {
                let event = match result {
                    Ok(event) => event,
                    Err(error) => {
                        log::warn!(
                            "native file watcher invalidated; scheduling full rescan: {error}"
                        );
                        let doc_ids = callback_state.lock().all_documents();
                        if !doc_ids.is_empty() {
                            publish(FileWatcherEvent::Rescan { doc_ids });
                        }
                        return;
                    }
                };

                // We only care about modifications and creates (for atomic saves).
                use notify::EventKind;
                match event.kind {
                    EventKind::Modify(_) | EventKind::Create(_) => {}
                    _ => return,
                }

                for path in &event.paths {
                    // Canonicalize to handle symlinks and path normalization
                    let canonical = match path.canonicalize() {
                        Ok(p) => p,
                        Err(_) => path.clone(),
                    };
                    let doc_ids = callback_state.lock().file_to_docs.get(&canonical).cloned();
                    if let Some(doc_ids) = doc_ids {
                        publish(FileWatcherEvent::Changed {
                            path: canonical,
                            doc_ids,
                        });
                    }
                }
            })?;

        Ok(Self {
            _watcher: watcher,
            state,
        })
    }

    /// Start watching a file for changes. Associates it with the given document ID.
    pub fn watch_file(&mut self, path: &Path, doc_id: DocumentId) {
        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => path.to_path_buf(),
        };

        let mut state = self.state.lock();

        // Add file → doc mapping
        let doc_ids = state.file_to_docs.entry(canonical.clone()).or_default();
        if !doc_ids.contains(&doc_id) {
            doc_ids.push(doc_id);
        }

        // Watch the parent directory if not already watched
        if let Some(parent) = canonical.parent() {
            let count = state.dir_refcounts.entry(parent.to_path_buf()).or_insert(0);
            if *count == 0 {
                // New directory to watch
                if let Err(e) = self._watcher.watch(parent, RecursiveMode::NonRecursive) {
                    log::warn!("Failed to watch directory {}: {e}", parent.display());
                }
            }
            *count += 1;
        }
    }

    /// Stop watching a file for a specific document. If no more documents reference
    /// the file's directory, the directory watch is removed.
    pub fn unwatch_file(&mut self, path: &Path, doc_id: DocumentId) {
        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => path.to_path_buf(),
        };

        let mut state = self.state.lock();

        // Remove doc from file mapping
        let should_remove_file = if let Some(doc_ids) = state.file_to_docs.get_mut(&canonical) {
            doc_ids.retain(|id| *id != doc_id);
            doc_ids.is_empty()
        } else {
            false
        };

        if should_remove_file {
            state.file_to_docs.remove(&canonical);

            // Decrement directory refcount
            if let Some(parent) = canonical.parent() {
                if let Some(count) = state.dir_refcounts.get_mut(&parent.to_path_buf()) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        state.dir_refcounts.remove(&parent.to_path_buf());
                        let _ = self._watcher.unwatch(parent);
                    }
                }
            }
        }
    }
}
