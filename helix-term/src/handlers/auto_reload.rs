use std::sync::atomic::{self, AtomicBool, Ordering};
use std::sync::Arc;

use helix_core::command_line::Args;
use helix_event::{register_hook, send_blocking};
use helix_view::document::Mode;
use helix_view::events::{DocumentDidClose, DocumentDidOpen};
use helix_view::file_watcher::FileWatcher;
use helix_view::handlers::{AutoReloadEvent, Handlers};
use helix_view::{Document, Editor};
use tokio::time::Instant;

use crate::commands;
use crate::events::OnModeSwitch;
use crate::job::{self, Jobs};
use crate::ui::PromptEvent;

#[derive(Debug)]
pub(super) struct AutoReloadHandler {
    reload_pending: Arc<AtomicBool>,
}

impl AutoReloadHandler {
    pub fn new() -> AutoReloadHandler {
        AutoReloadHandler {
            reload_pending: Default::default(),
        }
    }
}

impl helix_event::AsyncHook for AutoReloadHandler {
    type Event = AutoReloadEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _existing_debounce: Option<Instant>,
    ) -> Option<Instant> {
        match event {
            Self::Event::FileChanged { .. } => {
                self.finish_debounce();
                None
            }
            Self::Event::LeftInsertMode => {
                if self.reload_pending.load(Ordering::Relaxed) {
                    self.finish_debounce();
                }
                None
            }
        }
    }

    fn finish_debounce(&mut self) {
        let reload_pending = self.reload_pending.clone();
        job::dispatch_blocking(move |editor, _compositor| {
            if editor.mode() == Mode::Insert {
                reload_pending.store(true, atomic::Ordering::Relaxed);
            } else {
                reload_changed_documents(editor);
                reload_pending.store(false, atomic::Ordering::Relaxed);
            }
        });
    }
}

/// Reload documents that have been modified externally.
/// Only reloads unmodified buffers; modified buffers are left alone.
fn reload_changed_documents(editor: &mut Editor) {
    if count_externally_modified_documents(editor.documents()) == 0 {
        return;
    }

    let mut cx = crate::compositor::Context {
        editor,
        scroll: None,
        jobs: &mut Jobs::new(),
        plugin_manager: None,
    };

    match commands::typed::reload_all(&mut cx, Args::default(), PromptEvent::Validate) {
        Ok(_) => cx.editor.set_status("Reloaded modified documents"),
        Err(err) => cx
            .editor
            .set_error(format!("Failed to reload document: {err}")),
    }
}

pub fn count_externally_modified_documents<'a>(docs: impl Iterator<Item = &'a Document>) -> usize {
    docs.filter(|doc| !doc.is_modified())
        .filter(|doc| {
            let last_saved_time = doc.get_last_saved_time();
            let Some(path) = doc.path() else {
                return false;
            };
            if let Ok(metadata) = std::fs::metadata(path) {
                if let Ok(modified_time) = metadata.modified() {
                    if modified_time > last_saved_time {
                        return true;
                    }
                }
            }
            false
        })
        .count()
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

    let tx = editor.handlers.auto_reload.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            send_blocking(
                &tx,
                AutoReloadEvent::FileChanged {
                    path: event.path,
                    doc_ids: event.doc_ids,
                },
            );
        }
    });
}

pub(super) fn register_hooks(handlers: &Handlers) {
    // Watch files when documents are opened
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
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
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if let Some(ref mut watcher) = event.editor.file_watcher {
            if let Some(path) = event.doc.path() {
                watcher.unwatch_file(path, event.doc.id());
            }
        }
        Ok(())
    });

    // Handle mode switch (defer reload in insert mode)
    let tx = handlers.auto_reload.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if event.old_mode == Mode::Insert {
            send_blocking(&tx, AutoReloadEvent::LeftInsertMode)
        }
        Ok(())
    });
}
