use std::cmp::Ordering;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Instant;

use crate::bench::log_command_phase;
use crate::{editor::WorkspaceDiagnosticCounts, DocumentId, Editor, ViewId};
use helix_core::diagnostic::DiagnosticProvider;
use helix_core::syntax::config::{LanguageConfiguration, LanguageServerFeature};
use helix_core::{Selection, Uri};
use helix_lsp::{lsp, LanguageServerId};

pub use super::workspace_edit::{ApplyEditError, ApplyEditErrorKind};
use super::Handlers;

pub struct DocumentColorsEvent(pub DocumentId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionRangeDirection {
    Expand,
    Shrink,
}

#[derive(Debug)]
pub struct SelectionRangeResponse {
    pub doc_id: DocumentId,
    pub view_id: ViewId,
    pub expected_version: i32,
    pub expected_selection: Selection,
    pub offset_encoding: helix_lsp::OffsetEncoding,
    pub direction: SelectionRangeDirection,
    pub ranges: Vec<lsp::SelectionRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LspFeatureRefreshKind {
    CodeLens,
    DocumentLinks,
    FoldingRanges,
    SemanticTokens,
    InlineCompletion,
}

const MAX_DIAGNOSTICS_PREPARE_RETRIES: u8 = 2;

#[derive(Debug)]
pub struct LspDiagnosticsWork {
    provider: DiagnosticProvider,
    uri: Uri,
    version: Option<i32>,
    diagnostics: Vec<lsp::Diagnostic>,
    workspace_baseline: Option<Arc<Vec<(lsp::Diagnostic, DiagnosticProvider)>>>,
    document: Option<LspDiagnosticsDocumentSnapshot>,
    attempt: u8,
}

#[derive(Debug)]
struct LspDiagnosticsDocumentSnapshot {
    id: DocumentId,
    version: i32,
    text: helix_core::Rope,
    diagnostics: Arc<Vec<helix_core::Diagnostic>>,
    language_config: Option<Arc<LanguageConfiguration>>,
    offset_encoding: Option<helix_lsp::OffsetEncoding>,
    persistent_sources: Vec<String>,
}

#[derive(Debug)]
pub struct PreparedLspDiagnostics {
    work: LspDiagnosticsWork,
    unchanged: bool,
    previous_counts: WorkspaceDiagnosticCounts,
    replacement_counts: WorkspaceDiagnosticCounts,
    workspace_diagnostics: Vec<(lsp::Diagnostic, DiagnosticProvider)>,
    document_diagnostics: Vec<helix_core::Diagnostic>,
}

#[derive(Debug)]
pub enum LspDiagnosticsApply {
    Done {
        retired_document_diagnostics: Option<Arc<Vec<helix_core::Diagnostic>>>,
        retired_workspace_diagnostics: Option<Arc<Vec<(lsp::Diagnostic, DiagnosticProvider)>>>,
    },
    Retry(LspDiagnosticsWork),
}

impl LspDiagnosticsWork {
    fn baseline_diagnostics(&self) -> impl Clone + Iterator<Item = &lsp::Diagnostic> {
        self.workspace_baseline
            .iter()
            .flat_map(|diagnostics| diagnostics.iter())
            .filter(|(_, provider)| provider == &self.provider)
            .map(|(diagnostic, _)| diagnostic)
    }

    pub fn execute(mut self) -> PreparedLspDiagnostics {
        self.diagnostics
            .sort_by_key(|diagnostic| (diagnostic.severity, diagnostic.range.start));

        let unchanged = self.baseline_diagnostics().eq(self.diagnostics.iter());
        let previous_counts =
            WorkspaceDiagnosticCounts::from_diagnostics(self.baseline_diagnostics());
        let replacement_counts =
            WorkspaceDiagnosticCounts::from_diagnostics(self.diagnostics.iter());
        if unchanged {
            return PreparedLspDiagnostics {
                work: self,
                unchanged,
                previous_counts,
                replacement_counts,
                workspace_diagnostics: Vec::new(),
                document_diagnostics: Vec::new(),
            };
        }
        let unchanged_sources: Vec<String> =
            self.document
                .as_ref()
                .map(|document| {
                    document
                        .persistent_sources
                        .iter()
                        .filter(|source| {
                            self.baseline_diagnostics()
                                .filter(|diagnostic| diagnostic.source.as_ref() == Some(source))
                                .eq(self.diagnostics.iter().filter(|diagnostic| {
                                    diagnostic.source.as_ref() == Some(source)
                                }))
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
        let existing = self
            .workspace_baseline
            .iter()
            .flat_map(|diagnostics| diagnostics.iter())
            .filter(|(_, provider)| provider != &self.provider)
            .cloned()
            .collect();
        let replacement = self
            .diagnostics
            .iter()
            .cloned()
            .map(|diagnostic| (diagnostic, self.provider.clone()))
            .collect();
        let workspace_diagnostics = merge_diagnostic_entries(existing, replacement);
        let mut document_diagnostics = self
            .document
            .as_ref()
            .map(|document| {
                document
                    .diagnostics
                    .iter()
                    .filter(|diagnostic| {
                        diagnostic.provider != self.provider
                            || diagnostic
                                .source
                                .as_ref()
                                .is_some_and(|source| unchanged_sources.contains(source))
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some((document, offset_encoding)) = self
            .document
            .as_ref()
            .and_then(|document| Some((document, document.offset_encoding?)))
        {
            document_diagnostics.extend(self.diagnostics.iter().filter_map(|diagnostic| {
                if diagnostic
                    .source
                    .as_ref()
                    .is_some_and(|source| unchanged_sources.contains(source))
                {
                    return None;
                }
                crate::Document::lsp_diagnostic_to_diagnostic(
                    &document.text,
                    document.language_config.as_deref(),
                    diagnostic,
                    self.provider.clone(),
                    offset_encoding,
                )
            }));
        }
        document_diagnostics.sort_by(|left, right| {
            left.range
                .cmp(&right.range)
                .then_with(|| left.severity.cmp(&right.severity))
                .then_with(|| left.provider.cmp(&right.provider))
        });

        PreparedLspDiagnostics {
            work: self,
            unchanged,
            previous_counts,
            replacement_counts,
            workspace_diagnostics,
            document_diagnostics,
        }
    }
}

fn compare_diagnostic_entries(
    left: &(lsp::Diagnostic, DiagnosticProvider),
    right: &(lsp::Diagnostic, DiagnosticProvider),
) -> Ordering {
    left.0
        .severity
        .cmp(&right.0.severity)
        .then_with(|| left.0.range.start.cmp(&right.0.range.start))
        .then_with(|| left.1.cmp(&right.1))
}

fn merge_diagnostic_entries(
    existing: Vec<(lsp::Diagnostic, DiagnosticProvider)>,
    replacement: Vec<(lsp::Diagnostic, DiagnosticProvider)>,
) -> Vec<(lsp::Diagnostic, DiagnosticProvider)> {
    let capacity = existing.len() + replacement.len();
    let mut existing = existing.into_iter().peekable();
    let mut replacement = replacement.into_iter().peekable();
    let mut merged = Vec::with_capacity(capacity);

    while let (Some(left), Some(right)) = (existing.peek(), replacement.peek()) {
        if compare_diagnostic_entries(left, right).is_le() {
            merged.push(existing.next().expect("peeked diagnostic disappeared"));
        } else {
            merged.push(replacement.next().expect("peeked diagnostic disappeared"));
        }
    }
    merged.extend(existing);
    merged.extend(replacement);
    merged
}

pub struct LspFeatureRefreshEvent {
    pub doc_id: DocumentId,
    pub kind: LspFeatureRefreshKind,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SignatureHelpInvoked {
    Automatic,
    Manual,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Hash)]
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

impl Editor {
    pub fn handle_lsp_diagnostics(
        &mut self,
        provider: &DiagnosticProvider,
        uri: Uri,
        version: Option<i32>,
        diagnostics: Vec<lsp::Diagnostic>,
    ) {
        let Some(mut work) =
            self.prepare_lsp_diagnostics(provider.clone(), uri, version, diagnostics)
        else {
            return;
        };
        loop {
            match self.apply_prepared_lsp_diagnostics(work.execute()) {
                LspDiagnosticsApply::Done {
                    retired_document_diagnostics,
                    retired_workspace_diagnostics,
                } => {
                    if retired_document_diagnostics.is_some()
                        || retired_workspace_diagnostics.is_some()
                    {
                        self.runtime()
                            .block()
                            .spawn(move || {
                                drop(retired_document_diagnostics);
                                drop(retired_workspace_diagnostics);
                            })
                            .detach();
                    }
                    return;
                }
                LspDiagnosticsApply::Retry(retry) => work = retry,
            }
        }
    }

    pub fn prepare_lsp_diagnostics(
        &self,
        provider: DiagnosticProvider,
        uri: Uri,
        version: Option<i32>,
        diagnostics: Vec<lsp::Diagnostic>,
    ) -> Option<LspDiagnosticsWork> {
        self.prepare_lsp_diagnostics_attempt(provider, uri, version, diagnostics, 0)
    }

    fn prepare_lsp_diagnostics_attempt(
        &self,
        provider: DiagnosticProvider,
        uri: Uri,
        version: Option<i32>,
        diagnostics: Vec<lsp::Diagnostic>,
        attempt: u8,
    ) -> Option<LspDiagnosticsWork> {
        let doc_id = self
            .documents()
            .find(|doc| doc.uri().is_some_and(|u| u == uri))
            .map(|doc| doc.id());

        if let Some((version, doc)) = version.zip(doc_id.and_then(|id| self.document(id))) {
            if version != doc.version() {
                log::info!(
                    "Version ({version}) is out of date for {uri:?} (expected ({})), dropping PublishDiagnostic notification",
                    doc.version()
                );
                return None;
            }
        }

        let workspace_baseline = self.diagnostics.get(&uri).cloned();
        let document = doc_id.and_then(|id| {
            let document = self.document(id)?;
            let language_config = document.language_configuration().cloned();
            let offset_encoding = provider.language_server_id().and_then(|server_id| {
                let language_server = self.language_servers.get_by_id(server_id)?;
                language_config
                    .as_ref()?
                    .language_servers
                    .iter()
                    .any(|features| {
                        features.name == language_server.name()
                            && features.has_feature(LanguageServerFeature::Diagnostics)
                    })
                    .then(|| language_server.offset_encoding())
            });
            let persistent_sources = language_config
                .as_ref()
                .map(|config| config.persistent_diagnostic_sources.clone())
                .unwrap_or_default();
            Some(LspDiagnosticsDocumentSnapshot {
                id,
                version: document.version(),
                text: document.text().clone(),
                diagnostics: document.diagnostics_arc(),
                language_config,
                offset_encoding,
                persistent_sources,
            })
        });

        Some(LspDiagnosticsWork {
            provider,
            uri,
            version,
            diagnostics,
            workspace_baseline,
            document,
            attempt,
        })
    }

    pub fn apply_prepared_lsp_diagnostics(
        &mut self,
        prepared: PreparedLspDiagnostics,
    ) -> LspDiagnosticsApply {
        if prepared.unchanged {
            return LspDiagnosticsApply::Done {
                retired_document_diagnostics: None,
                retired_workspace_diagnostics: None,
            };
        }

        let baseline_is_current = match (
            self.diagnostics.get(&prepared.work.uri),
            prepared.work.workspace_baseline.as_ref(),
        ) {
            (None, None) => true,
            (Some(current), Some(baseline)) => Arc::ptr_eq(current, baseline),
            _ => false,
        };
        let document_is_current = prepared.work.document.as_ref().is_none_or(|snapshot| {
            self.document(snapshot.id)
                .is_some_and(|document| document.version() == snapshot.version)
        });
        if !baseline_is_current || !document_is_current {
            return self.retry_prepared_lsp_diagnostics(prepared.work);
        }

        let PreparedLspDiagnostics {
            work,
            previous_counts,
            replacement_counts,
            workspace_diagnostics,
            document_diagnostics,
            ..
        } = prepared;
        let LspDiagnosticsWork { uri, document, .. } = work;
        let retired_workspace_diagnostics = if workspace_diagnostics.is_empty() {
            self.diagnostics.remove(&uri)
        } else {
            self.diagnostics
                .insert(uri.clone(), Arc::new(workspace_diagnostics))
        };

        self.workspace_diagnostic_counts
            .replace(previous_counts, replacement_counts);
        self.replace_workspace_diagnostic_summary(&uri, previous_counts, replacement_counts);

        let retired_document_diagnostics = if let Some(snapshot) = document {
            let document = self
                .document_mut(snapshot.id)
                .expect("validated diagnostic document disappeared during atomic apply");
            let retired = document.swap_diagnostics(document_diagnostics);
            self.dispatch_diagnostics_change(snapshot.id);
            Some(retired)
        } else {
            None
        };
        self.bump_diagnostics_revision();
        LspDiagnosticsApply::Done {
            retired_document_diagnostics,
            retired_workspace_diagnostics,
        }
    }

    fn retry_prepared_lsp_diagnostics(&self, work: LspDiagnosticsWork) -> LspDiagnosticsApply {
        let LspDiagnosticsWork {
            provider,
            uri,
            version,
            diagnostics,
            attempt,
            ..
        } = work;
        self.retry_prepared_lsp_diagnostics_attempt(provider, uri, version, diagnostics, attempt)
    }

    fn retry_prepared_lsp_diagnostics_attempt(
        &self,
        provider: DiagnosticProvider,
        uri: Uri,
        version: Option<i32>,
        diagnostics: Vec<lsp::Diagnostic>,
        attempt: u8,
    ) -> LspDiagnosticsApply {
        if attempt >= MAX_DIAGNOSTICS_PREPARE_RETRIES {
            log::warn!("dropping diagnostics update for {uri:?} after repeated stale snapshots");
            return LspDiagnosticsApply::Done {
                retired_document_diagnostics: None,
                retired_workspace_diagnostics: None,
            };
        }
        self.prepare_lsp_diagnostics_attempt(provider, uri, version, diagnostics, attempt + 1)
            .map_or(
                LspDiagnosticsApply::Done {
                    retired_document_diagnostics: None,
                    retired_workspace_diagnostics: None,
                },
                LspDiagnosticsApply::Retry,
            )
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
                language_server
                    .text_document_did_change(event.doc.versioned_identifier(), event.doc.text());
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
    use crate::editor::{
        Action, Config, FileOperationCompletion, FileOperationDispatch, FileOperationRequest,
        WorkspaceEditContinuation, WorkspaceEditExecutionDispatch, WorkspaceEditExecutionUpdate,
    };
    use arc_swap::ArcSwap;
    use helix_core::{Rope, Transaction};
    use helix_lsp::OffsetEncoding;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_editor() -> Editor {
        let theme_loader = crate::theme::Loader::new(&[]);
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

    fn execute_workspace_resource(
        editor: &mut Editor,
        request: FileOperationRequest,
    ) -> FileOperationCompletion {
        let id = editor.enqueue_file_operation(request);
        let FileOperationDispatch::Inspect(inspection) = editor
            .next_file_operation_dispatch()
            .expect("workspace resource should inspect")
        else {
            panic!("workspace resource should inspect");
        };
        let prepared = inspection
            .execute()
            .expect("workspace resource should prepare");
        editor
            .accept_file_operation_preparation(prepared)
            .expect("workspace resource preparation should be current");
        let work = editor
            .begin_file_operation_mutation(id)
            .expect("workspace resource should mutate");
        editor
            .finish_file_operation(work.execute())
            .expect("workspace resource completion should be current")
            .pop()
            .expect("one workspace resource completion")
    }

    fn take_workspace_resource(update: WorkspaceEditExecutionUpdate) -> FileOperationRequest {
        assert!(update.parent_completion.is_none());
        match update.dispatch {
            WorkspaceEditExecutionDispatch::EnqueueResource(request) => request,
            dispatch => panic!("expected workspace resource, got {dispatch:?}"),
        }
    }

    fn advance_after_workspace_resource(
        editor: &mut Editor,
        completion: &FileOperationCompletion,
    ) -> WorkspaceEditExecutionUpdate {
        let update = editor
            .resume_workspace_edit_execution(completion)
            .expect("workspace resource completion should resume the cursor");
        assert!(update.parent_completion.is_none());
        let WorkspaceEditExecutionDispatch::Advance(batch_id) = update.dispatch else {
            panic!("workspace resource success should defer cursor advancement");
        };
        editor.advance_workspace_edit_execution_batch(batch_id)
    }

    fn diagnostic(severity: lsp::DiagnosticSeverity, message: &str) -> lsp::Diagnostic {
        lsp::Diagnostic {
            range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 1)),
            severity: Some(severity),
            code: None,
            code_description: None,
            source: Some("test".to_string()),
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn diagnostic_provider(identifier: &'static str) -> DiagnosticProvider {
        DiagnosticProvider::Lsp {
            server_id: LanguageServerId::default(),
            identifier: Some(Arc::<str>::from(identifier)),
        }
    }

    #[test]
    fn identical_diagnostic_publications_do_not_invalidate_workspace_state() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let mut editor = make_editor();
        let uri = Uri::from(PathBuf::from("C:/workspace/main.rs"));
        let provider = diagnostic_provider("rustc");
        let diagnostics = vec![
            diagnostic(lsp::DiagnosticSeverity::WARNING, "warning"),
            diagnostic(lsp::DiagnosticSeverity::ERROR, "error"),
        ];

        editor.handle_lsp_diagnostics(&provider, uri.clone(), None, diagnostics.clone());
        let revision = editor.diagnostics_revision();
        assert_eq!(editor.workspace_diagnostic_counts().warnings, 1);
        assert_eq!(editor.workspace_diagnostic_counts().errors, 1);
        let summaries = editor.workspace_diagnostic_summaries().collect::<Vec<_>>();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].1.total(), 2);
        assert_eq!(
            editor
                .workspace_diagnostic_path_summary(Path::new("C:/workspace/main.rs"))
                .expect("file summary")
                .total(),
            2
        );
        assert_eq!(
            editor
                .workspace_diagnostic_path_summary(Path::new("C:/workspace"))
                .expect("directory summary")
                .total(),
            2
        );

        editor.handle_lsp_diagnostics(&provider, uri, None, diagnostics);

        assert_eq!(editor.diagnostics_revision(), revision);
        assert_eq!(editor.workspace_diagnostic_counts().warnings, 1);
        assert_eq!(editor.workspace_diagnostic_counts().errors, 1);
    }

    #[test]
    fn replacing_one_diagnostic_provider_updates_only_its_count_delta() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let mut editor = make_editor();
        let uri = Uri::from(PathBuf::from("C:/workspace/main.rs"));
        let rustc = diagnostic_provider("rustc");
        let clippy = diagnostic_provider("clippy");

        editor.handle_lsp_diagnostics(
            &rustc,
            uri.clone(),
            None,
            vec![diagnostic(lsp::DiagnosticSeverity::ERROR, "rustc")],
        );
        editor.handle_lsp_diagnostics(
            &clippy,
            uri.clone(),
            None,
            vec![diagnostic(lsp::DiagnosticSeverity::INFORMATION, "clippy")],
        );
        editor.handle_lsp_diagnostics(
            &rustc,
            uri,
            None,
            vec![diagnostic(lsp::DiagnosticSeverity::HINT, "rustc hint")],
        );

        let counts = editor.workspace_diagnostic_counts();
        assert_eq!(counts.errors, 0);
        assert_eq!(counts.hints, 1);
        assert_eq!(counts.info, 1);
        let summary = editor
            .workspace_diagnostic_summaries()
            .next()
            .expect("diagnostic summary")
            .1;
        assert_eq!(summary, counts);
    }

    #[test]
    fn removing_language_server_diagnostics_clears_cached_summaries() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let mut editor = make_editor();
        let uri = Uri::from(PathBuf::from("C:/workspace/main.rs"));
        let provider = diagnostic_provider("rustc");

        editor.handle_lsp_diagnostics(
            &provider,
            uri,
            None,
            vec![diagnostic(lsp::DiagnosticSeverity::ERROR, "error")],
        );
        let revision = editor.diagnostics_revision();

        editor.remove_language_server_diagnostics(LanguageServerId::default());

        assert_eq!(editor.workspace_diagnostic_counts().total(), 0);
        assert_eq!(editor.workspace_diagnostic_summaries().count(), 0);
        assert!(editor
            .workspace_diagnostic_path_summary(Path::new("C:/workspace"))
            .is_none());
        assert_eq!(editor.diagnostics_revision(), revision + 1);

        editor.remove_language_server_diagnostics(LanguageServerId::default());
        assert_eq!(editor.diagnostics_revision(), revision + 1);
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
    fn closed_workspace_edit_preparation_runs_off_the_editor_thread() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        let temp_dir = make_temp_dir();
        let path = temp_dir.join("closed.txt");
        fs::write(&path, "alpha\n").expect("write closed file");
        let mut editor = make_editor();
        let workspace_edit = lsp::WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([(
                helix_lsp::Url::from_file_path(&path).expect("file url"),
                vec![text_edit(0..5, "omega")],
            )])),
            document_changes: None,
            change_annotations: None,
        };

        let preparation = editor.prepare_workspace_edit(OffsetEncoding::Utf8, workspace_edit);
        let plan = std::thread::spawn(move || preparation.execute())
            .join()
            .expect("workspace edit worker should not panic")
            .expect("workspace edit should prepare");
        editor
            .apply_prepared_workspace_edit(plan)
            .expect("apply worker-prepared workspace edit");

        assert_eq!(
            editor
                .document_by_path(&path)
                .expect("closed file should open from prepared state")
                .text(),
            &Rope::from("omega\n")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\n");
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

        let result = editor
            .apply_workspace_edit(OffsetEncoding::Utf8, &workspace_edit)
            .expect("apply workspace edit");

        assert!(matches!(
            result.file_operations.as_slice(),
            [crate::handlers::workspace_edit::WorkspaceEditFileOperation {
                request: crate::editor::FileOperationRequest {
                    origin: crate::editor::FileOperationOrigin::WorkspaceEdit { .. },
                    operation: crate::editor::FileOperation::Move { source, destination, .. },
                },
                ..
            }] if source == &old_dir
                && matches!(destination, crate::editor::FileOperationDestination::Exact(path) if path == &new_dir)
        ));
        assert!(old_dir.exists());
        assert!(!new_dir.exists());
        assert_eq!(
            fs::read_to_string(&old_file).expect("read old file"),
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

        let result = editor
            .apply_workspace_edit(OffsetEncoding::Utf8, &workspace_edit)
            .expect("apply workspace edit");

        assert!(matches!(
            result.file_operations.as_slice(),
            [crate::handlers::workspace_edit::WorkspaceEditFileOperation {
                request: crate::editor::FileOperationRequest {
                    origin: crate::editor::FileOperationOrigin::WorkspaceEdit { .. },
                    operation: crate::editor::FileOperation::Create { path, is_dir: false, .. },
                },
                ..
            }] if path == &file_path
        ));
        assert!(!file_path.exists());
        assert_eq!(
            editor
                .document_by_path(&file_path)
                .expect("created file should be open")
                .text(),
            &Rope::from("hello")
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn workspace_execution_orders_create_before_following_text_edit() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let temp_dir = make_temp_dir();
        let path = temp_dir.join("created.txt");
        let mut editor = make_editor();
        let edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(lsp::CreateFile {
                    uri: helix_lsp::Url::from_file_path(&path).expect("file url"),
                    options: None,
                    annotation_id: None,
                })),
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&path).expect("file url"),
                        version: None,
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..0, "hello"))],
                }),
            ])),
            change_annotations: None,
        };

        let plan = editor
            .prepare_workspace_edit(OffsetEncoding::Utf8, edit)
            .execute()
            .expect("workspace edit should prepare");
        let request = take_workspace_resource(editor.start_workspace_edit_execution(
            plan,
            Some(WorkspaceEditContinuation::ExecuteCommand {
                server_id: LanguageServerId::default(),
                command: lsp::Command {
                    title: "after edit".to_owned(),
                    command: "after.edit".to_owned(),
                    arguments: None,
                },
            }),
            None,
        ));
        assert!(editor.document_by_path(&path).is_none());

        let completion = execute_workspace_resource(&mut editor, request);
        assert!(completion.result.is_ok());
        assert!(path.exists());
        assert!(editor.document_by_path(&path).is_none());
        let update = advance_after_workspace_resource(&mut editor, &completion);
        assert!(matches!(
            update.dispatch,
            WorkspaceEditExecutionDispatch::Complete(
                crate::editor::WorkspaceEditBatchCompletion {
                    continuation: Some(WorkspaceEditContinuation::ExecuteCommand { command, .. }),
                    result: Ok(()),
                }
            ) if command.command == "after.edit"
        ));
        assert_eq!(
            editor
                .document_by_path(&path)
                .expect("text edit should run after create")
                .text(),
            &Rope::from("hello")
        );
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn workspace_execution_orders_text_rename_text_and_reopens_at_new_path() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let temp_dir = make_temp_dir();
        let old_path = temp_dir.join("old.txt");
        let new_path = temp_dir.join("new.txt");
        fs::write(&old_path, "alpha\n").expect("write file");
        let mut editor = make_editor();
        let doc_id = editor.open(&old_path, Action::Load).expect("open file");
        let version = editor.document(doc_id).expect("open document").version();
        let edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&old_path).expect("old url"),
                        version: Some(version),
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..5, "bravo"))],
                }),
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Rename(lsp::RenameFile {
                    old_uri: helix_lsp::Url::from_file_path(&old_path).expect("old url"),
                    new_uri: helix_lsp::Url::from_file_path(&new_path).expect("new url"),
                    options: None,
                    annotation_id: None,
                })),
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&new_path).expect("new url"),
                        version: None,
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..5, "omega"))],
                }),
            ])),
            change_annotations: None,
        };

        let plan = editor
            .prepare_workspace_edit(OffsetEncoding::Utf8, edit)
            .execute()
            .expect("workspace edit should prepare");
        let request =
            take_workspace_resource(editor.start_workspace_edit_execution(plan, None, None));
        assert_eq!(
            editor.document(doc_id).expect("document").text(),
            &Rope::from("bravo\n")
        );
        assert!(old_path.exists());

        let completion = execute_workspace_resource(&mut editor, request);
        assert!(completion.result.is_ok());
        editor.set_doc_path(doc_id, &new_path);
        let update = advance_after_workspace_resource(&mut editor, &completion);
        assert!(matches!(
            update.dispatch,
            WorkspaceEditExecutionDispatch::Complete(_)
        ));
        assert!(!old_path.exists());
        assert_eq!(
            editor.document(doc_id).expect("document").path(),
            Some(&new_path)
        );
        assert_eq!(
            editor.document(doc_id).expect("document").text(),
            &Rope::from("omega\n")
        );
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn workspace_execution_resource_failure_suppresses_later_text_and_command() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let temp_dir = make_temp_dir();
        let path = temp_dir.join("existing.txt");
        let mut editor = make_editor();
        let edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(lsp::CreateFile {
                    uri: helix_lsp::Url::from_file_path(&path).expect("file url"),
                    options: None,
                    annotation_id: None,
                })),
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&path).expect("file url"),
                        version: None,
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..0, "later"))],
                }),
            ])),
            change_annotations: None,
        };
        let continuation = WorkspaceEditContinuation::ExecuteCommand {
            server_id: LanguageServerId::default(),
            command: lsp::Command {
                title: "after edit".to_owned(),
                command: "after.edit".to_owned(),
                arguments: None,
            },
        };
        let plan = editor
            .prepare_workspace_edit(OffsetEncoding::Utf8, edit)
            .execute()
            .expect("workspace edit should prepare");
        let request = take_workspace_resource(editor.start_workspace_edit_execution(
            plan,
            Some(continuation),
            None,
        ));
        fs::write(&path, "already here").expect("make resource fail");
        let completion = execute_workspace_resource(&mut editor, request);
        assert!(completion.result.is_err());
        let update = editor
            .resume_workspace_edit_execution(&completion)
            .expect("failed resource should finish workspace execution");
        assert!(matches!(
            update.dispatch,
            WorkspaceEditExecutionDispatch::Complete(crate::editor::WorkspaceEditBatchCompletion {
                continuation: Some(WorkspaceEditContinuation::ExecuteCommand { .. }),
                result: Err(_),
            })
        ));
        assert!(editor.document_by_path(&path).is_none());
        assert!(editor
            .resume_workspace_edit_execution(&completion)
            .is_none());
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn workspace_execution_rejects_user_edit_while_waiting_for_resource() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let temp_dir = make_temp_dir();
        let created_path = temp_dir.join("created.txt");
        let edited_path = temp_dir.join("edited.txt");
        fs::write(&edited_path, "alpha\n").expect("write file");
        let mut editor = make_editor();
        let doc_id = editor.open(&edited_path, Action::Load).expect("open file");
        let version = editor.document(doc_id).expect("document").version();
        let edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(lsp::CreateFile {
                    uri: helix_lsp::Url::from_file_path(&created_path).expect("created url"),
                    options: None,
                    annotation_id: None,
                })),
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&edited_path).expect("edited url"),
                        version: Some(version),
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..5, "omega"))],
                }),
            ])),
            change_annotations: None,
        };
        let plan = editor
            .prepare_workspace_edit(OffsetEncoding::Utf8, edit)
            .execute()
            .expect("workspace edit should prepare");
        let request =
            take_workspace_resource(editor.start_workspace_edit_execution(plan, None, None));
        let view_id = editor.get_synced_view_id(doc_id);
        let transaction = Transaction::change(
            editor.document(doc_id).expect("document").text(),
            [(0, 5, Some("user".into()))].into_iter(),
        );
        assert!(editor
            .document_mut(doc_id)
            .expect("document")
            .apply(&transaction, view_id));

        let completion = execute_workspace_resource(&mut editor, request);
        let update = advance_after_workspace_resource(&mut editor, &completion);
        assert!(matches!(
            update.dispatch,
            WorkspaceEditExecutionDispatch::Complete(crate::editor::WorkspaceEditBatchCompletion {
                result: Err(crate::editor::WorkspaceEditBatchError {
                    failed_change_idx: Some(1),
                    ..
                }),
                ..
            })
        ));
        assert_eq!(
            editor.document(doc_id).expect("document").text(),
            &Rope::from("user\n")
        );
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn workspace_execution_ignores_duplicate_resource_completion_before_final_step() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let temp_dir = make_temp_dir();
        let first_path = temp_dir.join("first.txt");
        let second_path = temp_dir.join("second.txt");
        let mut editor = make_editor();
        let edit = lsp::WorkspaceEdit {
            changes: None,
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(lsp::CreateFile {
                    uri: helix_lsp::Url::from_file_path(&first_path).expect("first url"),
                    options: None,
                    annotation_id: None,
                })),
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(lsp::CreateFile {
                    uri: helix_lsp::Url::from_file_path(&second_path).expect("second url"),
                    options: None,
                    annotation_id: None,
                })),
                lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                        uri: helix_lsp::Url::from_file_path(&second_path).expect("second url"),
                        version: None,
                    },
                    edits: vec![lsp::OneOf::Left(text_edit(0..0, "done"))],
                }),
            ])),
            change_annotations: None,
        };
        let plan = editor
            .prepare_workspace_edit(OffsetEncoding::Utf8, edit)
            .execute()
            .expect("workspace edit should prepare");
        let first_request =
            take_workspace_resource(editor.start_workspace_edit_execution(plan, None, None));
        let first_completion = execute_workspace_resource(&mut editor, first_request);
        let second_request = take_workspace_resource(advance_after_workspace_resource(
            &mut editor,
            &first_completion,
        ));
        assert!(
            editor
                .resume_workspace_edit_execution(&first_completion)
                .is_none(),
            "a duplicate completion must not advance the final text step"
        );
        assert!(editor.document_by_path(&second_path).is_none());
        let second_completion = execute_workspace_resource(&mut editor, second_request);
        let update = advance_after_workspace_resource(&mut editor, &second_completion);
        assert!(matches!(
            update.dispatch,
            WorkspaceEditExecutionDispatch::Complete(_)
        ));
        assert_eq!(
            editor
                .document_by_path(&second_path)
                .expect("final text edit should run once")
                .text(),
            &Rope::from("done")
        );
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn workspace_execution_rejects_closed_snapshot_opened_and_changed_before_apply() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let temp_dir = make_temp_dir();
        let path = temp_dir.join("closed.txt");
        fs::write(&path, "alpha\n").expect("write file");
        let mut editor = make_editor();
        let edit = lsp::WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([(
                helix_lsp::Url::from_file_path(&path).expect("file url"),
                vec![text_edit(0..5, "omega")],
            )])),
            document_changes: None,
            change_annotations: None,
        };
        let plan = editor
            .prepare_workspace_edit(OffsetEncoding::Utf8, edit)
            .execute()
            .expect("workspace edit should prepare");
        let doc_id = editor
            .open(&path, Action::Load)
            .expect("open after snapshot");
        let view_id = editor.get_synced_view_id(doc_id);
        let transaction = Transaction::change(
            editor.document(doc_id).expect("document").text(),
            [(0, 5, Some("user".into()))].into_iter(),
        );
        assert!(editor
            .document_mut(doc_id)
            .expect("document")
            .apply(&transaction, view_id));

        let update = editor.start_workspace_edit_execution(plan, None, None);
        assert!(matches!(
            update.dispatch,
            WorkspaceEditExecutionDispatch::Complete(crate::editor::WorkspaceEditBatchCompletion {
                result: Err(crate::editor::WorkspaceEditBatchError {
                    failed_change_idx: Some(0),
                    ..
                }),
                ..
            })
        ));
        assert_eq!(
            editor.document(doc_id).expect("document").text(),
            &Rope::from("user\n")
        );
        let _ = fs::remove_dir_all(temp_dir);
    }
}
