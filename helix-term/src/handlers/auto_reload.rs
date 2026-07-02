use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::runtime::{send_task_event_with, RuntimeTaskEvent};
use helix_runtime::{send_blocking, Runtime, Work};
use helix_view::file_watcher::FileWatcher;
use helix_view::handlers::{AutoReloadEvent, Handlers};
use helix_view::Editor;

#[derive(Debug)]
pub(super) struct AutoReloadHandler {
    reload_pending: Arc<AtomicBool>,
    work: Work,
    ingress: crate::runtime::RuntimeIngress,
}

impl AutoReloadHandler {
    fn new(work: Work, ingress: crate::runtime::RuntimeIngress) -> AutoReloadHandler {
        AutoReloadHandler {
            reload_pending: Default::default(),
            work,
            ingress,
        }
    }

    fn event(&mut self, event: AutoReloadEvent) {
        match event {
            AutoReloadEvent::FileChanged { .. } => {
                let reload_pending = self.reload_pending.clone();
                let ingress = self.ingress.clone();
                self.work
                    .spawn(async move {
                        send_task_event_with(
                            RuntimeTaskEvent::AutoReloadRun { reload_pending },
                            ingress,
                        )
                        .await;
                    })
                    .detach();
            }
            AutoReloadEvent::LeftInsertMode => {
                if self.reload_pending.load(Ordering::Relaxed) {
                    let reload_pending = self.reload_pending.clone();
                    let ingress = self.ingress.clone();
                    self.work
                        .spawn(async move {
                            send_task_event_with(
                                RuntimeTaskEvent::AutoReloadRun { reload_pending },
                                ingress,
                            )
                            .await;
                        })
                        .detach();
                }
            }
        }
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<AutoReloadEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        work.clone()
            .spawn(async move {
                let mut handler = AutoReloadHandler::new(work, ingress);
                while let Some(event) = rx.recv().await {
                    handler.event(event);
                }
            })
            .detach();
        tx
    }
}

/// Initialize the file watcher and spawn the bridge task that forwards
/// notify events into the auto-reload handler channel.
pub fn setup_file_watcher(editor: &mut Editor) {
    if !editor.config().auto_reload {
        return;
    }

    let (watcher, mut rx) = match FileWatcher::new() {
        Ok(pair) => pair,
        Err(e) => {
            log::warn!("Failed to initialize file watcher: {e}");
            return;
        }
    };

    editor.file_watcher = Some(watcher);

    let tx = editor.auto_reload_sender().clone();
    editor
        .work()
        .spawn(async move {
            while let Some(event) = rx.recv().await {
                send_blocking(
                    &tx,
                    AutoReloadEvent::FileChanged {
                        path: event.path,
                        doc_ids: event.doc_ids,
                    },
                );
            }
        })
        .detach();
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
