use crate::{
    ContentId, DirectoryEntry, DirectoryOptions, ErrorCode, FileKind, FileMetadata, RemoteError,
    WorkspaceInfo, WorkspacePath,
};
use helix_workspace::{RootedWorkspace, WorkspaceFsError, WorkspaceFsErrorKind};
use ignore::WalkBuilder;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

#[derive(Debug)]
pub(crate) struct Workspace {
    files: RootedWorkspace,
    info: WorkspaceInfo,
}

impl Workspace {
    pub(crate) async fn open(root: String, session: crate::SessionId) -> Result<Self, RemoteError> {
        let requested = PathBuf::from(&root);
        if !requested.is_absolute() {
            return Err(RemoteError::new(
                ErrorCode::InvalidPath,
                "remote workspace root must be absolute",
            ));
        }
        let files = RootedWorkspace::open(requested)
            .await
            .map_err(workspace_error)?;
        let root = files.root();
        let display_name = root
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .unwrap_or(&root.to_string_lossy())
            .to_owned();
        let info = WorkspaceInfo {
            session,
            root: external_absolute_path(root),
            display_name,
            case_sensitive: !cfg!(windows),
        };
        Ok(Self { files, info })
    }

    pub(crate) fn root(&self) -> &Path {
        self.files.root()
    }

    pub(crate) fn info(&self) -> WorkspaceInfo {
        self.info.clone()
    }

    pub(crate) async fn resolve_existing(
        &self,
        path: &WorkspacePath,
    ) -> Result<PathBuf, RemoteError> {
        self.files
            .resolve_existing(path)
            .await
            .map_err(workspace_error)
    }

    pub(crate) async fn stat(
        &self,
        path: WorkspacePath,
    ) -> Result<Option<FileMetadata>, RemoteError> {
        let resolved = match self.resolve_existing(&path).await {
            Ok(path) => path,
            Err(error) if error.code == ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = tokio::fs::symlink_metadata(resolved)
            .await
            .map_err(|error| io_error(error, Some(path)))?;
        Ok(Some(file_metadata(&metadata)))
    }

    pub(crate) async fn resolve_for_write(
        &self,
        path: &WorkspacePath,
        create_parents: bool,
    ) -> Result<PathBuf, RemoteError> {
        self.files
            .resolve_for_write(path, create_parents)
            .await
            .map_err(workspace_error)
    }

    pub(crate) async fn read_dir(
        &self,
        path: WorkspacePath,
        options: DirectoryOptions,
    ) -> Result<Vec<DirectoryEntry>, RemoteError> {
        let root = self.files.root().to_path_buf();
        let resolved = self.resolve_existing(&path).await?;
        tokio::task::spawn_blocking(move || read_dir_blocking(&root, &resolved, options))
            .await
            .map_err(|error| {
                RemoteError::new(
                    ErrorCode::Internal,
                    format!("directory worker failed: {error}"),
                )
            })?
    }
}

pub(crate) fn external_absolute_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{path}");
        }
        if let Some(path) = path.strip_prefix(r"\\?\") {
            return path.to_owned();
        }
    }
    path.into_owned()
}

fn read_dir_blocking(
    root: &Path,
    directory: &Path,
    options: DirectoryOptions,
) -> Result<Vec<DirectoryEntry>, RemoteError> {
    let mut entries = read_directory_entries(root, directory, options)?;
    if options.flatten_dirs {
        for entry in &mut entries {
            if !matches!(entry.metadata.kind, FileKind::Directory) {
                continue;
            }
            let first_path = entry.path.clone();
            let mut nested_directory = root.join(native_path(&entry.path)?);
            let mut visited = std::collections::HashSet::new();
            while let Ok(target) = std::fs::canonicalize(&nested_directory) {
                if !target.starts_with(root) || !visited.insert(target) {
                    break;
                }
                let children = read_directory_entries(root, &nested_directory, options)?;
                let [child] = children.as_slice() else {
                    break;
                };
                if !matches!(child.metadata.kind, FileKind::Directory) {
                    break;
                }
                *entry = child.clone();
                nested_directory = root.join(native_path(&entry.path)?);
            }
            if entry.path != first_path {
                entry.name = entry
                    .path
                    .strip_prefix(&relative_path(root, directory)?)
                    .unwrap_or_else(|| entry.path.clone())
                    .to_string();
            }
        }
    }
    sort_directory_entries(&mut entries);
    Ok(entries)
}

fn read_directory_entries(
    root: &Path,
    directory: &Path,
    options: DirectoryOptions,
) -> Result<Vec<DirectoryEntry>, RemoteError> {
    let scan = options.scan;
    let mut builder = WalkBuilder::new(directory);
    builder
        .max_depth(Some(1))
        .hidden(scan.hidden)
        .parents(scan.parents)
        .ignore(scan.ignore)
        .git_ignore(scan.git_ignore)
        .git_global(scan.git_global)
        .git_exclude(scan.git_exclude)
        .follow_links(scan.follow_symlinks);

    let mut entries = Vec::new();
    for result in builder.build() {
        let entry = result.map_err(|error| {
            RemoteError::new(ErrorCode::Io, format!("failed to list directory: {error}"))
        })?;
        if entry.depth() == 0 || is_vcs_metadata(entry.file_name()) {
            continue;
        }
        let path = relative_path(root, entry.path())?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| {
                RemoteError::new(
                    ErrorCode::InvalidPath,
                    "non-UTF-8 filename is not supported",
                )
            })?
            .to_owned();
        let link_metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|error| io_error(error, Some(path.clone())))?;
        let metadata = if scan.follow_symlinks && link_metadata.file_type().is_symlink() {
            let target = std::fs::canonicalize(entry.path())
                .map_err(|error| io_error(error, Some(path.clone())))?;
            if target.starts_with(root) {
                if scan.deduplicate_symlinks {
                    continue;
                }
                std::fs::metadata(entry.path())
                    .map_err(|error| io_error(error, Some(path.clone())))?
            } else {
                link_metadata
            }
        } else {
            link_metadata
        };
        entries.push(DirectoryEntry {
            path,
            name,
            metadata: file_metadata(&metadata),
        });
    }
    Ok(entries)
}

fn sort_directory_entries(entries: &mut [DirectoryEntry]) {
    entries.sort_unstable_by(|left, right| {
        let left_file = matches!(left.metadata.kind, FileKind::File);
        let right_file = matches!(right.metadata.kind, FileKind::File);
        left_file
            .cmp(&right_file)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
}

pub(crate) fn relative_path(root: &Path, path: &Path) -> Result<WorkspacePath, RemoteError> {
    helix_workspace::relative_workspace_path(root, path).map_err(workspace_error)
}

fn native_path(path: &WorkspacePath) -> Result<PathBuf, RemoteError> {
    path.to_native_path_buf().map_err(|error| {
        RemoteError::new(ErrorCode::InvalidPath, error.to_string()).at(path.clone())
    })
}

pub(crate) fn file_metadata(metadata: &std::fs::Metadata) -> FileMetadata {
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_file() {
        FileKind::File
    } else if file_type.is_dir() {
        FileKind::Directory
    } else {
        FileKind::Other
    };
    let modified_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64);
    let content = matches!(kind, FileKind::File).then_some(ContentId {
        len: metadata.len(),
        modified_unix_nanos,
    });
    FileMetadata {
        kind,
        len: metadata.len(),
        modified_unix_nanos,
        readonly: metadata.permissions().readonly(),
        executable: is_executable(metadata),
        content,
    }
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn is_vcs_metadata(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | ".pijul" | ".jj" | ".hg" | ".svn" | ".double-helix")
    )
}

pub(crate) fn is_internal_path(path: &WorkspacePath) -> bool {
    helix_workspace::is_internal_path(path)
}

fn workspace_error(error: WorkspaceFsError) -> RemoteError {
    let code = match error.kind() {
        WorkspaceFsErrorKind::NotFound => ErrorCode::NotFound,
        WorkspaceFsErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
        WorkspaceFsErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        WorkspaceFsErrorKind::OutsideRoot => ErrorCode::WorkspaceOutsideRoot,
        WorkspaceFsErrorKind::InvalidPath => ErrorCode::InvalidPath,
        WorkspaceFsErrorKind::Io => ErrorCode::Io,
        WorkspaceFsErrorKind::Internal => ErrorCode::Internal,
    };
    RemoteError {
        code,
        message: error.to_string(),
        path: error.path().cloned(),
        retryable: error.is_retryable(),
    }
}

pub(crate) fn io_error(error: std::io::Error, path: Option<WorkspacePath>) -> RemoteError {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::NotFound,
        std::io::ErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
        std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => ErrorCode::Io,
        _ => ErrorCode::Io,
    };
    let retryable = matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    );
    RemoteError {
        code,
        message: error.to_string(),
        path,
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_workspace::ScanOptions;

    #[cfg(unix)]
    #[tokio::test]
    async fn lists_filenames_containing_backslashes_as_single_segments() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let name = r"\home\jonfo\src";
        let mut path = workspace_dir.path().to_owned();
        path.push(name);
        std::fs::write(path, b"remote file").unwrap();
        let workspace = Workspace::open(
            workspace_dir.path().to_string_lossy().into_owned(),
            crate::SessionId(5),
        )
        .await
        .unwrap();

        let entries = workspace
            .read_dir(
                WorkspacePath::root(),
                DirectoryOptions {
                    scan: ScanOptions {
                        hidden: false,
                        parents: false,
                        ignore: false,
                        git_ignore: false,
                        git_global: false,
                        git_exclude: false,
                        ..ScanOptions::default()
                    },
                    flatten_dirs: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, name);
        assert_eq!(entries[0].path.segments(), &[name]);
    }

    #[tokio::test]
    async fn rejects_symlink_escape() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = workspace_dir.path().join("escape");

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(outside.path(), &link) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("failed to create test symlink: {error}");
        }

        let workspace = Workspace::open(
            workspace_dir.path().to_string_lossy().into_owned(),
            crate::SessionId(1),
        )
        .await
        .unwrap();
        let error = workspace
            .resolve_existing(&WorkspacePath::from_slash_path("escape").unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::WorkspaceOutsideRoot);
    }

    #[tokio::test]
    async fn follows_only_contained_directory_symlinks_and_honors_deduplication() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace_dir.path().join("inside")).unwrap();
        std::fs::write(workspace_dir.path().join("inside/file.txt"), "inside").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        let internal_link = workspace_dir.path().join("inside-link");
        let external_link = workspace_dir.path().join("outside-link");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(workspace_dir.path().join("inside"), &internal_link)
                .unwrap();
            std::os::unix::fs::symlink(outside.path(), &external_link).unwrap();
        }
        #[cfg(windows)]
        {
            if let Err(error) = std::os::windows::fs::symlink_dir(
                workspace_dir.path().join("inside"),
                &internal_link,
            ) {
                if error.raw_os_error() == Some(1314) {
                    return;
                }
                panic!("failed to create test symlink: {error}");
            }
            std::os::windows::fs::symlink_dir(outside.path(), &external_link).unwrap();
        }

        let workspace = Workspace::open(
            workspace_dir.path().to_string_lossy().into_owned(),
            crate::SessionId(4),
        )
        .await
        .unwrap();
        let options = DirectoryOptions {
            scan: ScanOptions {
                hidden: false,
                follow_symlinks: true,
                deduplicate_symlinks: false,
                ..ScanOptions::default()
            },
            flatten_dirs: false,
        };
        let entries = workspace
            .read_dir(WorkspacePath::root(), options)
            .await
            .unwrap();
        let internal = entries
            .iter()
            .find(|entry| entry.name == "inside-link")
            .unwrap();
        let external = entries
            .iter()
            .find(|entry| entry.name == "outside-link")
            .unwrap();
        assert_eq!(internal.metadata.kind, FileKind::Directory);
        assert_eq!(external.metadata.kind, FileKind::Symlink);
        assert_eq!(
            workspace
                .read_dir(internal.path.clone(), options)
                .await
                .unwrap()[0]
                .name,
            "file.txt"
        );

        let deduplicated = workspace
            .read_dir(
                WorkspacePath::root(),
                DirectoryOptions {
                    scan: ScanOptions {
                        deduplicate_symlinks: true,
                        ..options.scan
                    },
                    ..options
                },
            )
            .await
            .unwrap();
        assert!(!deduplicated.iter().any(|entry| entry.name == "inside-link"));
    }

    #[tokio::test]
    async fn flattens_single_directory_chains_and_hides_internal_state() {
        let workspace_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace_dir.path().join("src/deep/nested")).unwrap();
        std::fs::write(
            workspace_dir.path().join("src/deep/nested/main.rs"),
            b"fn main() {}",
        )
        .unwrap();
        std::fs::create_dir_all(workspace_dir.path().join(".double-helix/transactions/1")).unwrap();

        let workspace = Workspace::open(
            workspace_dir.path().to_string_lossy().into_owned(),
            crate::SessionId(2),
        )
        .await
        .unwrap();
        let entries = workspace
            .read_dir(
                WorkspacePath::root(),
                DirectoryOptions {
                    scan: ScanOptions {
                        hidden: false,
                        parents: false,
                        ignore: false,
                        git_ignore: false,
                        git_global: false,
                        git_exclude: false,
                        ..ScanOptions::default()
                    },
                    flatten_dirs: true,
                },
            )
            .await
            .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path.to_string(), "src/deep/nested");
        assert_eq!(entries[0].name, "src/deep/nested");
    }

    #[tokio::test]
    async fn rejects_internal_workspace_paths() {
        let workspace_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace_dir.path().join(".double-helix")).unwrap();
        let workspace = Workspace::open(
            workspace_dir.path().to_string_lossy().into_owned(),
            crate::SessionId(3),
        )
        .await
        .unwrap();
        let internal = WorkspacePath::from_slash_path(".double-helix").unwrap();

        assert_eq!(
            workspace
                .resolve_existing(&internal)
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidPath
        );
    }
}
