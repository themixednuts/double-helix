use anyhow::{anyhow, Error};
use helix_core::encoding::Encoding;
use once_cell::sync::OnceCell;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DocumentLocation {
    Local(PathBuf),
    Remote(RemoteDocumentLocation),
    Collaboration(CollaborationDocumentLocation),
}

impl DocumentLocation {
    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path),
            Self::Remote(_) | Self::Collaboration(_) => None,
        }
    }

    pub fn remote(&self) -> Option<&RemoteDocumentLocation> {
        match self {
            Self::Remote(location) => Some(location),
            Self::Local(_) | Self::Collaboration(_) => None,
        }
    }

    pub fn collaboration(&self) -> Option<&CollaborationDocumentLocation> {
        match self {
            Self::Collaboration(location) => Some(location),
            Self::Local(_) | Self::Remote(_) => None,
        }
    }
}

impl std::fmt::Display for DocumentLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(path) => write!(formatter, "{}", path.display()),
            Self::Remote(location) => write!(formatter, "{}", location.resource_url()),
            Self::Collaboration(location) => write!(formatter, "{}", location.resource_url()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CollaborationDocumentLocation {
    pub project: helix_collab::ProjectId,
    pub path: helix_workspace::WorkspacePath,
    resource_url: Arc<Url>,
}

impl CollaborationDocumentLocation {
    pub fn new(
        project: helix_collab::ProjectId,
        path: helix_workspace::WorkspacePath,
    ) -> Result<Self, url::ParseError> {
        let resource_url = helix_collab::uri::document_url(project, &path);
        Ok(Self {
            project,
            path,
            resource_url: Arc::new(resource_url),
        })
    }

    pub fn resource_url(&self) -> &Url {
        &self.resource_url
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemoteDocumentLocation {
    pub authority: Arc<str>,
    pub session: helix_remote::SessionId,
    pub root: Arc<str>,
    pub path: helix_remote::WorkspacePath,
    pub content: helix_remote::ContentId,
    pub readonly: bool,
    pub path_separator: char,
    resource_url: Arc<Url>,
    lsp_url: Arc<Url>,
}

impl RemoteDocumentLocation {
    pub fn new(
        authority: impl Into<Arc<str>>,
        session: helix_remote::SessionId,
        root: impl Into<Arc<str>>,
        path: helix_remote::WorkspacePath,
        content: helix_remote::ContentId,
        readonly: bool,
        path_separator: char,
    ) -> Result<Self, url::ParseError> {
        let authority = authority.into();
        let root = root.into();
        let lsp_path = helix_remote::uri::url_path(&root, &path, path_separator);
        let lsp_url = helix_remote::uri::file_url(&root, &path, path_separator);
        let mut resource_url = Url::parse(&format!("ssh://{authority}/"))?;
        resource_url.set_path(&lsp_path);
        Ok(Self {
            authority,
            session,
            root,
            path,
            content,
            readonly,
            path_separator,
            resource_url: Arc::new(resource_url),
            lsp_url: Arc::new(lsp_url),
        })
    }

    pub fn absolute_path(&self) -> String {
        helix_remote::uri::absolute_path(&self.root, &self.path, self.path_separator)
    }

    pub fn resource_url(&self) -> &Url {
        &self.resource_url
    }

    pub fn lsp_url(&self) -> &Url {
        &self.lsp_url
    }

    pub fn with_path(&self, path: helix_remote::WorkspacePath) -> Result<Self, url::ParseError> {
        Self::new(
            self.authority.clone(),
            self.session,
            self.root.clone(),
            path,
            self.content,
            self.readonly,
            self.path_separator,
        )
    }
}

#[derive(Debug)]
pub struct FileBoundState {
    location: Option<DocumentLocation>,
    relative_path: OnceCell<Option<PathBuf>>,
    encoding: &'static Encoding,
    has_bom: bool,
    last_saved_time: SystemTime,
    last_saved_revision: usize,
    readonly: bool,
}

impl FileBoundState {
    pub fn new(encoding: &'static Encoding, has_bom: bool) -> Self {
        Self {
            location: None,
            relative_path: OnceCell::new(),
            encoding,
            has_bom,
            last_saved_time: SystemTime::now(),
            last_saved_revision: 0,
            readonly: false,
        }
    }

    pub fn clear_relative_path(&mut self) {
        self.relative_path.take();
    }

    pub fn set_encoding(&mut self, label: &str) -> Result<(), Error> {
        let encoding =
            Encoding::for_label(label.as_bytes()).ok_or_else(|| anyhow!("unknown encoding"))?;
        self.encoding = encoding;
        Ok(())
    }

    pub fn encoding(&self) -> &'static Encoding {
        self.encoding
    }

    pub fn encoding_with_bom_info(&self) -> (&'static Encoding, bool) {
        (self.encoding, self.has_bom)
    }

    pub fn path(&self) -> Option<&PathBuf> {
        match &self.location {
            Some(DocumentLocation::Local(path)) => Some(path),
            Some(DocumentLocation::Remote(_) | DocumentLocation::Collaboration(_)) | None => None,
        }
    }

    pub fn location(&self) -> Option<&DocumentLocation> {
        self.location.as_ref()
    }

    pub fn remote(&self) -> Option<&RemoteDocumentLocation> {
        match &self.location {
            Some(DocumentLocation::Remote(location)) => Some(location),
            Some(DocumentLocation::Local(_) | DocumentLocation::Collaboration(_)) | None => None,
        }
    }

    pub fn collaboration(&self) -> Option<&CollaborationDocumentLocation> {
        match &self.location {
            Some(DocumentLocation::Collaboration(location)) => Some(location),
            Some(DocumentLocation::Local(_) | DocumentLocation::Remote(_)) | None => None,
        }
    }

    pub fn language_path(&self) -> Option<Cow<'_, Path>> {
        match self.location.as_ref()? {
            DocumentLocation::Local(path) => Some(Cow::Borrowed(path)),
            DocumentLocation::Remote(location) => Some(Cow::Owned(location.path.to_path_buf())),
            DocumentLocation::Collaboration(location) => {
                Some(Cow::Owned(location.path.to_path_buf()))
            }
        }
    }

    pub fn set_path(&mut self, path: Option<&Path>) {
        self.location = path
            .map(helix_stdx::path::canonicalize)
            .map(DocumentLocation::Local);
        self.clear_relative_path();
        self.detect_readonly();
        self.pickup_last_saved_time();
    }

    pub fn set_remote(&mut self, location: RemoteDocumentLocation) {
        self.last_saved_time = remote_saved_time(location.content);
        self.readonly = location.readonly;
        self.location = Some(DocumentLocation::Remote(location));
        self.clear_relative_path();
    }

    pub fn set_collaboration(&mut self, location: CollaborationDocumentLocation, readonly: bool) {
        self.last_saved_time = SystemTime::now();
        self.readonly = readonly;
        self.location = Some(DocumentLocation::Collaboration(location));
        self.clear_relative_path();
    }

    pub fn set_collaboration_readonly(&mut self, readonly: bool) {
        if matches!(self.location, Some(DocumentLocation::Collaboration(_))) {
            self.readonly = readonly;
        }
    }

    pub fn set_collaboration_path(
        &mut self,
        path: helix_workspace::WorkspacePath,
    ) -> Result<bool, url::ParseError> {
        let Some(DocumentLocation::Collaboration(current)) = &self.location else {
            return Ok(false);
        };
        if current.path == path {
            return Ok(false);
        }
        let location = CollaborationDocumentLocation::new(current.project, path)?;
        self.location = Some(DocumentLocation::Collaboration(location));
        self.clear_relative_path();
        Ok(true)
    }

    pub fn set_remote_path(
        &mut self,
        path: helix_remote::WorkspacePath,
    ) -> Result<bool, url::ParseError> {
        let Some(DocumentLocation::Remote(current)) = &self.location else {
            return Ok(false);
        };
        if current.path == path {
            return Ok(false);
        }
        self.location = Some(DocumentLocation::Remote(current.with_path(path)?));
        self.clear_relative_path();
        Ok(true)
    }

    pub fn set_remote_saved(&mut self, content: helix_remote::ContentId, readonly: bool) {
        let Some(DocumentLocation::Remote(location)) = &mut self.location else {
            return;
        };
        location.content = content;
        location.readonly = readonly;
        self.last_saved_time = remote_saved_time(content);
        self.readonly = readonly;
    }

    pub fn url(&self) -> Option<Url> {
        match self.location.as_ref()? {
            DocumentLocation::Local(path) => Url::from_file_path(path).ok(),
            DocumentLocation::Remote(location) => Some(location.lsp_url().clone()),
            DocumentLocation::Collaboration(location) => Some(location.resource_url().clone()),
        }
    }

    pub fn uri(&self) -> Option<helix_core::Uri> {
        match self.location.as_ref()? {
            DocumentLocation::Local(path) => Some(path.clone().into()),
            DocumentLocation::Remote(location) => Some(helix_core::Uri::Resource(Arc::new(
                location.resource_url().clone(),
            ))),
            DocumentLocation::Collaboration(location) => Some(helix_core::Uri::Resource(Arc::new(
                location.resource_url().clone(),
            ))),
        }
    }

    pub fn relative_path(&self) -> Option<&Path> {
        self.relative_path
            .get_or_init(|| {
                self.location.as_ref().map(|location| match location {
                    DocumentLocation::Local(path) => {
                        helix_stdx::path::get_relative_path(path).to_path_buf()
                    }
                    DocumentLocation::Remote(remote) => remote.path.to_path_buf(),
                    DocumentLocation::Collaboration(collaboration) => {
                        collaboration.path.to_path_buf()
                    }
                })
            })
            .as_deref()
    }

    pub fn display_name<'a>(&'a self, scratch_name: &'a str) -> Cow<'a, str> {
        self.relative_path()
            .map_or_else(|| scratch_name.into(), |path| path.to_string_lossy())
    }

    pub fn pickup_last_saved_time(&mut self) {
        self.last_saved_time = match self.location.as_ref() {
            Some(DocumentLocation::Local(path)) => match path.metadata() {
                Ok(metadata) => match metadata.modified() {
                    Ok(mtime) => mtime,
                    Err(err) => {
                        log::debug!(
                            "Could not fetch file system's mtime, falling back to current system time: {}",
                            err
                        );
                        SystemTime::now()
                    }
                },
                Err(err) => {
                    log::debug!(
                        "Could not fetch file system's mtime, falling back to current system time: {}",
                        err
                    );
                    SystemTime::now()
                }
            },
            Some(DocumentLocation::Remote(location)) => remote_saved_time(location.content),
            Some(DocumentLocation::Collaboration(_)) => SystemTime::now(),
            None => SystemTime::now(),
        };
    }

    pub fn last_saved_time(&self) -> SystemTime {
        self.last_saved_time
    }

    pub(crate) fn apply_disk_state(&mut self, last_saved_time: SystemTime, readonly: bool) {
        self.last_saved_time = last_saved_time;
        self.readonly = readonly;
    }

    pub fn detect_readonly(&mut self) {
        self.readonly = match self.location.as_ref() {
            None => false,
            Some(DocumentLocation::Local(path)) => helix_stdx::faccess::readonly(path),
            Some(DocumentLocation::Remote(location)) => location.readonly,
            Some(DocumentLocation::Collaboration(_)) => self.readonly,
        };
    }

    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn is_modified(&self, current_revision: usize, has_pending_changes: bool) -> bool {
        current_revision != self.last_saved_revision || has_pending_changes
    }

    pub fn reset_modified(&mut self, current_revision: usize) {
        self.last_saved_revision = current_revision;
    }

    pub fn set_last_saved_revision(&mut self, rev: usize, save_time: SystemTime) {
        self.last_saved_revision = rev;
        self.last_saved_time = save_time;
    }

    pub fn last_saved_revision(&self) -> usize {
        self.last_saved_revision
    }
}

fn remote_saved_time(content: helix_remote::ContentId) -> SystemTime {
    content
        .modified_unix_nanos
        .map(Duration::from_nanos)
        .and_then(|duration| UNIX_EPOCH.checked_add(duration))
        .unwrap_or_else(SystemTime::now)
}
