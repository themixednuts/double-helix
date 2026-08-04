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
    Background,
}

impl DocumentOpenRole {
    pub const fn is_preview(self) -> bool {
        matches!(self, Self::Preview)
    }

    pub const fn is_visible(self) -> bool {
        !matches!(self, Self::Background)
    }

    pub const fn activates_language_servers(self) -> bool {
        !self.is_preview()
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

pub struct RemoteDocumentOpenWork {
    path: helix_remote::WorkspacePath,
    role: DocumentOpenRole,
    backend: Arc<helix_remote::backend::RemoteWorkspaceClient>,
    config: Arc<dyn DynAccess<super::Config> + Send + Sync>,
    syn_loader: Arc<ArcSwap<helix_core::syntax::Loader>>,
}

pub struct CollaborationDocumentOpenWork {
    path: helix_workspace::WorkspacePath,
    role: DocumentOpenRole,
    session: helix_collab::GuestSessionHandle,
    config: Arc<dyn DynAccess<super::Config> + Send + Sync>,
    syn_loader: Arc<ArcSwap<helix_core::syntax::Loader>>,
}

pub enum WorkspaceDocumentOpenWork {
    Local(DocumentOpenWork),
    Remote(RemoteDocumentOpenWork),
    Collaboration(CollaborationDocumentOpenWork),
    Failed {
        path: super::WorkspaceDocumentPath,
        role: DocumentOpenRole,
        error: DocumentOpenError,
    },
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

pub struct PreparedRemoteDocumentOpen {
    path: helix_remote::WorkspacePath,
    role: DocumentOpenRole,
    document: Document,
}

pub struct PreparedCollaborationDocumentOpen {
    path: helix_workspace::WorkspacePath,
    buffer: helix_collab::BufferId,
    role: DocumentOpenRole,
    document: Document,
}

pub enum PreparedWorkspaceDocumentOpen {
    Local(PreparedDocumentOpen),
    Remote(PreparedRemoteDocumentOpen),
    Collaboration(PreparedCollaborationDocumentOpen),
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

impl std::fmt::Debug for RemoteDocumentOpenWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteDocumentOpenWork")
            .field("path", &self.path)
            .field("role", &self.role)
            .field("authority", &self.backend.authority())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PreparedRemoteDocumentOpen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedRemoteDocumentOpen")
            .field("path", &self.path)
            .field("role", &self.role)
            .field("bytes", &self.document.text().len_bytes())
            .field("language", &self.document.language_name())
            .field("has_syntax", &self.document.has_syntax())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for CollaborationDocumentOpenWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CollaborationDocumentOpenWork")
            .field("path", &self.path)
            .field("role", &self.role)
            .field("project", &self.session.project().id)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PreparedCollaborationDocumentOpen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedCollaborationDocumentOpen")
            .field("path", &self.path)
            .field("buffer", &self.buffer)
            .field("role", &self.role)
            .field("bytes", &self.document.text().len_bytes())
            .field("language", &self.document.language_name())
            .field("has_syntax", &self.document.has_syntax())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for WorkspaceDocumentOpenWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(work) => work.fmt(formatter),
            Self::Remote(work) => work.fmt(formatter),
            Self::Collaboration(work) => work.fmt(formatter),
            Self::Failed { path, role, error } => formatter
                .debug_struct("WorkspaceDocumentOpenWork::Failed")
                .field("path", path)
                .field("role", role)
                .field("error", error)
                .finish(),
        }
    }
}

impl std::fmt::Debug for PreparedWorkspaceDocumentOpen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(prepared) => prepared.fmt(formatter),
            Self::Remote(prepared) => prepared.fmt(formatter),
            Self::Collaboration(prepared) => prepared.fmt(formatter),
        }
    }
}

impl WorkspaceDocumentOpenWork {
    pub fn path(&self) -> super::WorkspaceDocumentPath {
        match self {
            Self::Local(work) => super::WorkspaceDocumentPath::Local(work.path.clone()),
            Self::Remote(work) => super::WorkspaceDocumentPath::Remote(work.path.clone()),
            Self::Collaboration(work) => super::WorkspaceDocumentPath::Collaboration {
                project: work.session.project().id,
                path: work.path.clone(),
            },
            Self::Failed { path, .. } => path.clone(),
        }
    }

    pub fn role(&self) -> DocumentOpenRole {
        match self {
            Self::Local(work) => work.role,
            Self::Remote(work) => work.role,
            Self::Collaboration(work) => work.role,
            Self::Failed { role, .. } => *role,
        }
    }
}

impl PreparedWorkspaceDocumentOpen {
    pub fn path(&self) -> super::WorkspaceDocumentPath {
        match self {
            Self::Local(prepared) => super::WorkspaceDocumentPath::Local(prepared.path.clone()),
            Self::Remote(prepared) => super::WorkspaceDocumentPath::Remote(prepared.path.clone()),
            Self::Collaboration(prepared) => {
                let project = prepared
                    .document
                    .collaboration_location()
                    .expect("prepared collaboration document has no location")
                    .project;
                super::WorkspaceDocumentPath::Collaboration {
                    project,
                    path: prepared.path.clone(),
                }
            }
        }
    }

    pub fn role(&self) -> DocumentOpenRole {
        match self {
            Self::Local(prepared) => prepared.role,
            Self::Remote(prepared) => prepared.role,
            Self::Collaboration(prepared) => prepared.role,
        }
    }

    pub fn document(&self) -> &Document {
        match self {
            Self::Local(prepared) => &prepared.document,
            Self::Remote(prepared) => &prepared.document,
            Self::Collaboration(prepared) => &prepared.document,
        }
    }

    pub fn document_mut(&mut self) -> &mut Document {
        match self {
            Self::Local(prepared) => &mut prepared.document,
            Self::Remote(prepared) => &mut prepared.document,
            Self::Collaboration(prepared) => &mut prepared.document,
        }
    }

    pub fn replace_initial_text(&mut self, text: String) {
        self.document_mut().replace_initial_text(Rope::from(text));
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
            if let Some(request) = document.prepare_syntax_refresh() {
                match request.execute() {
                    Ok(syntax) => document.set_syntax(Some(syntax)),
                    Err(error) => log::warn!(
                        "[document_open] initial syntax parse failed path={} role={:?} error={error}",
                        self.path.display(),
                        self.role,
                    ),
                }
            }
        }
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

impl RemoteDocumentOpenWork {
    pub fn path(&self) -> &helix_remote::WorkspacePath {
        &self.path
    }

    pub fn role(&self) -> DocumentOpenRole {
        self.role
    }

    pub async fn metadata(
        &self,
        canceled: tokio_util::sync::CancellationToken,
    ) -> Result<Option<helix_remote::FileMetadata>, DocumentOpenError> {
        self.backend
            .stat(self.path.clone(), canceled)
            .await
            .map_err(|error| DocumentOpenError::Remote(error.to_string()))
    }

    pub async fn execute(
        self,
        canceled: tokio_util::sync::CancellationToken,
        inspect_binary: bool,
    ) -> Result<PreparedRemoteDocumentOpen, DocumentOpenError> {
        let file = self
            .backend
            .read_file(self.path.clone(), canceled)
            .await
            .map_err(|error| DocumentOpenError::Remote(error.to_string()))?;
        if inspect_binary
            && content_inspector::inspect(&file.bytes[..file.bytes.len().min(1024)]).is_binary()
        {
            return Err(DocumentOpenError::BinaryFile);
        }
        let content = file.metadata.content.ok_or_else(|| {
            DocumentOpenError::Remote("remote file has no content generation".to_owned())
        })?;
        let location = crate::file_bound::RemoteDocumentLocation::new(
            self.backend.authority(),
            self.backend.workspace().session,
            self.backend.workspace().root.clone(),
            self.path.clone(),
            content,
            file.metadata.readonly,
            self.backend.hello().platform.path_separator,
        )
        .map_err(|error| DocumentOpenError::Remote(error.to_string()))?;
        let role = self.role;
        let path = self.path;
        let log_path = path.clone();
        let config = self.config;
        let syn_loader = self.syn_loader;
        let mut document = tokio::task::spawn_blocking(move || {
            let mut document = Document::open_remote(
                &file.bytes,
                location,
                None,
                LanguageInitialization::Full,
                config,
                syn_loader,
            )?;
            if role.is_preview() {
                if let Some(request) = document.prepare_syntax_refresh() {
                    match request.execute() {
                        Ok(syntax) => document.set_syntax(Some(syntax)),
                        Err(error) => log::warn!(
                            "[document_open] remote initial syntax parse failed path={} error={error}",
                            log_path,
                        ),
                    }
                }
                document.mark_preview();
            }
            Ok::<_, DocumentOpenError>(document)
        })
        .await
        .map_err(|error| DocumentOpenError::Worker(error.to_string()))??;
        if role.is_preview() && !document.is_preview() {
            document.mark_preview();
        }
        Ok(PreparedRemoteDocumentOpen {
            path,
            role,
            document,
        })
    }
}

impl CollaborationDocumentOpenWork {
    pub fn path(&self) -> &helix_workspace::WorkspacePath {
        &self.path
    }

    pub fn role(&self) -> DocumentOpenRole {
        self.role
    }

    pub async fn execute(self) -> Result<PreparedCollaborationDocumentOpen, DocumentOpenError> {
        let opened = self
            .session
            .open(self.path.clone())
            .await
            .map_err(|error| DocumentOpenError::Collaboration(error.to_string()))?;
        let location = crate::file_bound::CollaborationDocumentLocation::new(
            self.session.project().id,
            self.path.clone(),
        )
        .map_err(|error| DocumentOpenError::Collaboration(error.to_string()))?;
        let role = self.role;
        let participant_role = self.session.participant().role;
        let path = self.path;
        let log_path = path.clone();
        let config = self.config;
        let syn_loader = self.syn_loader;
        let text = opened.text;
        let mut document = tokio::task::spawn_blocking(move || {
            let mut document = Document::open_collaboration(
                text,
                location,
                participant_role,
                config,
                syn_loader,
            );
            if role.is_preview() {
                if let Some(request) = document.prepare_syntax_refresh() {
                    match request.execute() {
                        Ok(syntax) => document.set_syntax(Some(syntax)),
                        Err(error) => log::warn!(
                            "[document_open] collaboration initial syntax parse failed path={} error={error}",
                            log_path,
                        ),
                    }
                }
                document.mark_preview();
            }
            document
        })
        .await
        .map_err(|error| DocumentOpenError::Worker(error.to_string()))?;
        if role.is_preview() && !document.is_preview() {
            document.mark_preview();
        }
        Ok(PreparedCollaborationDocumentOpen {
            path,
            buffer: opened.buffer,
            role,
            document,
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

impl PreparedRemoteDocumentOpen {
    pub fn path(&self) -> &helix_remote::WorkspacePath {
        &self.path
    }

    pub fn role(&self) -> DocumentOpenRole {
        self.role
    }

    pub fn document(&self) -> &Document {
        &self.document
    }
}

impl PreparedCollaborationDocumentOpen {
    pub fn path(&self) -> &helix_workspace::WorkspacePath {
        &self.path
    }

    pub fn role(&self) -> DocumentOpenRole {
        self.role
    }

    pub fn buffer(&self) -> helix_collab::BufferId {
        self.buffer
    }

    pub fn document(&self) -> &Document {
        &self.document
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
    pub fn prepare_workspace_document_open(
        &self,
        path: super::WorkspaceDocumentPath,
        role: DocumentOpenRole,
    ) -> WorkspaceDocumentOpenWork {
        match path {
            super::WorkspaceDocumentPath::Local(path) => {
                WorkspaceDocumentOpenWork::Local(self.prepare_document_open(&path, role))
            }
            super::WorkspaceDocumentPath::Remote(path) => {
                match self.prepare_remote_document_open(path.clone(), role) {
                    Ok(work) => WorkspaceDocumentOpenWork::Remote(work),
                    Err(error) => WorkspaceDocumentOpenWork::Failed {
                        path: super::WorkspaceDocumentPath::Remote(path),
                        role,
                        error,
                    },
                }
            }
            super::WorkspaceDocumentPath::Collaboration { project, path } => {
                match self.prepare_collaboration_document_open(project, path.clone(), role) {
                    Ok(work) => WorkspaceDocumentOpenWork::Collaboration(work),
                    Err(error) => WorkspaceDocumentOpenWork::Failed {
                        path: super::WorkspaceDocumentPath::Collaboration { project, path },
                        role,
                        error,
                    },
                }
            }
        }
    }

    pub fn document_id_by_workspace_path(
        &self,
        path: &super::WorkspaceDocumentPath,
    ) -> Option<DocumentId> {
        match path {
            super::WorkspaceDocumentPath::Local(path) => self.document_id_by_path(path),
            super::WorkspaceDocumentPath::Remote(path) => {
                let session = self
                    .workspace_backend
                    .remote()
                    .map(|remote| remote.workspace().session)?;
                self.documents.values().find_map(|document| {
                    document
                        .remote_location()
                        .is_some_and(|location| {
                            location.session == session && location.path == *path
                        })
                        .then_some(document.id())
                })
            }
            super::WorkspaceDocumentPath::Collaboration { project, path } => {
                self.documents.values().find_map(|document| {
                    document
                        .collaboration_location()
                        .is_some_and(|location| {
                            location.project == *project && location.path == *path
                        })
                        .then_some(document.id())
                })
            }
        }
    }

    pub fn apply_prepared_workspace_document_open(
        &mut self,
        prepared: PreparedWorkspaceDocumentOpen,
        action: super::Action,
    ) -> DocumentId {
        match prepared {
            PreparedWorkspaceDocumentOpen::Local(prepared) => {
                self.apply_prepared_document_open(prepared, action)
            }
            PreparedWorkspaceDocumentOpen::Remote(prepared) => {
                self.apply_prepared_remote_document_open(prepared, action)
            }
            PreparedWorkspaceDocumentOpen::Collaboration(prepared) => {
                self.apply_prepared_collaboration_document_open(prepared, action)
            }
        }
    }

    pub fn prepare_remote_document_open(
        &self,
        path: helix_remote::WorkspacePath,
        role: DocumentOpenRole,
    ) -> Result<RemoteDocumentOpenWork, DocumentOpenError> {
        let backend = self
            .workspace_backend
            .remote()
            .cloned()
            .ok_or_else(|| DocumentOpenError::Remote("workspace is not remote".to_owned()))?;
        Ok(RemoteDocumentOpenWork {
            path,
            role,
            backend,
            config: self.config.clone(),
            syn_loader: self.syn_loader.clone(),
        })
    }

    pub fn prepare_collaboration_document_open(
        &self,
        project: helix_collab::ProjectId,
        path: helix_workspace::WorkspacePath,
        role: DocumentOpenRole,
    ) -> Result<CollaborationDocumentOpenWork, DocumentOpenError> {
        let session = self
            .collaboration
            .session()
            .filter(|session| session.project().id == project)
            .ok_or_else(|| {
                DocumentOpenError::Collaboration(
                    "collaboration project is not connected".to_owned(),
                )
            })?;
        Ok(CollaborationDocumentOpenWork {
            path,
            role,
            session,
            config: self.config.clone(),
            syn_loader: self.syn_loader.clone(),
        })
    }

    pub fn apply_prepared_remote_document_open(
        &mut self,
        prepared: PreparedRemoteDocumentOpen,
        action: super::Action,
    ) -> DocumentId {
        let session = self
            .workspace_backend
            .remote()
            .map(|remote| remote.workspace().session);
        if let Some(document) = self.documents.values().find(|document| {
            document.remote_location().is_some_and(|location| {
                Some(location.session) == session && location.path == prepared.path
            })
        }) {
            let document = document.id();
            if prepared.role.activates_language_servers() {
                self.promote_preview_document(document);
            }
            if prepared.role.is_visible() {
                self.switch(document, action);
            }
            return document;
        }

        let PreparedRemoteDocumentOpen {
            path,
            role,
            mut document,
        } = prepared;
        let diagnostics =
            Self::doc_diagnostics(&self.language_servers, &self.diagnostics, &document)
                .collect::<Vec<_>>();
        document.replace_diagnostics(diagnostics, &[], None);
        let document = self.new_document(document);
        if role.activates_language_servers() {
            self.documents
                .get_mut(&document)
                .expect("newly opened remote document disappeared")
                .promote_from_preview();
            let location = self
                .documents
                .get(&document)
                .and_then(|document| document.location().cloned())
                .expect("remote document has no location");
            self.dispatch_document_open(document, &location);
        }
        if role.is_visible() {
            self.switch(document, action);
        }
        log::info!(
            "[document_open] apply_remote path={} doc={document:?} role={role:?} documents={}",
            path,
            self.document_count(),
        );
        document
    }

    pub fn apply_prepared_collaboration_document_open(
        &mut self,
        prepared: PreparedCollaborationDocumentOpen,
        action: super::Action,
    ) -> DocumentId {
        let project = prepared
            .document
            .collaboration_location()
            .expect("prepared collaboration document has no location")
            .project;
        if let Some(document) = self.documents.values().find(|document| {
            document.collaboration_location().is_some_and(|location| {
                location.project == project && location.path == prepared.path
            })
        }) {
            let document = document.id();
            if prepared.role.activates_language_servers() {
                self.promote_preview_document(document);
            }
            self.collaboration
                .bind(document, prepared.buffer, prepared.path.clone());
            if prepared.role.is_visible() {
                self.switch(document, action);
            }
            return document;
        }

        let PreparedCollaborationDocumentOpen {
            path,
            buffer,
            role,
            mut document,
        } = prepared;
        let diagnostics =
            Self::doc_diagnostics(&self.language_servers, &self.diagnostics, &document)
                .collect::<Vec<_>>();
        document.replace_diagnostics(diagnostics, &[], None);
        let document = self.new_document(document);
        self.collaboration.bind(document, buffer, path.clone());
        if role.activates_language_servers() {
            self.documents
                .get_mut(&document)
                .expect("newly opened collaboration document disappeared")
                .promote_from_preview();
            let location = self
                .documents
                .get(&document)
                .and_then(|document| document.location().cloned())
                .expect("collaboration document has no location");
            self.dispatch_document_open(document, &location);
        }
        if role.is_visible() {
            self.switch(document, action);
        }
        log::info!(
            "[document_open] apply_collaboration path={} doc={document:?} role={role:?} documents={}",
            path,
            self.document_count(),
        );
        document
    }

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
            if prepared.role.activates_language_servers() {
                self.promote_preview_document(document);
            }
            if prepared.role.is_visible() {
                self.switch(document, action);
            }
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
        if role.activates_language_servers() {
            doc.promote_from_preview();
        }
        let diagnostics = doc.diagnostics().len();
        let has_syntax = doc.has_syntax();
        let has_diff_base = doc.diff_handle().is_some();
        let language = doc.language_name().unwrap_or("<none>").to_owned();
        let _ = doc;

        if role.activates_language_servers() {
            self.launch_language_servers(document);
            self.dispatch_document_open(
                document,
                &crate::file_bound::DocumentLocation::Local(path.clone()),
            );
        }
        if role.is_visible() {
            self.switch(document, action);
        }
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

    #[test]
    fn prepared_preview_contains_its_initial_syntax_tree() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("preview.rs");
        std::fs::write(&path, "fn preview() -> usize { 42 }\n").unwrap();
        let editor = collab_test_editor();
        editor.syn_loader.store(std::sync::Arc::new(
            helix_core::config::default_lang_loader(),
        ));

        let prepared = editor
            .prepare_document_open(&path, DocumentOpenRole::Preview)
            .execute()
            .unwrap();

        assert_eq!(prepared.document().language_name(), Some("rust"));
        assert!(prepared.document().has_syntax());
    }

    #[test]
    fn interactive_open_defers_initial_syntax_to_the_syntax_service() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("interactive.rs");
        std::fs::write(&path, "fn interactive() -> usize { 42 }\n").unwrap();
        let editor = collab_test_editor();
        editor.syn_loader.store(std::sync::Arc::new(
            helix_core::config::default_lang_loader(),
        ));

        let prepared = editor
            .prepare_document_open(&path, DocumentOpenRole::Interactive)
            .execute()
            .unwrap();

        assert_eq!(prepared.document().language_name(), Some("rust"));
        assert!(!prepared.document().has_syntax());
    }

    #[test]
    fn background_open_activates_document_without_switching_focus() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("background.rs");
        std::fs::write(&path, "fn background() {}\n").unwrap();
        let mut editor = collab_test_editor();
        let focused = editor.focused_document_id();
        editor.syn_loader.store(std::sync::Arc::new(
            helix_core::config::default_lang_loader(),
        ));
        let prepared = editor
            .prepare_document_open(&path, DocumentOpenRole::Background)
            .execute()
            .unwrap();

        let opened = editor.apply_prepared_document_open(prepared, Action::Replace);

        assert_ne!(opened, focused);
        assert_eq!(editor.focused_document_id(), focused);
        assert!(!editor.document(opened).unwrap().is_preview());
        assert_eq!(
            editor.document(opened).unwrap().language_name(),
            Some("rust")
        );
    }
}
