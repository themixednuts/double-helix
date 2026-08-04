use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};

use helix_workspace::{WorkspacePath, WorkspacePathError};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceDocumentPath {
    Local(PathBuf),
    Remote(helix_remote::WorkspacePath),
    Collaboration {
        project: helix_collab::ProjectId,
        path: helix_workspace::WorkspacePath,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WorkspaceId {
    Local(PathBuf),
    Remote {
        authority: Arc<str>,
        session: helix_remote::SessionId,
    },
    Collaboration {
        project: helix_collab::ProjectId,
    },
}

/// The active filesystem authority and the root paths are resolved against.
///
/// Keeping the authority handle beside its typed root prevents UI and command
/// surfaces from accidentally falling back to the editor process filesystem.
#[derive(Clone)]
pub struct WorkspaceContext {
    root: WorkspaceDocumentPath,
    backend: WorkspaceBackend,
}

impl WorkspaceContext {
    pub fn from_backend(local_root: PathBuf, backend: &WorkspaceBackend) -> Self {
        match backend {
            WorkspaceBackend::Local => Self {
                root: WorkspaceDocumentPath::Local(local_root),
                backend: WorkspaceBackend::Local,
            },
            WorkspaceBackend::Remote(remote) => Self {
                root: WorkspaceDocumentPath::Remote(WorkspacePath::root()),
                backend: WorkspaceBackend::Remote(remote.clone()),
            },
            WorkspaceBackend::Collaboration(session) => Self {
                root: WorkspaceDocumentPath::Collaboration {
                    project: session.project().id,
                    path: WorkspacePath::root(),
                },
                backend: WorkspaceBackend::Collaboration(session.clone()),
            },
        }
    }

    pub fn from_root(
        root: WorkspaceDocumentPath,
        backend: &WorkspaceBackend,
    ) -> Result<Self, WorkspaceContextError> {
        let belongs = matches!(
            (backend, &root),
            (WorkspaceBackend::Local, WorkspaceDocumentPath::Local(_))
                | (
                    WorkspaceBackend::Remote(_),
                    WorkspaceDocumentPath::Remote(_)
                )
        ) || matches!(
            (backend, &root),
            (
                WorkspaceBackend::Collaboration(session),
                WorkspaceDocumentPath::Collaboration { project, .. }
            ) if *project == session.project().id
        );
        if !belongs {
            return Err(WorkspaceContextError::BackendMismatch {
                path: root.display(),
                backend: format!("{backend:?}"),
            });
        }
        Ok(Self {
            root,
            backend: backend.clone(),
        })
    }

    pub fn root(&self) -> &WorkspaceDocumentPath {
        &self.root
    }

    pub fn backend(&self) -> &WorkspaceBackend {
        &self.backend
    }

    pub fn remote(&self) -> Option<&Arc<helix_remote::backend::RemoteWorkspaceClient>> {
        self.backend.remote()
    }

    pub fn collaboration(&self) -> Option<&helix_collab::GuestSessionHandle> {
        self.backend.collaboration()
    }

    pub fn identity(&self) -> WorkspaceId {
        match &self.backend {
            WorkspaceBackend::Local => WorkspaceId::Local(
                self.root
                    .local_path()
                    .expect("local workspace context has a non-local root")
                    .to_path_buf(),
            ),
            WorkspaceBackend::Remote(remote) => WorkspaceId::Remote {
                authority: Arc::from(remote.authority()),
                session: remote.workspace().session,
            },
            WorkspaceBackend::Collaboration(session) => WorkspaceId::Collaboration {
                project: session.project().id,
            },
        }
    }

    pub fn resolve(&self, path: &Path) -> Result<WorkspaceDocumentPath, WorkspaceContextError> {
        self.root.resolve(path)
    }

    /// Resolve a path reported by a service running inside this workspace.
    /// Remote language servers report authority-absolute paths, which must be
    /// stripped against the negotiated remote root before opening a document.
    pub fn resolve_reported_path(
        &self,
        path: &Path,
    ) -> Result<WorkspaceDocumentPath, WorkspaceContextError> {
        let WorkspaceBackend::Remote(remote) = &self.backend else {
            return self.resolve(path);
        };
        let path = path.to_str().ok_or(WorkspaceContextError::NonUtf8)?;
        if let Some(path) = helix_remote::uri::workspace_path_from_absolute_path(
            path,
            &remote.workspace().root,
            remote.workspace().case_sensitive,
        ) {
            return Ok(WorkspaceDocumentPath::Remote(path));
        }
        if path.starts_with(['/', '\\'])
            || (path.len() >= 2
                && path.as_bytes()[0].is_ascii_alphabetic()
                && path.as_bytes()[1] == b':')
        {
            return Err(WorkspaceContextError::OutsideWorkspace(path.to_owned()));
        }
        self.resolve(Path::new(path))
    }

    pub fn root_label(&self) -> String {
        match &self.backend {
            WorkspaceBackend::Local => self
                .root
                .local_path()
                .and_then(Path::file_name)
                .filter(|name| !name.is_empty())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.root.display()),
            WorkspaceBackend::Remote(remote) => remote.workspace().display_name.clone(),
            WorkspaceBackend::Collaboration(session) => session.project().name,
        }
    }
}

impl std::fmt::Debug for WorkspaceContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceContext")
            .field("root", &self.root)
            .field("backend", &self.backend)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceContextError {
    #[error("workspace path '{path}' does not belong to backend {backend}")]
    BackendMismatch { path: String, backend: String },
    #[error("workspace paths must be valid UTF-8")]
    NonUtf8,
    #[error("invalid workspace path: {0}")]
    InvalidWorkspacePath(WorkspacePathError),
    #[error("reported path is outside the active workspace: {0}")]
    OutsideWorkspace(String),
}

impl WorkspaceDocumentPath {
    pub fn resolve(&self, path: &Path) -> Result<Self, WorkspaceContextError> {
        match self {
            Self::Local(root) => {
                let path = helix_stdx::path::expand_tilde(path);
                Ok(Self::Local(if path.is_absolute() {
                    path.into_owned()
                } else {
                    root.join(path.as_ref())
                }))
            }
            Self::Remote(root) => {
                let path = path.to_str().ok_or(WorkspaceContextError::NonUtf8)?;
                root.resolve_relative(path)
                    .map(Self::Remote)
                    .map_err(WorkspaceContextError::InvalidWorkspacePath)
            }
            Self::Collaboration {
                project,
                path: root,
            } => {
                let path = path.to_str().ok_or(WorkspaceContextError::NonUtf8)?;
                root.resolve_relative(path)
                    .map(|path| Self::Collaboration {
                        project: *project,
                        path,
                    })
                    .map_err(WorkspaceContextError::InvalidWorkspacePath)
            }
        }
    }

    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path),
            Self::Remote(_) | Self::Collaboration { .. } => None,
        }
    }

    pub fn remote_path(&self) -> Option<&helix_remote::WorkspacePath> {
        match self {
            Self::Remote(path) => Some(path),
            Self::Local(_) | Self::Collaboration { .. } => None,
        }
    }

    pub fn collaboration_path(&self) -> Option<&helix_workspace::WorkspacePath> {
        match self {
            Self::Collaboration { path, .. } => Some(path),
            Self::Local(_) | Self::Remote(_) => None,
        }
    }

    pub fn with_workspace_path(&self, path: helix_workspace::WorkspacePath) -> Option<Self> {
        match self {
            Self::Remote(_) => Some(Self::Remote(path)),
            Self::Collaboration { project, .. } => Some(Self::Collaboration {
                project: *project,
                path,
            }),
            Self::Local(_) => None,
        }
    }

    pub fn into_local(self) -> Option<PathBuf> {
        match self {
            Self::Local(path) => Some(path),
            Self::Remote(_) | Self::Collaboration { .. } => None,
        }
    }

    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::Local(path) => path.parent().map(|path| Self::Local(path.to_path_buf())),
            Self::Remote(path) => path.parent().map(Self::Remote),
            Self::Collaboration { project, path } => {
                path.parent().map(|path| Self::Collaboration {
                    project: *project,
                    path,
                })
            }
        }
    }

    pub fn join(&self, label: &str) -> Result<Self, helix_remote::WorkspacePathError> {
        match self {
            Self::Local(path) => Ok(Self::Local(path.join(label))),
            Self::Remote(path) => path.join(label.to_owned()).map(Self::Remote),
            Self::Collaboration { project, path } => {
                path.join(label.to_owned()).map(|path| Self::Collaboration {
                    project: *project,
                    path,
                })
            }
        }
    }

    pub fn join_relative(&self, relative: &Path) -> Result<Self, String> {
        let mut joined = self.clone();
        for component in relative.components() {
            let std::path::Component::Normal(segment) = component else {
                return Err(String::from(
                    "workspace path must contain only normal segments",
                ));
            };
            let segment = segment
                .to_str()
                .ok_or_else(|| String::from("workspace path must be valid UTF-8"))?;
            joined = joined.join(segment).map_err(|error| error.to_string())?;
        }
        Ok(joined)
    }

    pub fn starts_with(&self, root: &Self) -> bool {
        match (self, root) {
            (Self::Local(path), Self::Local(root)) => path.starts_with(root),
            (Self::Remote(path), Self::Remote(root)) => path.starts_with(root),
            (
                Self::Collaboration { project, path },
                Self::Collaboration {
                    project: root_project,
                    path: root,
                },
            ) => project == root_project && path.starts_with(root),
            _ => false,
        }
    }

    pub fn relative_to(&self, root: &Self) -> Option<Self> {
        match (self, root) {
            (Self::Local(path), Self::Local(root)) => path
                .strip_prefix(root)
                .ok()
                .map(|path| Self::Local(path.to_path_buf())),
            (Self::Remote(path), Self::Remote(root)) => path.strip_prefix(root).map(Self::Remote),
            (
                Self::Collaboration { project, path },
                Self::Collaboration {
                    project: root_project,
                    path: root,
                },
            ) if project == root_project => {
                path.strip_prefix(root).map(|path| Self::Collaboration {
                    project: *project,
                    path,
                })
            }
            _ => None,
        }
    }

    pub fn file_name(&self) -> Option<Cow<'_, str>> {
        match self {
            Self::Local(path) => path.file_name().map(|name| name.to_string_lossy()),
            Self::Remote(path) => path.file_name().map(Cow::Borrowed),
            Self::Collaboration { path, .. } => path.file_name().map(Cow::Borrowed),
        }
    }

    pub fn is_root(&self) -> bool {
        match self {
            Self::Local(path) => path.parent().is_none(),
            Self::Remote(path) => path.is_root(),
            Self::Collaboration { path, .. } => path.is_root(),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Remote(path) if path.is_root() => String::from("."),
            Self::Remote(path) => path.to_string(),
            Self::Collaboration { path, .. } if path.is_root() => String::from("."),
            Self::Collaboration { path, .. } => path.to_string(),
        }
    }

    pub fn model_id(&self) -> String {
        match self {
            Self::Local(path) => path.to_string_lossy().into_owned(),
            Self::Remote(path) => path.to_string(),
            Self::Collaboration { project, path } => format!("{project}:{path}"),
        }
    }

    pub fn icon_path(&self) -> Cow<'_, Path> {
        match self {
            Self::Local(path) => Cow::Borrowed(path),
            Self::Remote(path) => Cow::Owned(path.to_path_buf()),
            Self::Collaboration { path, .. } => Cow::Owned(path.to_path_buf()),
        }
    }
}

impl From<PathBuf> for WorkspaceDocumentPath {
    fn from(path: PathBuf) -> Self {
        Self::Local(path)
    }
}

impl From<&Path> for WorkspaceDocumentPath {
    fn from(path: &Path) -> Self {
        Self::Local(path.to_path_buf())
    }
}

impl From<&crate::file_bound::DocumentLocation> for WorkspaceDocumentPath {
    fn from(location: &crate::file_bound::DocumentLocation) -> Self {
        match location {
            crate::file_bound::DocumentLocation::Local(path) => Self::Local(path.clone()),
            crate::file_bound::DocumentLocation::Remote(location) => {
                Self::Remote(location.path.clone())
            }
            crate::file_bound::DocumentLocation::Collaboration(location) => Self::Collaboration {
                project: location.project,
                path: location.path.clone(),
            },
        }
    }
}

impl PartialEq<PathBuf> for WorkspaceDocumentPath {
    fn eq(&self, other: &PathBuf) -> bool {
        self.local_path() == Some(other.as_path())
    }
}

impl PartialEq<WorkspaceDocumentPath> for PathBuf {
    fn eq(&self, other: &WorkspaceDocumentPath) -> bool {
        other == self
    }
}

#[derive(Clone, Default)]
pub enum WorkspaceBackend {
    #[default]
    Local,
    Remote(Arc<helix_remote::backend::RemoteWorkspaceClient>),
    Collaboration(helix_collab::GuestSessionHandle),
}

impl WorkspaceBackend {
    pub fn remote(&self) -> Option<&Arc<helix_remote::backend::RemoteWorkspaceClient>> {
        match self {
            Self::Remote(remote) => Some(remote),
            Self::Local | Self::Collaboration(_) => None,
        }
    }

    pub fn collaboration(&self) -> Option<&helix_collab::GuestSessionHandle> {
        match self {
            Self::Collaboration(session) => Some(session),
            Self::Local | Self::Remote(_) => None,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    pub fn is_collaboration(&self) -> bool {
        matches!(self, Self::Collaboration(_))
    }
}

impl std::fmt::Debug for WorkspaceBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => formatter.write_str("Local"),
            Self::Remote(remote) => formatter
                .debug_struct("Remote")
                .field("authority", &remote.authority())
                .field("root", &remote.workspace().root)
                .field("session", &remote.workspace().session)
                .finish(),
            Self::Collaboration(session) => formatter
                .debug_struct("Collaboration")
                .field("project", &session.project().id)
                .field("participant", &session.participant().id)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_document_paths_resolve_without_local_fallback() {
        let root = WorkspaceDocumentPath::Remote(WorkspacePath::root());

        assert_eq!(
            root.resolve(Path::new(r"src\main.rs")).unwrap(),
            WorkspaceDocumentPath::Remote(WorkspacePath::from_slash_path("src/main.rs").unwrap())
        );
        assert!(root.resolve(Path::new("../outside")).is_err());
        assert!(root.resolve(Path::new("C:/local/file.rs")).is_err());
    }

    #[test]
    fn local_document_paths_resolve_from_the_captured_root() {
        let root = WorkspaceDocumentPath::Local(PathBuf::from("workspace"));

        assert_eq!(
            root.resolve(Path::new("src/main.rs")).unwrap(),
            WorkspaceDocumentPath::Local(PathBuf::from("workspace/src/main.rs"))
        );
    }
}
