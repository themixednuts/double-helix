use std::collections::btree_map::Entry;
use std::collections::HashSet;
use std::num::NonZeroU64;
use std::time::Instant;

use crate::bench::log_command_phase;
use crate::{DocumentId, Editor};
use helix_core::diagnostic::DiagnosticProvider;
use helix_core::Uri;
use helix_lsp::{lsp, LanguageServerId};

pub use super::workspace_edit::{ApplyEditError, ApplyEditErrorKind};
use super::Handlers;

pub struct DocumentColorsEvent(pub DocumentId);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SignatureHelpInvoked {
    Automatic,
    Manual,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct SignatureHelpRequestId(NonZeroU64);

impl SignatureHelpRequestId {
    #[must_use]
    pub const fn new(id: NonZeroU64) -> Self {
        Self(id)
    }
}

pub enum SignatureHelpEvent {
    Invoked,
    Trigger,
    ReTrigger,
    Cancel,
    RequestComplete {
        request: SignatureHelpRequestId,
        open: bool,
    },
}

pub struct PullDiagnosticsEvent {
    pub document_id: DocumentId,
}

pub struct PullAllDocumentsDiagnosticsEvent {
    pub language_servers: HashSet<LanguageServerId>,
}

impl Editor {
    pub fn handle_lsp_diagnostics(
        &mut self,
        provider: &DiagnosticProvider,
        uri: Uri,
        version: Option<i32>,
        mut diagnostics: Vec<lsp::Diagnostic>,
    ) {
        let doc = self
            .documents
            .values_mut()
            .find(|doc| doc.uri().is_some_and(|u| u == uri));

        if let Some((version, doc)) = version.zip(doc.as_ref()) {
            if version != doc.version() {
                log::info!("Version ({version}) is out of date for {uri:?} (expected ({})), dropping PublishDiagnostic notification", doc.version());
                return;
            }
        }

        let mut unchanged_diag_sources = Vec::new();
        if let Some((lang_conf, old_diagnostics)) = doc
            .as_ref()
            .and_then(|doc| Some((doc.language_config()?, self.diagnostics.get(&uri)?)))
        {
            if !lang_conf.persistent_diagnostic_sources.is_empty() {
                // Sort diagnostics first by severity and then by line numbers.
                // Note: The `lsp::DiagnosticSeverity` enum is already defined in decreasing order
                diagnostics.sort_by_key(|d| (d.severity, d.range.start));
            }
            for source in &lang_conf.persistent_diagnostic_sources {
                let new_diagnostics = diagnostics
                    .iter()
                    .filter(|d| d.source.as_ref() == Some(source));
                let old_diagnostics = old_diagnostics
                    .iter()
                    .filter(|(d, d_provider)| {
                        d_provider == provider && d.source.as_ref() == Some(source)
                    })
                    .map(|(d, _)| d);
                if new_diagnostics.eq(old_diagnostics) {
                    unchanged_diag_sources.push(source.clone())
                }
            }
        }

        let diagnostics = diagnostics.into_iter().map(|d| (d, provider.clone()));

        // Insert the original lsp::Diagnostics here because we may have no open document
        // for diagnostic message and so we can't calculate the exact position.
        // When using them later in the diagnostics picker, we calculate them on-demand.
        let diagnostics = match self.diagnostics.entry(uri) {
            Entry::Occupied(o) => {
                let current_diagnostics = o.into_mut();
                // there may entries of other language servers, which is why we can't overwrite the whole entry
                current_diagnostics.retain(|(_, d_provider)| d_provider != provider);
                current_diagnostics.extend(diagnostics);
                current_diagnostics
                // Sort diagnostics first by severity and then by line numbers.
            }
            Entry::Vacant(v) => v.insert(diagnostics.collect()),
        };

        // Sort diagnostics first by severity and then by line numbers.
        // Note: The `lsp::DiagnosticSeverity` enum is already defined in decreasing order
        diagnostics.sort_by_key(|(d, provider)| (d.severity, d.range.start, provider.clone()));

        if let Some(doc) = doc {
            let diagnostic_of_language_server_and_not_in_unchanged_sources =
                |diagnostic: &lsp::Diagnostic, d_provider: &DiagnosticProvider| {
                    d_provider == provider
                        && diagnostic
                            .source
                            .as_ref()
                            .is_none_or(|source| !unchanged_diag_sources.contains(source))
                };
            let diagnostics = Self::doc_diagnostics_with_filter(
                &self.language_servers,
                &self.diagnostics,
                doc,
                diagnostic_of_language_server_and_not_in_unchanged_sources,
            );
            doc.replace_diagnostics(diagnostics, &unchanged_diag_sources, Some(provider));

            let doc = doc.id();
            self.dispatch_diagnostics_change(doc);
        }
        self.refresh_workspace_diagnostic_counts();
    }

    pub fn execute_lsp_command(&mut self, command: lsp::Command, server_id: LanguageServerId) {
        crate::handlers::lsp::request_execute_lsp_command(self, command, server_id);
    }
}

pub fn request_execute_lsp_command(
    editor: &mut Editor,
    command: lsp::Command,
    server_id: LanguageServerId,
) {
    let Some(future) = editor
        .language_server_by_id(server_id)
        .and_then(|server| server.command(command))
    else {
        editor.set_error("Language server does not support executing commands");
        return;
    };

    tokio::spawn(async move {
        if let Err(err) = future.await {
            log::error!("Error executing LSP command: {err}");
        }
    });
}

pub fn attach(editor: &Editor, _handlers: &Handlers) {
    editor
        .lifecycle()
        .on_language_server_initialized(move |event| {
            let language_server = event.editor.language_server_by_id(event.server_id).unwrap();

            for doc in event
                .editor
                .documents()
                .filter(|doc| doc.supports_language_server(event.server_id))
            {
                let Some(url) = doc.url() else {
                    continue;
                };

                let language_id = doc.language_id().map(ToOwned::to_owned).unwrap_or_default();

                language_server.text_document_did_open(url, doc.version(), doc.text(), language_id);
            }

            Ok(())
        });

    editor.lifecycle().on_document_change(move |event| {
        let hook_start = Instant::now();
        // Send textDocument/didChange notifications.
        if !event.ghost_transaction {
            for language_server in event.doc.language_servers() {
                language_server.text_document_did_change(
                    event.doc.versioned_identifier(),
                    event.old_text,
                    event.doc.text(),
                    event.changes,
                );
            }
        }
        let hook_dur = hook_start.elapsed();
        log_command_phase(
            "document_did_change_hook",
            "lsp_did_change",
            hook_dur,
            || {
                format!(
                    "doc_id={:?} ghost={} language_servers={} lines={} bytes={}",
                    event.doc.id(),
                    event.ghost_transaction,
                    event.doc.language_servers().count(),
                    event.doc.text().len_lines(),
                    event.doc.text().len_bytes()
                )
            },
        );
        Ok(())
    });

    editor.lifecycle().on_document_close(move |event| {
        // Send textDocument/didClose notifications.
        for language_server in event.doc.language_servers() {
            language_server.text_document_did_close(event.doc.identifier());
        }

        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Action;
    use crate::editor::Config;
    use arc_swap::ArcSwap;
    use helix_core::Rope;
    use helix_loader::runtime_dirs;
    use helix_lsp::OffsetEncoding;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_editor() -> Editor {
        let theme_loader = crate::theme::Loader::new(runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        let mut editor = Editor::new(
            crate::graphics::Rect::new(0, 0, 80, 24),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(config, |cfg: &Config| cfg)),
            helix_runtime::test::runtime(),
            Handlers::dummy(),
        );
        let doc = crate::Document::from(
            Rope::from(""),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        );
        editor.new_file_from_document(Action::VerticalSplit, doc);
        editor
    }

    fn make_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let counter = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "helix-lsp-workspace-edit-{}-{}-{}",
            std::process::id(),
            unique,
            counter
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn text_edit(range: std::ops::Range<usize>, replacement: &str) -> lsp::TextEdit {
        lsp::TextEdit {
            range: lsp::Range {
                start: lsp::Position {
                    line: 0,
                    character: range.start as u32,
                },
                end: lsp::Position {
                    line: 0,
                    character: range.end as u32,
                },
            },
            new_text: replacement.to_string(),
        }
    }

    #[test]
    fn workspace_edit_text_edits_are_planned_before_apply() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        let temp_dir = make_temp_dir();
        let first_path = temp_dir.join("first.txt");
        let second_path = temp_dir.join("second.txt");
        fs::write(&first_path, "alpha\n").expect("write first file");
        fs::write(&second_path, "bravo\n").expect("write second file");

        let mut editor = make_editor();
        let first_doc = editor
            .open(&first_path, Action::Load)
            .expect("open first file");
        let second_doc = editor
            .open(&second_path, Action::Load)
            .expect("open second file");

        let first_version = editor.document(first_doc).expect("first doc").version();
        let second_version = editor.document(second_doc).expect("second doc").version();

        let workspace_edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Edits(vec![
                lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&first_path).expect("first url"),
                        version: Some(first_version),
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..5, "omega"))],
                },
                lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&second_path).expect("second url"),
                        version: Some(second_version + 1),
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..5, "delta"))],
                },
            ])),
            change_annotations: None,
        };

        let result = editor.apply_workspace_edit(OffsetEncoding::Utf8, &workspace_edit);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().failed_change_idx, 1);

        assert_eq!(
            editor.document(first_doc).expect("first doc").text(),
            &Rope::from("alpha\n")
        );
        assert_eq!(
            editor.document(second_doc).expect("second doc").text(),
            &Rope::from("bravo\n")
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn mixed_workspace_edit_operations_are_planned_before_apply() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        let temp_dir = make_temp_dir();
        let rename_from = temp_dir.join("rename-from.txt");
        let rename_to = temp_dir.join("rename-to.txt");
        let second_path = temp_dir.join("second.txt");
        fs::write(&rename_from, "alpha\n").expect("write rename source");
        fs::write(&second_path, "bravo\n").expect("write second file");

        let mut editor = make_editor();
        let second_doc = editor
            .open(&second_path, Action::Load)
            .expect("open second file");
        let second_version = editor.document(second_doc).expect("second doc").version();

        let workspace_edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Rename(lsp::RenameFile {
                    old_uri: helix_lsp::Url::from_file_path(&rename_from).expect("rename from url"),
                    new_uri: helix_lsp::Url::from_file_path(&rename_to).expect("rename to url"),
                    options: None,
                    annotation_id: None,
                })),
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&second_path).expect("second url"),
                        version: Some(second_version + 1),
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..5, "delta"))],
                }),
            ])),
            change_annotations: None,
        };

        let result = editor.apply_workspace_edit(OffsetEncoding::Utf8, &workspace_edit);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().failed_change_idx, 1);

        assert!(rename_from.exists());
        assert!(!rename_to.exists());
        assert_eq!(
            editor.document(second_doc).expect("second doc").text(),
            &Rope::from("bravo\n")
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn directory_rename_workspace_edit_maps_descendant_text_edits() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        let temp_dir = make_temp_dir();
        let old_dir = temp_dir.join("old-dir");
        let new_dir = temp_dir.join("new-dir");
        let old_file = old_dir.join("file.txt");
        let new_file = new_dir.join("file.txt");
        fs::create_dir_all(&old_dir).expect("create old dir");
        fs::write(&old_file, "alpha\n").expect("write old file");

        let mut editor = make_editor();
        let workspace_edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Rename(lsp::RenameFile {
                    old_uri: helix_lsp::Url::from_file_path(&old_dir).expect("old dir url"),
                    new_uri: helix_lsp::Url::from_file_path(&new_dir).expect("new dir url"),
                    options: None,
                    annotation_id: None,
                })),
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&new_file).expect("new file url"),
                        version: None,
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..5, "omega"))],
                }),
            ])),
            change_annotations: None,
        };

        editor
            .apply_workspace_edit(OffsetEncoding::Utf8, &workspace_edit)
            .expect("apply workspace edit");

        assert!(!old_dir.exists());
        assert_eq!(
            fs::read_to_string(&new_file).expect("read new file"),
            "alpha\n"
        );
        assert_eq!(
            editor
                .document_by_path(&new_file)
                .expect("renamed file should be open")
                .text(),
            &Rope::from("omega\n")
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn deleted_directory_blocks_descendant_text_edits_during_planning() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        let temp_dir = make_temp_dir();
        let dir_path = temp_dir.join("dir");
        let file_path = dir_path.join("file.txt");
        fs::create_dir_all(&dir_path).expect("create dir");
        fs::write(&file_path, "alpha\n").expect("write file");

        let mut editor = make_editor();
        let workspace_edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Delete(lsp::DeleteFile {
                    uri: helix_lsp::Url::from_file_path(&dir_path).expect("dir url"),
                    options: Some(lsp::DeleteFileOptions {
                        recursive: Some(true),
                        ignore_if_not_exists: None,
                        annotation_id: None,
                    }),
                })),
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&file_path).expect("file url"),
                        version: None,
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..5, "omega"))],
                }),
            ])),
            change_annotations: None,
        };

        let result = editor.apply_workspace_edit(OffsetEncoding::Utf8, &workspace_edit);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().failed_change_idx, 1);
        assert!(dir_path.exists());
        assert_eq!(
            fs::read_to_string(&file_path).expect("read file"),
            "alpha\n"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn created_file_can_be_edited_later_in_same_workspace_edit() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        let temp_dir = make_temp_dir();
        let file_path = temp_dir.join("created.txt");

        let mut editor = make_editor();
        let workspace_edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(lsp::CreateFile {
                    uri: helix_lsp::Url::from_file_path(&file_path).expect("file url"),
                    options: None,
                    annotation_id: None,
                })),
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&file_path).expect("file url"),
                        version: None,
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..0, "hello"))],
                }),
            ])),
            change_annotations: None,
        };

        editor
            .apply_workspace_edit(OffsetEncoding::Utf8, &workspace_edit)
            .expect("apply workspace edit");

        assert!(file_path.exists());
        assert_eq!(fs::read_to_string(&file_path).expect("read file"), "");
        assert_eq!(
            editor
                .document_by_path(&file_path)
                .expect("created file should be open")
                .text(),
            &Rope::from("hello")
        );

        let _ = fs::remove_dir_all(temp_dir);
    }
}
