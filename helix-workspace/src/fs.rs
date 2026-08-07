use crate::WorkspacePath;
use std::{
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceFsErrorKind {
    NotFound,
    AlreadyExists,
    PermissionDenied,
    OutsideRoot,
    InvalidPath,
    Io,
    Internal,
}

#[derive(Debug)]
pub struct WorkspaceFsError {
    kind: WorkspaceFsErrorKind,
    path: Option<WorkspacePath>,
    message: String,
    source: Option<io::Error>,
}

impl WorkspaceFsError {
    pub fn kind(&self) -> WorkspaceFsErrorKind {
        self.kind
    }

    pub fn path(&self) -> Option<&WorkspacePath> {
        self.path.as_ref()
    }

    pub fn is_retryable(&self) -> bool {
        self.source.as_ref().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            )
        })
    }

    fn new(
        kind: WorkspaceFsErrorKind,
        path: Option<WorkspacePath>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            path,
            message: message.into(),
            source: None,
        }
    }

    fn io(error: io::Error, path: Option<WorkspacePath>) -> Self {
        let kind = match error.kind() {
            io::ErrorKind::NotFound => WorkspaceFsErrorKind::NotFound,
            io::ErrorKind::AlreadyExists => WorkspaceFsErrorKind::AlreadyExists,
            io::ErrorKind::PermissionDenied => WorkspaceFsErrorKind::PermissionDenied,
            _ => WorkspaceFsErrorKind::Io,
        };
        Self {
            kind,
            path,
            message: error.to_string(),
            source: Some(error),
        }
    }
}

impl fmt::Display for WorkspaceFsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for WorkspaceFsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }
}

/// A canonical filesystem root that rejects path traversal and symlink escapes.
#[derive(Debug, Clone)]
pub struct RootedWorkspace {
    root: PathBuf,
}

impl RootedWorkspace {
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, WorkspaceFsError> {
        let requested = root.into();
        let root = tokio::fs::canonicalize(&requested)
            .await
            .map_err(|error| WorkspaceFsError::io(error, None))?;
        let metadata = tokio::fs::metadata(&root)
            .await
            .map_err(|error| WorkspaceFsError::io(error, None))?;
        if !metadata.is_dir() {
            return Err(WorkspaceFsError::new(
                WorkspaceFsErrorKind::InvalidPath,
                None,
                "workspace root is not a directory",
            ));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn resolve_existing(
        &self,
        path: &WorkspacePath,
    ) -> Result<PathBuf, WorkspaceFsError> {
        ensure_public_path(path)?;
        let candidate = self.root.join(native_path(path)?);
        let resolved = tokio::fs::canonicalize(candidate)
            .await
            .map_err(|error| WorkspaceFsError::io(error, Some(path.clone())))?;
        self.ensure_inside(path, resolved)
    }

    pub async fn resolve_for_write(
        &self,
        path: &WorkspacePath,
        create_parents: bool,
    ) -> Result<PathBuf, WorkspaceFsError> {
        ensure_public_path(path)?;
        if path.is_root() {
            return Err(WorkspaceFsError::new(
                WorkspaceFsErrorKind::InvalidPath,
                Some(path.clone()),
                "workspace root is not a writable file path",
            ));
        }
        let candidate = self.root.join(native_path(path)?);
        match tokio::fs::canonicalize(&candidate).await {
            Ok(resolved) => return self.ensure_inside(path, resolved),
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(WorkspaceFsError::io(error, Some(path.clone())));
            }
            Err(_) => {}
        }

        let parent = candidate.parent().ok_or_else(|| {
            WorkspaceFsError::new(
                WorkspaceFsErrorKind::InvalidPath,
                Some(path.clone()),
                "workspace file path has no parent",
            )
        })?;
        let mut existing = parent;
        loop {
            match tokio::fs::canonicalize(existing).await {
                Ok(resolved) => {
                    self.ensure_inside(path, resolved)?;
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    existing = existing.parent().ok_or_else(|| {
                        WorkspaceFsError::new(
                            WorkspaceFsErrorKind::OutsideRoot,
                            Some(path.clone()),
                            "workspace file parent escapes the root",
                        )
                    })?;
                }
                Err(error) => {
                    return Err(WorkspaceFsError::io(error, Some(path.clone())));
                }
            }
        }
        if create_parents {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| WorkspaceFsError::io(error, Some(path.clone())))?;
            let resolved_parent = tokio::fs::canonicalize(parent)
                .await
                .map_err(|error| WorkspaceFsError::io(error, Some(path.clone())))?;
            self.ensure_inside(path, resolved_parent)?;
        } else if let Err(error) = tokio::fs::metadata(parent).await {
            return Err(WorkspaceFsError::io(error, Some(path.clone())));
        }
        Ok(candidate)
    }

    pub fn relative_path(&self, path: &Path) -> Result<WorkspacePath, WorkspaceFsError> {
        relative_workspace_path(&self.root, path)
    }

    fn ensure_inside(
        &self,
        path: &WorkspacePath,
        resolved: PathBuf,
    ) -> Result<PathBuf, WorkspaceFsError> {
        if resolved.starts_with(&self.root) {
            Ok(resolved)
        } else {
            Err(WorkspaceFsError::new(
                WorkspaceFsErrorKind::OutsideRoot,
                Some(path.clone()),
                "path resolves outside the workspace root",
            ))
        }
    }
}

pub fn relative_workspace_path(
    root: &Path,
    path: &Path,
) -> Result<WorkspacePath, WorkspaceFsError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        WorkspaceFsError::new(
            WorkspaceFsErrorKind::OutsideRoot,
            None,
            "path is outside the workspace root",
        )
    })?;
    WorkspacePath::from_native_path(relative).map_err(|error| {
        WorkspaceFsError::new(WorkspaceFsErrorKind::InvalidPath, None, error.to_string())
    })
}

fn ensure_public_path(path: &WorkspacePath) -> Result<(), WorkspaceFsError> {
    if is_internal_path(path) {
        Err(WorkspaceFsError::new(
            WorkspaceFsErrorKind::InvalidPath,
            Some(path.clone()),
            "the .double-helix workspace namespace is reserved",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn native_path(path: &WorkspacePath) -> Result<PathBuf, WorkspaceFsError> {
    path.to_native_path_buf().map_err(|error| {
        WorkspaceFsError::new(
            WorkspaceFsErrorKind::InvalidPath,
            Some(path.clone()),
            error.to_string(),
        )
    })
}

pub fn is_internal_path(path: &WorkspacePath) -> bool {
    path.segments()
        .first()
        .is_some_and(|segment| segment == ".double-helix")
}

pub async fn atomic_replace(source: PathBuf, destination: PathBuf) -> io::Result<()> {
    tokio::task::spawn_blocking(move || atomic_replace_blocking(&source, &destination))
        .await
        .map_err(io::Error::other)?
}

#[cfg(not(windows))]
pub(crate) fn atomic_replace_blocking(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
pub(crate) fn atomic_replace_blocking(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both pointers reference NUL-terminated buffers that remain alive
    // for the call. The source and destination are on the same filesystem.
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
pub async fn sync_parent_directory(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(directory) = tokio::fs::File::open(parent).await {
            let _ = directory.sync_all().await;
        }
    }
}

#[cfg(not(unix))]
pub async fn sync_parent_directory(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_symlink_escape_and_internal_state() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = workspace.path().join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(outside.path(), &link) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("failed to create test symlink: {error}");
        }

        let rooted = RootedWorkspace::open(workspace.path()).await.unwrap();
        let escape = WorkspacePath::from_slash_path("escape").unwrap();
        assert_eq!(
            rooted.resolve_existing(&escape).await.unwrap_err().kind(),
            WorkspaceFsErrorKind::OutsideRoot
        );
        let internal = WorkspacePath::from_slash_path(".double-helix/state").unwrap();
        assert_eq!(
            rooted
                .resolve_for_write(&internal, true)
                .await
                .unwrap_err()
                .kind(),
            WorkspaceFsErrorKind::InvalidPath
        );
    }
}
