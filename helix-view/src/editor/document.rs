use std::{
    fs,
    future::Future,
    io::{self, ErrorKind},
    path::Path,
};

use crate::DocumentId;
use helix_stdx::path::canonicalize;

use super::Editor;

impl Editor {
    fn apply_file_operation_edit(
        &mut self,
        language_server: &helix_lsp::Client,
        request: impl Future<Output = helix_lsp::Result<Option<helix_lsp::lsp::WorkspaceEdit>>>,
    ) {
        let edit = match helix_lsp::block_on(request) {
            Ok(edit) => edit.unwrap_or_default(),
            Err(err) => {
                log::error!("invalid file operation response: {err:?}");
                return;
            }
        };
        if let Err(err) = self.apply_workspace_edit(language_server.offset_encoding(), &edit) {
            log::error!("failed to apply workspace edit: {err:?}")
        }
    }

    pub fn create_path(&mut self, path: &Path, is_dir: bool) -> io::Result<()> {
        let path = canonicalize(path);
        let created = !path.exists();

        if created {
            let language_servers: Vec<_> = self
                .language_servers
                .iter_clients()
                .filter(|client| client.is_initialized())
                .cloned()
                .collect();
            for language_server in language_servers {
                let Some(request) = language_server.will_create(&path, is_dir) else {
                    continue;
                };
                self.apply_file_operation_edit(&language_server, request);
            }
        }

        if is_dir {
            fs::create_dir_all(&path)?;
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::File::create(&path)?;
        }

        if created {
            for ls in self.language_servers.iter_clients() {
                if !ls.is_initialized() {
                    continue;
                }
                ls.did_create(&path, is_dir);
            }
        }
        self.language_servers.file_event_handler.file_changed(path);
        Ok(())
    }

    pub fn copy_path(&mut self, old_path: &Path, new_path: &Path) -> io::Result<u64> {
        let new_path = canonicalize(new_path);
        if old_path.is_dir() {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "copying directories is not supported",
            ));
        }

        let created = !new_path.exists();
        if created {
            let language_servers: Vec<_> = self
                .language_servers
                .iter_clients()
                .filter(|client| client.is_initialized())
                .cloned()
                .collect();
            for language_server in language_servers {
                let Some(request) = language_server.will_create(&new_path, false) else {
                    continue;
                };
                self.apply_file_operation_edit(&language_server, request);
            }
        }

        if let Some(parent) = new_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = fs::copy(old_path, &new_path)?;

        if created {
            for ls in self.language_servers.iter_clients() {
                if !ls.is_initialized() {
                    continue;
                }
                ls.did_create(&new_path, false);
            }
        }
        self.language_servers
            .file_event_handler
            .file_changed(new_path);
        Ok(bytes)
    }

    pub fn delete_path(&mut self, path: &Path) -> io::Result<()> {
        let path = canonicalize(path);
        if !path.exists() {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!("path {} does not exist", path.display()),
            ));
        }

        let is_dir = path.is_dir();
        let language_servers: Vec<_> = self
            .language_servers
            .iter_clients()
            .filter(|client| client.is_initialized())
            .cloned()
            .collect();
        for language_server in language_servers {
            let Some(request) = language_server.will_delete(&path, is_dir) else {
                continue;
            };
            self.apply_file_operation_edit(&language_server, request);
        }

        if path.exists() {
            if is_dir {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }

        for ls in self.language_servers.iter_clients() {
            if !ls.is_initialized() {
                continue;
            }
            ls.did_delete(&path, is_dir);
        }
        self.language_servers.file_event_handler.file_changed(path);
        Ok(())
    }

    pub fn move_path(&mut self, old_path: &Path, new_path: &Path) -> io::Result<()> {
        let new_path = canonicalize(new_path);
        if old_path == new_path {
            return Ok(());
        }
        let is_dir = old_path.is_dir();
        let language_servers: Vec<_> = self
            .language_servers
            .iter_clients()
            .filter(|client| client.is_initialized())
            .cloned()
            .collect();
        for language_server in language_servers {
            let Some(request) = language_server.will_rename(old_path, &new_path, is_dir) else {
                continue;
            };
            self.apply_file_operation_edit(&language_server, request);
        }

        if old_path.exists() {
            fs::rename(old_path, &new_path)?;
        }

        if let Some(doc) = self.document_by_path(old_path) {
            self.set_doc_path(doc.id(), &new_path);
        }
        let is_dir = new_path.is_dir();
        for ls in self.language_servers.iter_clients() {
            if !ls.is_initialized() {
                continue;
            }
            ls.did_rename(old_path, &new_path, is_dir);
        }
        self.language_servers
            .file_event_handler
            .file_changed(old_path.to_owned());
        self.language_servers
            .file_event_handler
            .file_changed(new_path);
        Ok(())
    }

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
