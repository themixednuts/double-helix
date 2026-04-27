use std::{collections::HashMap, sync::Arc};

use crate::DocumentId;

use super::Editor;

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

    pub fn remove_language_server(&mut self, language_server_id: helix_lsp::LanguageServerId) {
        self.language_servers.remove_by_id(language_server_id);
    }

    pub fn stop_language_server(&mut self, name: &str) {
        self.language_servers.stop(name);
    }

    pub fn notify_file_changed(&self, path: std::path::PathBuf) {
        self.language_servers.file_event_handler.file_changed(path);
    }

    pub fn request_blame(&self, event: crate::handlers::BlameEvent) {
        helix_runtime::send_blocking(&self.handlers.blame, event);
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
        if !self.config().lsp.enable {
            return;
        }
        let Some(doc) = self.documents.get_mut(&doc_id) else {
            return;
        };
        let Some(doc_url) = doc.url() else {
            return;
        };
        let (lang, path) = (doc.language_configuration().cloned(), doc.path().cloned());
        let config = doc.config.load();
        let root_dirs = &config.workspace_lsp_roots;

        let language_servers = lang.as_ref().map_or_else(HashMap::default, |language| {
            self.language_servers
                .get(language, path.as_ref(), root_dirs, config.lsp.snippets)
                .filter_map(|(lang, client)| match client {
                    Ok(client) => Some((lang, client)),
                    Err(err) => {
                        if let helix_lsp::Error::ExecutableNotFound(err) = err {
                            log::debug!(
                                "Language server not found for `{}` {} {}",
                                language.scope,
                                lang,
                                err,
                            );
                        } else {
                            log::error!(
                                "Failed to initialize the language servers for `{}` - `{}` {{ {} }}",
                                language.scope,
                                lang,
                                err
                            );
                        }
                        None
                    }
                })
                .collect::<HashMap<_, _>>()
        });

        if language_servers.is_empty() && !doc.has_language_servers() {
            return;
        }

        let language_id = doc.language_id().map(ToOwned::to_owned).unwrap_or_default();

        let doc_language_servers_not_in_registry = doc.all_language_servers().filter(|doc_ls| {
            language_servers
                .get(doc_ls.name())
                .is_none_or(|language_server| language_server.id() != doc_ls.id())
        });

        for language_server in doc_language_servers_not_in_registry {
            language_server.text_document_did_close(doc.identifier());
        }

        let language_servers_not_in_doc =
            language_servers.iter().filter(|(name, language_server)| {
                doc.language_server_by_name(name)
                    .is_none_or(|doc_ls| language_server.id() != doc_ls.id())
            });

        for (_, language_server) in language_servers_not_in_doc {
            language_server.text_document_did_open(
                doc_url.clone(),
                doc.version(),
                doc.text(),
                language_id.clone(),
            );
        }

        doc.set_language_servers(language_servers);
    }
}
