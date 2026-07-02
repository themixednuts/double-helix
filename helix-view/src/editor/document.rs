use std::{
    fs,
    future::Future,
    io::{self, ErrorKind},
    path::Path,
};

use crate::DocumentId;
use helix_stdx::path::canonicalize;

use super::Editor;

fn symlink_metadata_optional(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

impl Editor {
    pub(super) fn apply_file_operation_edit(
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
        let created = symlink_metadata_optional(&path)?.is_none();

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
            let mut options = fs::OpenOptions::new();
            options.write(true);
            if created {
                options.create_new(true);
            } else {
                options.create(true).truncate(true);
            }
            options.open(&path)?;
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
        let source_metadata = fs::symlink_metadata(old_path)?;
        if source_metadata.is_dir() {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "copying directories is not supported",
            ));
        }

        let created = symlink_metadata_optional(&new_path)?.is_none();
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
        let mut source = fs::File::open(old_path)?;
        let mut options = fs::OpenOptions::new();
        options.write(true);
        if created {
            options.create_new(true);
        } else {
            options.create(true).truncate(true);
        }
        let mut destination = options.open(&new_path)?;
        let bytes = io::copy(&mut source, &mut destination)?;

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
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!("path {} does not exist", path.display()),
                )
            } else {
                err
            }
        })?;
        let is_dir = metadata.is_dir();
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

        if is_dir {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
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
        let old_path = canonicalize(old_path);
        let new_path = canonicalize(new_path);
        if old_path == new_path {
            return Ok(());
        }
        let source_metadata = fs::symlink_metadata(&old_path)?;
        let is_dir = source_metadata.is_dir();
        let language_servers: Vec<_> = self
            .language_servers
            .iter_clients()
            .filter(|client| client.is_initialized())
            .cloned()
            .collect();
        for language_server in language_servers {
            let Some(request) = language_server.will_rename(&old_path, &new_path, is_dir) else {
                continue;
            };
            self.apply_file_operation_edit(&language_server, request);
        }

        fs::rename(&old_path, &new_path)?;

        if let Some(doc) = self.document_by_path(&old_path) {
            self.set_doc_path(doc.id(), &new_path);
        }
        for ls in self.language_servers.iter_clients() {
            if !ls.is_initialized() {
                continue;
            }
            ls.did_rename(&old_path, &new_path, is_dir);
        }
        self.language_servers
            .file_event_handler
            .file_changed(old_path);
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
