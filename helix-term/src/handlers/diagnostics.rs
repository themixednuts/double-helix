use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_core::syntax::config::LanguageServerFeature;
use helix_event::register_hook;
use helix_lsp::{lsp, LanguageServerId};
use helix_runtime::{send_blocking, Clock, Debounce, Runtime, Work};
use helix_view::bench::log_command_phase;
use helix_view::document::Mode;
use helix_view::events::{
    DiagnosticsDidChange, DocumentDidChange, DocumentDidOpen, LanguageServerInitialized,
};
use helix_view::handlers::lsp::{PullAllDocumentsDiagnosticsEvent, PullDiagnosticsEvent};
use helix_view::handlers::Handlers;
use helix_view::DocumentId;

use crate::{
    effect::language_server::request_document_diagnostics,
    events::OnModeSwitch,
    runtime::{send_task_event_with, RuntimeEvent, RuntimeTaskEvent},
};

pub(super) fn register_hooks(handlers: &Handlers, ingress: helix_runtime::Sender<RuntimeEvent>) {
    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        if event.editor.mode != Mode::Insert {
            for (view, _) in event.editor.tree.views_mut() {
                view.diagnostics_handler.refresh()
            }
        }
        Ok(())
    });
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        for (view, _) in event.cx.editor.tree.views_mut() {
            view.diagnostics_handler.active = event.new_mode != Mode::Insert;
        }
        Ok(())
    });

    let tx = handlers.pull_diagnostics.clone();
    let tx_all_documents = handlers.pull_all_documents_diagnostics.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        let hook_start = std::time::Instant::now();
        if event
            .doc
            .has_language_server_with_feature(LanguageServerFeature::PullDiagnostics)
            && !event.ghost_transaction
        {
            // Cancel the ongoing request, if present.
            event.doc.cancel_pull_diagnostics();
            let document_id = event.doc.id();
            send_blocking(&tx, PullDiagnosticsEvent { document_id });

            let inter_file_dependencies_language_servers = event
                .doc
                .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
                .filter(|language_server| {
                    language_server
                        .capabilities()
                        .diagnostic_provider
                        .as_ref()
                        .is_some_and(|diagnostic_provider| match diagnostic_provider {
                            lsp::DiagnosticServerCapabilities::Options(options) => {
                                options.inter_file_dependencies
                            }

                            lsp::DiagnosticServerCapabilities::RegistrationOptions(options) => {
                                options.diagnostic_options.inter_file_dependencies
                            }
                        })
                })
                .map(|language_server| language_server.id())
                .collect();

            send_blocking(
                &tx_all_documents,
                PullAllDocumentsDiagnosticsEvent {
                    language_servers: inter_file_dependencies_language_servers,
                },
            );
        }
        let hook_dur = hook_start.elapsed();
        log_command_phase(
            "document_did_change_hook",
            "diagnostics_pull",
            hook_dur,
            || {
                format!(
                    "doc_id={:?} ghost={} lines={} bytes={}",
                    event.doc.id(),
                    event.ghost_transaction,
                    event.doc.text().len_lines(),
                    event.doc.text().len_bytes()
                )
            },
        );
        Ok(())
    });

    let open_ingress = ingress.clone();
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        request_document_diagnostics(event.editor, event.doc, open_ingress.clone());

        Ok(())
    });

    let init_ingress = ingress;
    register_hook!(move |event: &mut LanguageServerInitialized<'_>| {
        let doc_ids: Vec<_> = event.editor.documents.keys().copied().collect();

        for doc_id in doc_ids {
            request_document_diagnostics(event.editor, doc_id, init_ingress.clone());
        }

        Ok(())
    });
}

#[derive(Debug)]
pub(super) struct PullDiagnosticsHandler {
    document_ids: Arc<Mutex<HashSet<DocumentId>>>,
    debounce: Debounce,
    work: Work,
    clock: Clock,
    ingress: helix_runtime::Sender<RuntimeEvent>,
}

impl PullDiagnosticsHandler {
    fn new(work: Work, clock: Clock, ingress: helix_runtime::Sender<RuntimeEvent>) -> Self {
        Self {
            document_ids: Default::default(),
            debounce: Debounce::new(Duration::from_millis(250)),
            work,
            clock,
            ingress,
        }
    }

    fn event(&mut self, event: PullDiagnosticsEvent) {
        self.document_ids
            .lock()
            .expect("pull diagnostics lock poisoned")
            .insert(event.document_id);
        let document_ids = self.document_ids.clone();
        let ingress = self.ingress.clone();
        self.debounce.restart(&self.work, &self.clock, async move {
            let document_ids = {
                let mut document_ids = document_ids.lock().expect("pull diagnostics lock poisoned");
                std::mem::take(&mut *document_ids)
            };
            if !document_ids.is_empty() {
                send_task_event_with(
                    RuntimeTaskEvent::PullDiagnosticsDebounced { document_ids },
                    ingress,
                )
                .await;
            }
        });
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: helix_runtime::Sender<RuntimeEvent>,
    ) -> helix_runtime::Sender<PullDiagnosticsEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone().spawn(async move {
            let mut handler = PullDiagnosticsHandler::new(work, clock, ingress);
            while let Some(event) = rx.recv().await {
                handler.event(event);
            }
            handler.debounce.cancel();
        }).detach();
        tx
    }
}

#[derive(Debug)]
pub(super) struct PullAllDocumentsDiagnosticHandler {
    language_servers: Arc<Mutex<HashSet<LanguageServerId>>>,
    debounce: Debounce,
    work: Work,
    clock: Clock,
    ingress: helix_runtime::Sender<RuntimeEvent>,
}

impl PullAllDocumentsDiagnosticHandler {
    fn new(work: Work, clock: Clock, ingress: helix_runtime::Sender<RuntimeEvent>) -> Self {
        Self {
            language_servers: Default::default(),
            debounce: Debounce::new(Duration::from_secs(1)),
            work,
            clock,
            ingress,
        }
    }

    fn event(&mut self, event: PullAllDocumentsDiagnosticsEvent) {
        self.language_servers
            .lock()
            .expect("pull all diagnostics lock poisoned")
            .extend(event.language_servers);
        let language_servers = self.language_servers.clone();
        let ingress = self.ingress.clone();
        self.debounce.restart(&self.work, &self.clock, async move {
            let language_servers = {
                let mut language_servers = language_servers
                    .lock()
                    .expect("pull all diagnostics lock poisoned");
                std::mem::take(&mut *language_servers)
            };
            if !language_servers.is_empty() {
                send_task_event_with(
                    RuntimeTaskEvent::PullAllDocumentsDiagnosticsDebounced { language_servers },
                    ingress,
                )
                .await;
            }
        });
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: helix_runtime::Sender<RuntimeEvent>,
    ) -> helix_runtime::Sender<PullAllDocumentsDiagnosticsEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone().spawn(async move {
            let mut handler = PullAllDocumentsDiagnosticHandler::new(work, clock, ingress);
            while let Some(event) = rx.recv().await {
                handler.event(event);
            }
            handler.debounce.cancel();
        }).detach();
        tx
    }
}

