use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use crate::runtime::{send_task_event_with, RuntimeTaskEvent};
use helix_runtime::Runtime;
use helix_view::file_watcher::{FileWatcher, FileWatcherEvent};
use helix_view::handlers::{AutoReloadEvent, Handlers};
use helix_view::Editor;

#[derive(Debug)]
pub(super) struct AutoReloadHandler {
    reload_pending: Arc<Mutex<BTreeSet<helix_view::DocumentId>>>,
    ingress: crate::runtime::RuntimeIngress,
}

impl AutoReloadHandler {
    fn new(ingress: crate::runtime::RuntimeIngress) -> AutoReloadHandler {
        AutoReloadHandler {
            reload_pending: Default::default(),
            ingress,
        }
    }

    async fn request_reload(&self, documents: BTreeSet<helix_view::DocumentId>) {
        send_task_event_with(
            RuntimeTaskEvent::AutoReloadRun {
                documents,
                reload_pending: self.reload_pending.clone(),
            },
            self.ingress.clone(),
        )
        .await;
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<AutoReloadEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        work.clone()
            .spawn(async move {
                let handler = AutoReloadHandler::new(ingress);
                while let Some(event) = rx.recv().await {
                    let mut documents = BTreeSet::new();
                    let mut left_insert_mode = matches!(event, AutoReloadEvent::LeftInsertMode);
                    if let AutoReloadEvent::DocumentsChanged { doc_ids } = event {
                        documents.extend(doc_ids);
                    }
                    while let Ok(event) = rx.try_recv() {
                        left_insert_mode |= matches!(event, AutoReloadEvent::LeftInsertMode);
                        if let AutoReloadEvent::DocumentsChanged { doc_ids } = event {
                            documents.extend(doc_ids);
                        }
                    }

                    if !documents.is_empty() || left_insert_mode {
                        handler.request_reload(documents).await;
                    }
                }
            })
            .detach();
        tx
    }
}

/// Initialize the file watcher and publish directly into the auto-reload reducer.
pub fn setup_file_watcher(editor: &mut Editor) {
    if !editor.config().auto_reload {
        return;
    }

    let tx = editor.auto_reload_sender().clone();
    let watcher = match FileWatcher::new(move |event| {
        let doc_ids = match event {
            FileWatcherEvent::Changed { path, doc_ids } => {
                log::trace!("watched file changed: {}", path.display());
                doc_ids
            }
            FileWatcherEvent::Rescan { doc_ids } => doc_ids,
        };
        tx.send(AutoReloadEvent::DocumentsChanged { doc_ids });
    }) {
        Ok(watcher) => watcher,
        Err(e) => {
            log::warn!("Failed to initialize file watcher: {e}");
            return;
        }
    };

    editor.file_watcher = Some(watcher);
}

pub(super) fn attach(editor: &helix_view::Editor, _handlers: &Handlers) {
    // Watch files when documents are opened
    editor.lifecycle().on_document_open(move |event| {
        if let Some(ref mut watcher) = event.editor.file_watcher {
            if let Some(doc) = event.editor.documents.get(&event.doc) {
                if let Some(path) = doc.path() {
                    watcher.watch_file(path, event.doc);
                }
            }
        }
        Ok(())
    });

    // Unwatch files when documents are closed
    editor.lifecycle().on_document_close(move |event| {
        if let Some(ref mut watcher) = event.editor.file_watcher {
            if let Some(path) = event.doc.path() {
                watcher.unwatch_file(path, event.doc.id());
            }
        }
        Ok(())
    });
}
