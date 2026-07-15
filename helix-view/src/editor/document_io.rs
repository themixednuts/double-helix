use std::{
    fs::{File, Metadata},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use arc_swap::{access::DynAccess, ArcSwap};
use helix_core::{
    encoding::Encoding,
    indent::{auto_detect_indent_style, IndentStyle},
    line_ending::auto_detect_line_ending,
    LineEnding, Rope, Transaction,
};
use helix_vcs::DiffProviderRegistry;

use crate::{
    document::{
        from_reader, DocumentOpenError, DocumentReloadFormatConfig, LanguageInitialization,
    },
    handlers::BlameEvent,
    traits::{HistoryViewport, Identified},
    Document, DocumentId,
};

use super::Editor;

const MAX_STABLE_READ_ATTEMPTS: usize = 3;
const PREPARED_DOCUMENT_OPEN_CACHE_CAPACITY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentOpenRole {
    Interactive,
    Preview,
}

impl DocumentOpenRole {
    pub const fn is_preview(self) -> bool {
        matches!(self, Self::Preview)
    }
}

/// Immutable editor services captured for one document load.
pub struct DocumentOpenWork {
    path: PathBuf,
    role: DocumentOpenRole,
    config: Arc<dyn DynAccess<super::Config> + Send + Sync>,
    syn_loader: Arc<ArcSwap<helix_core::syntax::Loader>>,
    diff_providers: DiffProviderRegistry,
}

impl std::fmt::Debug for DocumentOpenWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DocumentOpenWork")
            .field("path", &self.path)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

/// A document and all filesystem/VCS-derived state needed to insert it into an editor.
pub struct PreparedDocumentOpen {
    path: PathBuf,
    role: DocumentOpenRole,
    document: Document,
    diff_base: Option<Rope>,
    version_control_head: Option<Arc<ArcSwap<Box<str>>>>,
}

#[derive(Default)]
pub(crate) struct PreparedDocumentOpenCache {
    entries: std::collections::VecDeque<PreparedDocumentOpen>,
}

impl PreparedDocumentOpenCache {
    fn insert(&mut self, prepared: PreparedDocumentOpen) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|cached| cached.path == prepared.path)
        {
            self.entries.remove(index);
        }
        self.entries.push_front(prepared);
        self.entries.truncate(PREPARED_DOCUMENT_OPEN_CACHE_CAPACITY);
    }

    fn take(&mut self, path: &Path) -> Option<PreparedDocumentOpen> {
        let index = self
            .entries
            .iter()
            .position(|prepared| prepared.path == path)?;
        self.entries.remove(index)
    }
}

impl std::fmt::Debug for PreparedDocumentOpen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedDocumentOpen")
            .field("path", &self.path)
            .field("role", &self.role)
            .field("bytes", &self.document.text().len_bytes())
            .field("language", &self.document.language_name())
            .field("has_syntax", &self.document.has_syntax())
            .field("has_diff_base", &self.diff_base.is_some())
            .finish_non_exhaustive()
    }
}

impl DocumentOpenWork {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn role(&self) -> DocumentOpenRole {
        self.role
    }

    pub fn execute(self) -> Result<PreparedDocumentOpen, DocumentOpenError> {
        let mut document = Document::open(
            &self.path,
            None,
            LanguageInitialization::Full,
            self.config,
            self.syn_loader,
        )?;
        if self.role.is_preview() {
            document.mark_preview();
        }

        let diff_base = self
            .diff_providers
            .get_diff_base(&self.path)
            .and_then(|bytes| from_reader(&mut bytes.as_slice(), Some(document.encoding())).ok())
            .map(|(text, _, _)| text);
        let version_control_head = self.diff_providers.get_current_head_name(&self.path);

        Ok(PreparedDocumentOpen {
            path: self.path,
            role: self.role,
            document,
            diff_base,
            version_control_head,
        })
    }
}

impl PreparedDocumentOpen {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn role(&self) -> DocumentOpenRole {
        self.role
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.document
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DiskFingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

impl From<&Metadata> for DiskFingerprint {
    fn from(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

#[derive(Debug)]
struct DiskSnapshot {
    text: Rope,
    modified: SystemTime,
    readonly: bool,
}

/// Immutable document state captured on the UI thread and executed on a blocking worker.
pub struct DocumentReloadWork {
    document: DocumentId,
    path: PathBuf,
    version: i32,
    encoding: &'static Encoding,
    text: Rope,
    format: DocumentReloadFormatConfig,
    diff_providers: DiffProviderRegistry,
}

impl std::fmt::Debug for DocumentReloadWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DocumentReloadWork")
            .field("document", &self.document)
            .field("path", &self.path)
            .field("version", &self.version)
            .field("encoding", &self.encoding.name())
            .finish_non_exhaustive()
    }
}

/// Fully prepared reload state. No filesystem or VCS access is required to apply it.
pub struct PreparedDocumentReload {
    document: DocumentId,
    path: PathBuf,
    version: i32,
    encoding: &'static Encoding,
    transaction: Transaction,
    modified: SystemTime,
    readonly: bool,
    indent: IndentStyle,
    line_ending: LineEnding,
    diff_base: Option<Rope>,
    version_control_head: Option<Arc<ArcSwap<Box<str>>>>,
}

impl std::fmt::Debug for PreparedDocumentReload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedDocumentReload")
            .field("document", &self.document)
            .field("path", &self.path)
            .field("version", &self.version)
            .field("encoding", &self.encoding.name())
            .field("readonly", &self.readonly)
            .field("indent", &self.indent)
            .field("line_ending", &self.line_ending)
            .field("has_diff_base", &self.diff_base.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DocumentReloadError {
    #[error("failed to reload '{}': {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("'{}' kept changing while it was being reloaded", path.display())]
    Unstable { path: PathBuf },
    #[error("reload worker failed for '{}': {message}", path.display())]
    Worker { path: PathBuf, message: String },
}

impl DocumentReloadError {
    pub fn worker(path: PathBuf, message: impl Into<String>) -> Self {
        Self::Worker {
            path,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentReloadStale {
    MissingDocument,
    PathChanged,
    VersionChanged,
    EncodingChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentReloadApply {
    Applied,
    Stale(DocumentReloadStale),
}

impl DocumentReloadWork {
    pub fn document(&self) -> DocumentId {
        self.document
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn execute(self) -> Result<PreparedDocumentReload, DocumentReloadError> {
        let disk = read_stable_snapshot(&self.path, self.encoding)?;
        let transaction = helix_core::diff::compare_ropes(&self.text, &disk.text);
        let indent = self.format.forced_indent.unwrap_or_else(|| {
            auto_detect_indent_style(&disk.text).unwrap_or(self.format.fallback_indent)
        });
        let line_ending = self
            .format
            .forced_line_ending
            .or_else(|| auto_detect_line_ending(&disk.text))
            .unwrap_or(self.format.fallback_line_ending);

        let diff_base = self
            .diff_providers
            .get_diff_base(&self.path)
            .and_then(|bytes| from_reader(&mut bytes.as_slice(), Some(self.encoding)).ok())
            .map(|(text, _, _)| text);
        let version_control_head = self.diff_providers.get_current_head_name(&self.path);

        Ok(PreparedDocumentReload {
            document: self.document,
            path: self.path,
            version: self.version,
            encoding: self.encoding,
            transaction,
            modified: disk.modified,
            readonly: disk.readonly,
            indent,
            line_ending,
            diff_base,
            version_control_head,
        })
    }
}

fn read_stable_snapshot(
    path: &Path,
    encoding: &'static Encoding,
) -> Result<DiskSnapshot, DocumentReloadError> {
    for _ in 0..MAX_STABLE_READ_ATTEMPTS {
        let mut file = File::open(path).map_err(|source| DocumentReloadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let before = file.metadata().map_err(|source| DocumentReloadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let (text, _, _) =
            from_reader(&mut file, Some(encoding)).map_err(|source| DocumentReloadError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let after = file.metadata().map_err(|source| DocumentReloadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let current = std::fs::metadata(path).map_err(|source| DocumentReloadError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let before = DiskFingerprint::from(&before);
        let after = DiskFingerprint::from(&after);
        let current = DiskFingerprint::from(&current);
        if before != after || after != current {
            continue;
        }

        return Ok(DiskSnapshot {
            text,
            modified: current.modified.unwrap_or_else(SystemTime::now),
            readonly: helix_stdx::faccess::readonly(path),
        });
    }

    Err(DocumentReloadError::Unstable {
        path: path.to_path_buf(),
    })
}

impl Editor {
    pub fn cache_prepared_document_open(&mut self, prepared: PreparedDocumentOpen) {
        debug_assert!(prepared.role.is_preview());
        self.prepared_document_opens.insert(prepared);
    }

    pub fn take_prepared_document_open(&mut self, path: &Path) -> Option<PreparedDocumentOpen> {
        let path = helix_stdx::path::canonicalize(path);
        self.prepared_document_opens.take(&path)
    }

    /// Capture an uncached document load. Calling this does not touch the filesystem.
    pub fn prepare_document_open(&self, path: &Path, role: DocumentOpenRole) -> DocumentOpenWork {
        DocumentOpenWork {
            path: helix_stdx::path::canonicalize(path),
            role,
            config: self.config.clone(),
            syn_loader: self.syn_loader.clone(),
            diff_providers: self.diff_providers.clone(),
        }
    }

    /// Insert a prepared document, or reuse a document opened while its worker was running.
    pub fn apply_prepared_document_open(
        &mut self,
        prepared: PreparedDocumentOpen,
        action: super::Action,
    ) -> DocumentId {
        if let Some(document) = self.document_id_by_path(&prepared.path) {
            if !prepared.role.is_preview() {
                self.promote_preview_document(document);
            }
            self.switch(document, action);
            return document;
        }

        let PreparedDocumentOpen {
            path,
            role,
            mut document,
            diff_base,
            version_control_head,
        } = prepared;
        let diagnostics =
            Self::doc_diagnostics(&self.language_servers, &self.diagnostics, &document)
                .collect::<Vec<_>>();
        document.replace_diagnostics(diagnostics, &[], None);

        let document = self.new_document(document);
        let redraw = self.document_redraw_handle();
        let doc = self
            .documents
            .get_mut(&document)
            .expect("newly opened document disappeared");
        doc.set_decoded_diff_base(diff_base, &redraw);
        doc.set_version_control_head(version_control_head);
        if !role.is_preview() {
            doc.promote_from_preview();
        }
        let diagnostics = doc.diagnostics().len();
        let has_syntax = doc.has_syntax();
        let has_diff_base = doc.diff_handle().is_some();
        let language = doc.language_name().unwrap_or("<none>").to_owned();
        let _ = doc;

        if !role.is_preview() {
            self.launch_language_servers(document);
            self.dispatch_document_open(document, &path);
        }
        self.switch(document, action);
        log::info!(
            "[document_open] apply path={} doc={document:?} role={role:?} language={} syntax={} diagnostics={} diff_base={} documents={}",
            path.display(),
            language,
            has_syntax,
            diagnostics,
            has_diff_base,
            self.document_count(),
        );
        document
    }

    /// Snapshot a document for background reload without touching the filesystem.
    pub fn prepare_document_reload(&self, document: DocumentId) -> Option<DocumentReloadWork> {
        let doc = self.documents.get(&document)?;
        Some(DocumentReloadWork {
            document,
            path: doc.path()?.clone(),
            version: doc.version(),
            encoding: doc.encoding(),
            text: doc.text().clone(),
            format: doc.reload_format_config(),
            diff_providers: self.diff_providers.clone(),
        })
    }

    /// Apply a prepared reload if the document still represents the worker snapshot.
    pub fn apply_prepared_document_reload(
        &mut self,
        prepared: PreparedDocumentReload,
    ) -> DocumentReloadApply {
        let Some(doc) = self.documents.get(&prepared.document) else {
            return DocumentReloadApply::Stale(DocumentReloadStale::MissingDocument);
        };
        if doc.path() != Some(&prepared.path) {
            return DocumentReloadApply::Stale(DocumentReloadStale::PathChanged);
        }
        if doc.version() != prepared.version {
            return DocumentReloadApply::Stale(DocumentReloadStale::VersionChanged);
        }
        if !std::ptr::eq(doc.encoding(), prepared.encoding) {
            return DocumentReloadApply::Stale(DocumentReloadStale::EncodingChanged);
        }

        let fallback_view = self.tree.focus;
        let mut view_ids: Vec<_> = doc
            .selections()
            .keys()
            .copied()
            .filter(|view| self.tree.contains(*view) || self.component_views.contains_key(view))
            .collect();
        if view_ids.is_empty() {
            self.documents
                .get_mut(&prepared.document)
                .expect("reload document disappeared")
                .ensure_view_init(fallback_view);
            view_ids.push(fallback_view);
        }
        let primary_view = view_ids[0];
        let redraw = self.document_redraw_handle();
        let auto_fetch_blame = self.config().inline_blame.auto_fetch;
        let document = prepared.document;
        let path = prepared.path.clone();

        let should_request_blame =
            self.with_view_doc_mut(primary_view, document, move |view, doc| {
                view.sync_changes(doc);
                doc.apply(&prepared.transaction, view.id());
                doc.append_changes_to_history(view);
                doc.reset_modified();
                doc.apply_reloaded_disk_state(prepared.modified, prepared.readonly);
                doc.set_indent_style(prepared.indent);
                doc.set_line_ending(prepared.line_ending);
                doc.set_decoded_diff_base(prepared.diff_base, &redraw);
                doc.set_version_control_head(prepared.version_control_head);
                let request = doc.should_request_full_file_blame(auto_fetch_blame);
                doc.mark_blame_outdated();
                request
            });

        let scrolloff = self.config().scrolloff;
        let Self {
            tree, documents, ..
        } = self;
        let doc = documents
            .get_mut(&document)
            .expect("reload document disappeared during apply");
        for view_id in view_ids {
            if tree.contains(view_id) {
                let view = tree.get_mut(view_id);
                if view.doc == document {
                    view.ensure_cursor_in_view(doc, scrolloff);
                }
            }
        }
        let _ = doc;

        self.notify_file_changed(path.clone());
        if should_request_blame {
            self.request_blame(BlameEvent {
                path,
                doc_id: document,
                line: None,
            });
        }
        self.mark_redraw_pending();
        self.request_redraw();

        DocumentReloadApply::Applied
    }
}

#[cfg(test)]
mod tests {
    use helix_core::Transaction;

    use super::*;
    use crate::editor::{test_support::collab_test_editor, Action};

    #[test]
    fn prepared_reload_applies_disk_text_and_file_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("document.txt");
        std::fs::write(&path, "from disk\r\n").unwrap();
        let mut editor = collab_test_editor();
        let document = editor.focused_document_id();
        editor.document_mut(document).unwrap().set_path(Some(&path));

        let prepared = editor
            .prepare_document_reload(document)
            .unwrap()
            .execute()
            .unwrap();
        assert_eq!(
            editor.apply_prepared_document_reload(prepared),
            DocumentReloadApply::Applied
        );

        let doc = editor.document(document).unwrap();
        assert_eq!(doc.text().to_string(), "from disk\r\n");
        assert_eq!(doc.line_ending(), LineEnding::Crlf);
        assert!(!doc.is_modified());
    }

    #[test]
    fn prepared_reload_never_overwrites_a_newer_document_edit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("document.txt");
        std::fs::write(&path, "from disk").unwrap();
        let mut editor = collab_test_editor();
        let document = editor.focused_document_id();
        let view = editor.focused_view_id();
        editor.document_mut(document).unwrap().set_path(Some(&path));
        let prepared = editor
            .prepare_document_reload(document)
            .unwrap()
            .execute()
            .unwrap();

        let doc = editor.document_mut(document).unwrap();
        let transaction = Transaction::insert(doc.text(), doc.selection(view), "user edit".into());
        doc.apply(&transaction, view);
        let edited_text = doc.text().to_string();

        assert_eq!(
            editor.apply_prepared_document_reload(prepared),
            DocumentReloadApply::Stale(DocumentReloadStale::VersionChanged)
        );
        assert_eq!(
            editor.document(document).unwrap().text().to_string(),
            edited_text
        );
    }

    #[test]
    fn prepared_reload_never_applies_after_document_path_changes() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.txt");
        let second = temp.path().join("second.txt");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&second, "second").unwrap();
        let mut editor = collab_test_editor();
        let document = editor.focused_document_id();
        editor
            .document_mut(document)
            .unwrap()
            .set_path(Some(&first));
        let prepared = editor
            .prepare_document_reload(document)
            .unwrap()
            .execute()
            .unwrap();
        editor
            .document_mut(document)
            .unwrap()
            .set_path(Some(&second));
        let unchanged_text = editor.document(document).unwrap().text().to_string();

        assert_eq!(
            editor.apply_prepared_document_reload(prepared),
            DocumentReloadApply::Stale(DocumentReloadStale::PathChanged)
        );
        assert_eq!(
            editor.document(document).unwrap().text().to_string(),
            unchanged_text
        );
    }

    #[test]
    fn interactive_open_consumes_and_promotes_cached_prepared_preview() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("preview.txt");
        std::fs::write(&path, "prepared\n").unwrap();
        let mut editor = collab_test_editor();
        let prepared = editor
            .prepare_document_open(&path, DocumentOpenRole::Preview)
            .execute()
            .unwrap();
        editor.cache_prepared_document_open(prepared);
        std::fs::write(&path, "changed after preview\n").unwrap();

        let document = editor.open(&path, Action::Replace).unwrap();

        let doc = editor.document(document).unwrap();
        assert_eq!(doc.text().to_string(), "prepared\n");
        assert!(!doc.is_preview());
        assert!(editor.take_prepared_document_open(&path).is_none());
    }
}
