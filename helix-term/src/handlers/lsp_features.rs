use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use helix_runtime::Runtime;
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
        request_semantic_tokens,
    },
    runtime::RuntimeTaskEvent,
};

const CODE_LENS_PLUGIN_SCOPE: &str = "helix-lsp-code-lens";

pub(super) struct LspFeatureRefreshHandler {
    docs: HashMap<DocumentId, HashSet<LspFeatureRefreshKind>>,
    deadline: Option<Instant>,
    clock: helix_runtime::Clock,
    ingress: crate::runtime::RuntimeIngress,
}

const DOCUMENT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

impl LspFeatureRefreshHandler {
    fn new(clock: helix_runtime::Clock, ingress: crate::runtime::RuntimeIngress) -> Self {
        Self {
            docs: Default::default(),
            deadline: None,
            clock,
            ingress,
        }
    }

    fn event(&mut self, event: LspFeatureRefreshEvent) {
        self.docs
            .entry(event.doc_id)
            .or_default()
            .insert(event.kind);
        self.deadline = Some(self.clock.deadline_after(DOCUMENT_CHANGE_DEBOUNCE));
    }

    async fn flush(&mut self) {
        let docs = std::mem::take(&mut self.docs);
        if !docs.is_empty() {
            let _ = self
                .ingress
                .send_task(RuntimeTaskEvent::RequestLspFeaturesDebounced { docs })
                .await;
        }
    }

    async fn run(mut self, mut rx: helix_runtime::Receiver<LspFeatureRefreshEvent>) {
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
    ) -> helix_runtime::Sender<LspFeatureRefreshEvent> {
        let (tx, rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                LspFeatureRefreshHandler::new(clock, ingress).run(rx).await;
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
        request_semantic_tokens(event.editor, event.doc, open_ingress.clone());
        Ok(())
    });

    let refreshes = handlers.lsp_feature_refresh.clone();
    editor.lifecycle().on_document_change(move |event| {
        if !event.ghost_transaction {
            event.doc.cancel_code_lenses();
            event.doc.cancel_document_links();
            event.doc.cancel_folding_ranges();
            event.doc.cancel_semantic_tokens();
            event.doc.cancel_inline_completion();
            for kind in [
                LspFeatureRefreshKind::CodeLens,
                LspFeatureRefreshKind::DocumentLinks,
                LspFeatureRefreshKind::FoldingRanges,
                LspFeatureRefreshKind::SemanticTokens,
                LspFeatureRefreshKind::InlineCompletion,
            ] {
                refreshes.send(LspFeatureRefreshEvent {
                    doc_id: event.doc.id(),
                    kind,
                });
            }
        }
        Ok(())
    });

    let init_refreshes = handlers.lsp_feature_refresh.clone();
    editor
        .lifecycle()
        .on_language_server_initialized(move |event| {
            let doc_ids = event
                .editor
                .documents_supporting_language_server(event.server_id);
            for doc_id in doc_ids {
                for kind in [
                    LspFeatureRefreshKind::CodeLens,
                    LspFeatureRefreshKind::DocumentLinks,
                    LspFeatureRefreshKind::FoldingRanges,
                    LspFeatureRefreshKind::SemanticTokens,
                ] {
                    init_refreshes.send(LspFeatureRefreshEvent { doc_id, kind });
                }
            }
            Ok(())
        });

    editor.lifecycle().on_language_server_exited(move |event| {
        for doc in event.editor.documents_mut() {
            if doc.supports_language_server(event.server_id) {
                doc.clear_code_lenses();
                doc.clear_document_links();
                doc.clear_semantic_tokens();
                doc.clear_inline_completion();
                doc.clear_plugin_annotations(CODE_LENS_PLUGIN_SCOPE);
            }
        }
        Ok(())
    });
}
