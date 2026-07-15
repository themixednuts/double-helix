use std::path::{Path, PathBuf};

use crate::DocumentId;

use super::Editor;

impl Editor {
    pub fn file_operation_language_servers(&self) -> Vec<std::sync::Arc<helix_lsp::Client>> {
        self.language_servers
            .iter_clients()
            .filter(|client| client.is_initialized())
            .cloned()
            .collect()
    }

    pub fn file_operation_path_changed(&mut self, path: PathBuf) {
        self.language_servers.file_event_handler.file_changed(path);
    }

    /// Update a document after a completed file operation. File-system work,
    /// LSP `will*`, and notifications are owned by the terminal operation
    /// pipeline; this method only maintains editor document identity.
    pub fn set_doc_path(&mut self, doc_id: DocumentId, path: &Path) {
        let doc = doc_mut!(self, &doc_id);
        let old_path = doc.path();

        if let Some(old_path) = old_path {
            if old_path == path {
                return;
            }
            for language_server in doc.language_servers() {
                language_server.text_document_did_close(doc.identifier());
            }
        }
        doc.clear_language_servers();
        doc.set_path(Some(path));
        doc.detect_editor_config();
        self.refresh_doc_language(doc_id)
    }
}
