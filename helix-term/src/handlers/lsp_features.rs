use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use helix_runtime::{Runtime, Work};
use helix_view::{
    handlers::{
        lsp::{LspFeatureRefreshEvent, LspFeatureRefreshKind},
        Handlers,
    },
    DocumentId,
};

use crate::{
    effect::language_server::{
        request_code_lenses, request_document_links, request_folding_ranges,
    },
    runtime::RuntimeTaskEvent,
};

const CODE_LENS_PLUGIN_SCOPE: &str = "helix-lsp-code-lens";

pub(super) struct LspFeatureRefreshHandler {
    docs: Arc<Mutex<HashMap<DocumentId, HashSet<LspFeatureRefreshKind>>>>,
    debouncer: crate::runtime::RuntimeTaskDebouncer,
}

const DOCUMENT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

impl LspFeatureRefreshHandler {
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

    fn event(&mut self, event: LspFeatureRefreshEvent) {
        self.docs
            .lock()
            .expect("lsp feature refresh lock poisoned")
            .entry(event.doc_id)
            .or_default()
            .insert(event.kind);

        let docs = self.docs.clone();
        self.debouncer
            .send_after_with(DOCUMENT_CHANGE_DEBOUNCE, move || {
                let docs = {
                    let mut docs = docs.lock().expect("lsp feature refresh lock poisoned");
                    std::mem::take(&mut *docs)
                };
                (!docs.is_empty()).then(|| RuntimeTaskEvent::RequestLspFeaturesDebounced { docs })
            });
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<LspFeatureRefreshEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                let mut handler = LspFeatureRefreshHandler::new(work, clock, ingress);
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
        request_code_lenses(event.editor, event.doc, open_ingress.clone());
        request_document_links(event.editor, event.doc, open_ingress.clone());
        request_folding_ranges(event.editor, event.doc, open_ingress.clone());
        Ok(())
    });

    let tx = handlers.lsp_feature_refresh.clone();
    editor.lifecycle().on_document_change(move |event| {
        if !event.ghost_transaction {
            event.doc.cancel_code_lenses();
            event.doc.cancel_document_links();
            event.doc.cancel_folding_ranges();
            for kind in [
                LspFeatureRefreshKind::CodeLens,
                LspFeatureRefreshKind::DocumentLinks,
                LspFeatureRefreshKind::FoldingRanges,
            ] {
                helix_runtime::send_blocking(
                    &tx,
                    LspFeatureRefreshEvent {
                        doc_id: event.doc.id(),
                        kind,
                    },
                );
            }
        }
        Ok(())
    });

    let init_ingress = ingress.clone();
    editor
        .lifecycle()
        .on_language_server_initialized(move |event| {
            let doc_ids: Vec<_> = event.editor.documents().map(|doc| doc.id()).collect();
            for doc_id in doc_ids {
                request_code_lenses(event.editor, doc_id, init_ingress.clone());
                request_document_links(event.editor, doc_id, init_ingress.clone());
                request_folding_ranges(event.editor, doc_id, init_ingress.clone());
            }
            Ok(())
        });

    editor.lifecycle().on_language_server_exited(move |event| {
        for doc in event.editor.documents_mut() {
            if doc.supports_language_server(event.server_id) {
                doc.clear_code_lenses();
                doc.clear_document_links();
                doc.clear_plugin_annotations(CODE_LENS_PLUGIN_SCOPE);
            }
        }
        Ok(())
    });
}
