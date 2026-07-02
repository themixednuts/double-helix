use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use helix_runtime::{Runtime, Work};
use helix_view::bench::log_command_phase;
use helix_view::{
    handlers::{lsp::DocumentColorsEvent, Handlers},
    DocumentId,
};

use crate::{effect::language_server::request_document_colors, runtime::RuntimeTaskEvent};

pub(super) struct DocumentColorsHandler {
    docs: Arc<Mutex<HashSet<DocumentId>>>,
    debouncer: crate::runtime::RuntimeTaskDebouncer,
}

const DOCUMENT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

impl DocumentColorsHandler {
    fn new(
        work: Work,
        clock: helix_runtime::Clock,
        ingress: crate::runtime::RuntimeIngress,
    ) -> Self {
        Self {
            docs: Default::default(),
            debouncer: crate::runtime::RuntimeTaskDebouncer::new(
                DOCUMENT_CHANGE_DEBOUNCE,
                work,
                clock,
                ingress,
            ),
        }
    }

    fn event(&mut self, event: DocumentColorsEvent) {
        let DocumentColorsEvent(doc_id) = event;
        self.docs
            .lock()
            .expect("document colors lock poisoned")
            .insert(doc_id);

        let docs = self.docs.clone();
        self.debouncer
            .send_after_with(DOCUMENT_CHANGE_DEBOUNCE, move || {
                let doc_ids = {
                    let mut docs = docs.lock().expect("document colors lock poisoned");
                    std::mem::take(&mut *docs)
                };
                (!doc_ids.is_empty())
                    .then(|| RuntimeTaskEvent::RequestDocumentColorsDebounced { doc_ids })
            });
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<DocumentColorsEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                let mut handler = DocumentColorsHandler::new(work, clock, ingress);
                while let Some(event) = rx.recv().await {
                    handler.event(event);
                }
                handler.debouncer.cancel();
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

    let tx = handlers.document_colors.clone();
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
            helix_runtime::send_blocking(&tx, DocumentColorsEvent(event.doc.id()));
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

    let init_ingress = ingress.clone();
    editor
        .lifecycle()
        .on_language_server_initialized(move |event| {
            let doc_ids: Vec<_> = event.editor.documents().map(|doc| doc.id()).collect();

            for doc_id in doc_ids {
                request_document_colors(event.editor, doc_id, init_ingress.clone());
            }

            Ok(())
        });

    let exit_ingress = ingress;
    editor.lifecycle().on_language_server_exited(move |event| {
        // Clear and re-request all color swatches when a server exits.
        for doc in event.editor.documents_mut() {
            if doc.supports_language_server(event.server_id) {
                doc.clear_color_swatches();
            }
        }

        let doc_ids: Vec<_> = event.editor.documents().map(|doc| doc.id()).collect();

        for doc_id in doc_ids {
            request_document_colors(event.editor, doc_id, exit_ingress.clone());
        }

        Ok(())
    });
}
