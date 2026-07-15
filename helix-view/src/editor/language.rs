use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

use crate::DocumentId;

use super::{language_server_supervisor::LaunchOrigin, Editor};

impl Editor {
    #[inline]
    pub fn language_server_by_id(
        &self,
        language_server_id: helix_lsp::LanguageServerId,
    ) -> Option<&helix_lsp::Client> {
        self.language_servers
            .get_by_id(language_server_id)
            .map(|client| &**client)
    }

    pub fn refresh_language_servers(&mut self, doc_id: DocumentId) {
        self.launch_language_servers(doc_id)
    }

    pub fn refresh_all_language_servers(&mut self) {
        let documents = self.documents.keys().copied().collect::<Vec<_>>();
        for document in documents {
            self.launch_language_servers(document);
        }
    }

    /// Reconciles open documents against one newly published runtime activation generation.
    pub fn reconcile_runtime_asset_change(&mut self, change: &helix_loader::RuntimeAssetsChange) {
        let changed_commands = change
            .changed_asset_keys
            .iter()
            .filter(|asset| asset.kind == helix_loader::RuntimeAssetKind::Command)
            .map(|asset| asset.key.as_str())
            .collect::<std::collections::HashSet<_>>();
        if changed_commands.is_empty() {
            return;
        }

        let loader = self.syn_loader.load();
        let affected = loader
            .language_server_configs()
            .iter()
            .filter(|(_, config)| changed_commands.contains(config.command.as_str()))
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        drop(loader);
        if affected.is_empty() {
            return;
        }
        self.restart_language_server_demands(&affected, LaunchOrigin::RuntimeChange);
    }

    pub fn remove_language_server(&mut self, language_server_id: helix_lsp::LanguageServerId) {
        self.handle_language_server_exit(language_server_id);
    }

    pub fn stop_language_server(&mut self, name: &str) {
        self.stop_language_server_demands(name);
    }

    pub fn restart_language_servers(&mut self, names: &HashSet<String>) {
        self.restart_language_server_demands(names, LaunchOrigin::ExplicitRestart);
    }

    pub fn mark_language_server_initialization_dispatched(
        &mut self,
        language_server_id: helix_lsp::LanguageServerId,
    ) -> bool {
        self.language_servers
            .mark_initialization_dispatched(language_server_id)
    }

    pub fn notify_file_changed(&self, path: std::path::PathBuf) {
        self.language_servers.file_event_handler.file_changed(path);
    }

    pub fn request_blame(&self, event: crate::handlers::BlameEvent) {
        self.handlers.blame.send(event);
    }

    pub fn clear_language_server_document_diagnostics(
        &mut self,
        language_server_id: helix_lsp::LanguageServerId,
    ) {
        for doc in self.documents_mut() {
            doc.clear_diagnostics_for_language_server(language_server_id);
        }
    }

    pub fn documents_supporting_language_server(
        &self,
        language_server_id: helix_lsp::LanguageServerId,
    ) -> Vec<DocumentId> {
        self.documents
            .values()
            .filter(|doc| doc.supports_language_server(language_server_id))
            .map(|doc| doc.id())
            .collect()
    }

    pub fn language_server_client(
        &self,
        language_server_id: helix_lsp::LanguageServerId,
    ) -> Option<&Arc<helix_lsp::Client>> {
        self.language_servers.get_by_id(language_server_id)
    }

    pub fn register_language_server_file_watch(
        &mut self,
        language_server_id: helix_lsp::LanguageServerId,
        client: &Arc<helix_lsp::Client>,
        registration_id: String,
        options: helix_lsp::lsp::DidChangeWatchedFilesRegistrationOptions,
    ) {
        self.language_servers.file_event_handler.register(
            language_server_id,
            Arc::downgrade(client),
            registration_id,
            options,
        );
    }

    pub fn unregister_language_server_file_watch(
        &mut self,
        language_server_id: helix_lsp::LanguageServerId,
        registration_id: String,
    ) {
        self.language_servers
            .file_event_handler
            .unregister(language_server_id, registration_id);
    }

    pub fn replace_language_loader(&mut self, loader: helix_core::syntax::Loader) {
        self.syn_loader.store(Arc::new(loader));
    }

    /// Installs a prepared loader and clears syntax only for documents backed by changed grammars.
    pub fn apply_runtime_language_loader(
        &mut self,
        loader: helix_core::syntax::Loader,
        changed_grammars: &BTreeSet<String>,
    ) -> Vec<DocumentId> {
        let affected = self
            .documents
            .iter()
            .filter_map(|(document_id, document)| {
                let language = document.language_configuration()?;
                let grammar = language.grammar.as_deref().unwrap_or(&language.language_id);
                changed_grammars.contains(grammar).then_some(*document_id)
            })
            .collect::<Vec<_>>();
        self.replace_language_loader(loader);
        let loader = self.syn_loader.load();
        for document_id in &affected {
            let Some(document) = self.documents.get_mut(document_id) else {
                continue;
            };
            let language = document.detect_language_config(&loader);
            document.set_language_configuration(language);
            document.set_syntax(None);
        }
        affected
    }

    pub fn refresh_document_languages(&mut self) {
        let loader = self.syn_loader.load();
        for document in self.documents.values_mut() {
            document.detect_editor_config();
            document.detect_language(&loader);
            let diagnostics =
                Editor::doc_diagnostics(&self.language_servers, &self.diagnostics, document);
            document.replace_diagnostics(diagnostics, &[], None);
        }
    }

    pub fn refresh_doc_language(&mut self, doc_id: DocumentId) {
        let loader = self.syn_loader.load();
        let doc = doc_mut!(self, &doc_id);
        doc.detect_language(&loader);
        doc.detect_editor_config();
        doc.detect_indent_and_line_ending();
        self.refresh_language_servers(doc_id);
        let doc = doc_mut!(self, &doc_id);
        let diagnostics = Editor::doc_diagnostics(&self.language_servers, &self.diagnostics, doc);
        doc.replace_diagnostics(diagnostics, &[], None);
        doc.reset_all_inlay_hints();
    }

    pub(super) fn launch_language_servers(&mut self, doc_id: DocumentId) {
        self.reconcile_language_server_demands(doc_id, LaunchOrigin::Automatic, None);
    }

    pub(super) fn handle_missing_language_server(
        &mut self,
        document: DocumentId,
        language: &helix_core::syntax::config::LanguageConfiguration,
        server: &str,
        runtime_generation: u64,
    ) {
        let loader = self.syn_loader.load();
        let Some(server_config) = loader.language_server_configs().get(server) else {
            return;
        };
        let config = self.config();
        self.handlers
            .pkg
            .send(crate::handlers::PkgEvent::MissingLanguageServer {
                documents: std::collections::BTreeSet::from([document]),
                server: server.to_owned(),
                language: language.language_id.clone(),
                command: server_config.command.clone(),
                config: config.pkg.clone(),
                config_generation: self.config_gen,
                runtime_generation,
            });
    }
}
