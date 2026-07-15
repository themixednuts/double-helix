use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use helix_runtime::Runtime;
use helix_view::bench::log_command_phase;
use helix_view::{
    handlers::{lsp::DocumentColorsEvent, Handlers},
    DocumentId,
};

use crate::{effect::language_server::request_document_colors, runtime::RuntimeTaskEvent};

pub(super) struct DocumentColorsHandler {
    docs: HashSet<DocumentId>,
    deadline: Option<Instant>,
    clock: helix_runtime::Clock,
    ingress: crate::runtime::RuntimeIngress,
}

const DOCUMENT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

impl DocumentColorsHandler {
    fn new(clock: helix_runtime::Clock, ingress: crate::runtime::RuntimeIngress) -> Self {
        Self {
            docs: Default::default(),
            deadline: None,
            clock,
            ingress,
        }
    }

    fn event(&mut self, event: DocumentColorsEvent) {
        let DocumentColorsEvent(doc_id) = event;
        self.docs.insert(doc_id);
        self.deadline = Some(self.clock.deadline_after(DOCUMENT_CHANGE_DEBOUNCE));
    }

    async fn flush(&mut self) {
        let doc_ids = std::mem::take(&mut self.docs);
        if !doc_ids.is_empty() {
            let _ = self
                .ingress
                .send_task(RuntimeTaskEvent::RequestDocumentColorsDebounced { doc_ids })
                .await;
        }
    }

    async fn run(mut self, mut rx: helix_runtime::Receiver<DocumentColorsEvent>) {
        loop {
            if let Some(deadline) = self.deadline {
                let mut timer = self.clock.timer_at(deadline);
                tokio::select! {
                    biased;
                    event = rx.recv() => {
                        let Some(event) = event else { break };
                        self.event(event);
                    }
                    _ = &mut timer => {
                        self.deadline = None;
                        self.flush().await;
                    }
                }
            } else {
                let Some(event) = rx.recv().await else { break };
                self.event(event);
            }
        }
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<DocumentColorsEvent> {
        let (tx, rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                DocumentColorsHandler::new(clock, ingress).run(rx).await;
            })
            .detach();
        tx
    }
}

pub(super) fn attach(
    editor: &helix_view::Editor,
    handlers: &Handlers,
    ingress: crate::runtime::RuntimeIngress,
) {
    let open_ingress = ingress.clone();
    editor.lifecycle().on_document_open(move |event| {
        // when a document is initially opened, request colors for it
        request_document_colors(event.editor, event.doc, open_ingress.clone());

        Ok(())
    });

    let changes = handlers.document_colors.clone();
    editor.lifecycle().on_document_change(move |event| {
        let hook_start = std::time::Instant::now();
        // Update the color swatch positions so they stay aligned with edits.
        event.doc.update_color_swatches(event.changes);

        // Avoid re-requesting document colors if the change is a ghost transaction (completion)
        // because the language server will not know about the updates to the document and will
        // give out-of-date locations.
        if !event.ghost_transaction {
            // Cancel the ongoing request, if present.
            event.doc.cancel_color_swatches();
            changes.send(DocumentColorsEvent(event.doc.id()));
        }

        let hook_dur = hook_start.elapsed();
        log_command_phase(
            "document_did_change_hook",
            "document_colors",
            hook_dur,
            || {
                format!(
                    "doc_id={:?} ghost={} lines={} bytes={} has_swatches={}",
                    event.doc.id(),
                    event.ghost_transaction,
                    event.doc.text().len_lines(),
                    event.doc.text().len_bytes(),
                    event.doc.color_swatches().is_some()
                )
            },
        );
        Ok(())
    });

    let init_changes = handlers.document_colors.clone();
    editor
        .lifecycle()
        .on_language_server_initialized(move |event| {
            for doc_id in event
                .editor
                .documents_supporting_language_server(event.server_id)
            {
                init_changes.send(DocumentColorsEvent(doc_id));
            }

            Ok(())
        });

    let exit_changes = handlers.document_colors.clone();
    editor.lifecycle().on_language_server_exited(move |event| {
        // Clear and re-request all color swatches when a server exits.
        let document_ids = event
            .editor
            .documents_mut()
            .filter_map(|doc| {
                if !doc.supports_language_server(event.server_id) {
                    return None;
                }
                doc.clear_color_swatches();
                Some(doc.id())
            })
            .collect::<Vec<_>>();

        for doc_id in document_ids {
            exit_changes.send(DocumentColorsEvent(doc_id));
        }

        Ok(())
    });
}
