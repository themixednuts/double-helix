use std::{
    collections::BTreeSet,
    fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

use helix_stdx::path::canonicalize;

use super::Editor;

#[derive(Default)]
pub(crate) struct FileOperationJournal {
    undo: Option<FileOperationRecord>,
    redo: Option<FileOperationRecord>,
}

#[derive(Clone, Debug)]
struct FileOperationRecord {
    kind: FileOperationKind,
    before: Box<[PathImage]>,
    after: Box<[PathImage]>,
}

#[derive(Clone, Debug)]
enum FileOperationKind {
    Create {
        path: PathBuf,
        is_dir: bool,
    },
    Copy {
        source: PathBuf,
        destination: PathBuf,
    },
    Move {
        source: PathBuf,
        destination: PathBuf,
    },
    Delete {
        path: PathBuf,
        mode: DeleteMode,
    },
}

#[derive(Clone, Copy, Debug)]
enum DeleteMode {
    Trash,
    Permanent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PathImage {
    path: PathBuf,
    state: PathState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PathState {
    Missing,
    Present(BackupSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BackupSnapshot {
    entries: Box<[BackupEntry]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BackupEntry {
    Directory {
        relative_path: PathBuf,
    },
    File {
        relative_path: PathBuf,
        compressed_data: Box<[u8]>,
        readonly: bool,
    },
}

impl FileOperationJournal {
    fn commit(&mut self, record: FileOperationRecord) {
        if record.before == record.after {
            return;
        }
        self.undo = Some(record);
        self.redo = None;
    }
}

impl FileOperationRecord {
    fn new(kind: FileOperationKind, before: Box<[PathImage]>, after: Box<[PathImage]>) -> Self {
        Self {
            kind,
            before,
            after,
        }
    }

    fn undo(&self) -> io::Result<()> {
        self.validate(&self.after)?;
        apply_images(&self.before)
    }

    fn redo(&self) -> io::Result<()> {
        self.validate(&self.before)?;
        apply_images(&self.after)
    }

    fn validate(&self, expected: &[PathImage]) -> io::Result<()> {
        for image in expected {
            if !image.state.matches_path(&image.path)? {
                return Err(io::Error::other(format!(
                    "refusing to modify {}; it changed after the file operation",
                    image.path.display()
                )));
            }
        }
        Ok(())
    }

    fn paths(&self) -> impl Iterator<Item = &Path> {
        self.before
            .iter()
            .chain(self.after.iter())
            .map(|image| image.path.as_path())
    }

    fn undo_message(&self) -> String {
        match &self.kind {
            FileOperationKind::Create { path, is_dir } => format!(
                "Undid create {}: {}",
                if *is_dir { "directory" } else { "file" },
                path.display()
            ),
            FileOperationKind::Copy {
                source,
                destination,
            } => format!(
                "Undid copy {} -> {}",
                source.display(),
                destination.display()
            ),
            FileOperationKind::Move {
                source,
                destination,
            } => format!(
                "Undid move {} -> {}",
                source.display(),
                destination.display()
            ),
            FileOperationKind::Delete { path, mode } => {
                format!("Undid {}: {}", mode.label(), path.display())
            }
        }
    }

    fn redo_message(&self) -> String {
        match &self.kind {
            FileOperationKind::Create { path, is_dir } => format!(
                "Redid create {}: {}",
                if *is_dir { "directory" } else { "file" },
                path.display()
            ),
            FileOperationKind::Copy {
                source,
                destination,
            } => format!(
                "Redid copy {} -> {}",
                source.display(),
                destination.display()
            ),
            FileOperationKind::Move {
                source,
                destination,
            } => format!(
                "Redid move {} -> {}",
                source.display(),
                destination.display()
            ),
            FileOperationKind::Delete { path, mode } => {
                format!("Redid {}: {}", mode.label(), path.display())
            }
        }
    }
}

impl DeleteMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Trash => "trash",
            Self::Permanent => "delete",
        }
    }
}

impl PathImage {
    fn capture(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = canonicalize(path.as_ref());
        Ok(Self {
            state: PathState::capture(&path)?,
            path,
        })
    }
}

impl PathState {
    fn capture(path: &Path) -> io::Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "file operation history does not support symlinks: {}",
                    path.display()
                ),
            )),
            Ok(metadata) if metadata.is_file() || metadata.is_dir() => {
                Ok(Self::Present(BackupSnapshot::capture(path)?))
            }
            Ok(_) => Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "file operation history does not support special files: {}",
                    path.display()
                ),
            )),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(Self::Missing),
            Err(err) => Err(err),
        }
    }

    fn matches_path(&self, path: &Path) -> io::Result<bool> {
        Ok(Self::capture(path)? == *self)
    }
}

impl BackupSnapshot {
    fn capture(path: &Path) -> io::Result<Self> {
        let mut entries = Vec::new();
        capture_entry(path, path, &mut entries)?;
        entries.sort_by(|left, right| entry_relative_path(left).cmp(entry_relative_path(right)));
        Ok(Self {
            entries: entries.into_boxed_slice(),
        })
    }

    fn restore_to(&self, path: &Path) -> io::Result<()> {
        remove_path(path)?;

        let mut directories: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                BackupEntry::Directory { relative_path } => Some(relative_path),
                BackupEntry::File { .. } => None,
            })
            .collect();
        directories.sort_by_key(|path| path.components().count());
        for relative_path in directories {
            fs::create_dir_all(snapshot_target(path, relative_path))?;
        }

        let mut files: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                BackupEntry::File {
                    relative_path,
                    compressed_data,
                    readonly,
                } => Some((relative_path, compressed_data, *readonly)),
                BackupEntry::Directory { .. } => None,
            })
            .collect();
        files.sort_by_key(|(relative_path, ..)| *relative_path);
        for (relative_path, compressed_data, readonly) in files {
            let target = snapshot_target(path, relative_path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let data = zstd::stream::decode_all(&compressed_data[..])?;
            fs::write(&target, data)?;
            set_readonly(&target, readonly)?;
        }

        Ok(())
    }
}

fn capture_images(paths: impl IntoIterator<Item = PathBuf>) -> io::Result<Box<[PathImage]>> {
    let mut seen = BTreeSet::new();
    let mut images = Vec::new();
    for path in paths {
        let path = canonicalize(&path);
        if seen.insert(path.clone()) {
            images.push(PathImage::capture(path)?);
        }
    }
    Ok(images.into_boxed_slice())
}

fn capture_entry(root: &Path, path: &Path, entries: &mut Vec<BackupEntry>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            format!(
                "file operation history does not support symlinks: {}",
                path.display()
            ),
        ));
    }

    let relative_path = path
        .strip_prefix(root)
        .unwrap_or_else(|_| Path::new(""))
        .to_path_buf();
    if metadata.is_dir() {
        entries.push(BackupEntry::Directory { relative_path });
        let mut children = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(|entry| entry.path());
        for child in children {
            capture_entry(root, &child.path(), entries)?;
        }
        return Ok(());
    }

    if metadata.is_file() {
        let data = fs::read(path)?;
        let compressed_data = zstd::stream::encode_all(&data[..], 0)?.into_boxed_slice();
        entries.push(BackupEntry::File {
            relative_path,
            compressed_data,
            readonly: metadata.permissions().readonly(),
        });
        return Ok(());
    }

    Err(io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "file operation history does not support special files: {}",
            path.display()
        ),
    ))
}

fn entry_relative_path(entry: &BackupEntry) -> &Path {
    match entry {
        BackupEntry::Directory { relative_path } | BackupEntry::File { relative_path, .. } => {
            relative_path
        }
    }
}

fn snapshot_target(root: &Path, relative_path: &Path) -> PathBuf {
    if relative_path.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative_path)
    }
}

fn apply_images(images: &[PathImage]) -> io::Result<()> {
    let mut missing: Vec<_> = images
        .iter()
        .filter(|image| matches!(image.state, PathState::Missing))
        .collect();
    missing.sort_by_key(|image| std::cmp::Reverse(image.path.components().count()));
    for image in missing {
        remove_path(&image.path)?;
    }

    let mut present: Vec<_> = images
        .iter()
        .filter_map(|image| match &image.state {
            PathState::Present(snapshot) => Some((&image.path, snapshot)),
            PathState::Missing => None,
        })
        .collect();
    present.sort_by_key(|(path, _)| path.components().count());
    for (path, snapshot) in present {
        snapshot.restore_to(path)?;
    }

    Ok(())
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            make_writable_recursive(path)?;
            fs::remove_dir_all(path)
        }
        Ok(metadata) if metadata.is_file() => {
            set_readonly(path, false)?;
            fs::remove_file(path)
        }
        Ok(_) => Err(io::Error::new(
            ErrorKind::Unsupported,
            format!(
                "file operation history does not support special files: {}",
                path.display()
            ),
        )),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn make_writable_recursive(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return set_readonly(path, false);
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    for child in fs::read_dir(path)? {
        make_writable_recursive(&child?.path())?;
    }
    set_readonly(path, false)
}

#[cfg(windows)]
fn set_readonly(path: &Path, readonly: bool) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(readonly);
    fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn set_readonly(path: &Path, readonly: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    if readonly {
        permissions.set_mode(mode & !0o222);
    } else {
        permissions.set_mode(mode | 0o200);
    }
    fs::set_permissions(path, permissions)
}

impl Editor {
    pub fn create_path_with_history(&mut self, path: &Path, is_dir: bool) -> io::Result<()> {
        let path = canonicalize(path);
        let before = capture_images([path.clone()])?;
        self.create_path(&path, is_dir)?;
        let after = capture_images([path.clone()])?;
        self.file_operations.commit(FileOperationRecord::new(
            FileOperationKind::Create { path, is_dir },
            before,
            after,
        ));
        Ok(())
    }

    pub fn copy_path_with_history(&mut self, old_path: &Path, new_path: &Path) -> io::Result<u64> {
        let source = canonicalize(old_path);
        let destination = canonicalize(new_path);
        let before = capture_images([destination.clone()])?;
        let bytes = self.copy_path(&source, &destination)?;
        let after = capture_images([destination.clone()])?;
        self.file_operations.commit(FileOperationRecord::new(
            FileOperationKind::Copy {
                source,
                destination,
            },
            before,
            after,
        ));
        Ok(bytes)
    }

    pub fn move_path_with_history(&mut self, old_path: &Path, new_path: &Path) -> io::Result<()> {
        let source = canonicalize(old_path);
        let destination = canonicalize(new_path);
        if source == destination {
            return Ok(());
        }

        let before = capture_images([source.clone(), destination.clone()])?;
        self.move_path(&source, &destination)?;
        let after = capture_images([source.clone(), destination.clone()])?;
        self.file_operations.commit(FileOperationRecord::new(
            FileOperationKind::Move {
                source,
                destination,
            },
            before,
            after,
        ));
        Ok(())
    }

    pub fn trash_path_with_history(&mut self, path: &Path) -> io::Result<()> {
        let path = canonicalize(path);
        let before = capture_images([path.clone()])?;
        self.trash_path(&path)?;
        let after = capture_images([path.clone()])?;
        self.file_operations.commit(FileOperationRecord::new(
            FileOperationKind::Delete {
                path,
                mode: DeleteMode::Trash,
            },
            before,
            after,
        ));
        Ok(())
    }

    pub fn delete_path_permanently_with_history(&mut self, path: &Path) -> io::Result<()> {
        let path = canonicalize(path);
        let before = capture_images([path.clone()])?;
        self.delete_path(&path)?;
        let after = capture_images([path.clone()])?;
        self.file_operations.commit(FileOperationRecord::new(
            FileOperationKind::Delete {
                path,
                mode: DeleteMode::Permanent,
            },
            before,
            after,
        ));
        Ok(())
    }

    pub fn undo_file_operation(&mut self) -> io::Result<Option<String>> {
        let Some(record) = self.file_operations.undo.take() else {
            return Ok(None);
        };

        match record.undo() {
            Ok(()) => {
                let message = record.undo_message();
                self.notify_file_operation_paths(record.paths());
                self.file_operations.redo = Some(record);
                Ok(Some(message))
            }
            Err(err) => {
                self.file_operations.undo = Some(record);
                Err(err)
            }
        }
    }

    pub fn redo_file_operation(&mut self) -> io::Result<Option<String>> {
        let Some(record) = self.file_operations.redo.take() else {
            return Ok(None);
        };

        match record.redo() {
            Ok(()) => {
                let message = record.redo_message();
                self.notify_file_operation_paths(record.paths());
                self.file_operations.undo = Some(record);
                Ok(Some(message))
            }
            Err(err) => {
                self.file_operations.redo = Some(record);
                Err(err)
            }
        }
    }

    fn trash_path(&mut self, path: &Path) -> io::Result<()> {
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

        trash::delete(&path).map_err(|err| io::Error::other(err.to_string()))?;

        for ls in self.language_servers.iter_clients() {
            if !ls.is_initialized() {
                continue;
            }
            ls.did_delete(&path, is_dir);
        }
        self.language_servers.file_event_handler.file_changed(path);
        Ok(())
    }

    fn notify_file_operation_paths<'a>(&mut self, paths: impl Iterator<Item = &'a Path>) {
        let mut seen = BTreeSet::new();
        for path in paths {
            if seen.insert(path.to_path_buf()) {
                self.language_servers
                    .file_event_handler
                    .file_changed(path.to_path_buf());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_snapshot_restores_file_contents() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file.txt");
        fs::write(&file, "before").unwrap();
        let snapshot = BackupSnapshot::capture(&file).unwrap();

        fs::write(&file, "after").unwrap();
        snapshot.restore_to(&file).unwrap();

        assert_eq!(fs::read_to_string(file).unwrap(), "before");
    }

    #[test]
    fn backup_snapshot_restores_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        let snapshot = BackupSnapshot::capture(&root).unwrap();

        fs::remove_dir_all(&root).unwrap();
        snapshot.restore_to(&root).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("src").join("main.rs")).unwrap(),
            "fn main() {}"
        );
    }

    #[test]
    fn operation_record_refuses_to_undo_changed_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file.txt");
        fs::write(&file, "before").unwrap();
        let before = capture_images([file.clone()]).unwrap();
        fs::write(&file, "after").unwrap();
        let after = capture_images([file.clone()]).unwrap();
        let record = FileOperationRecord::new(
            FileOperationKind::Create {
                path: file.clone(),
                is_dir: false,
            },
            before,
            after,
        );

        fs::write(&file, "user edit").unwrap();

        assert!(record.undo().is_err());
        assert_eq!(fs::read_to_string(file).unwrap(), "user edit");
    }

    #[test]
    fn operation_record_round_trips_one_file_change() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file.txt");
        let before = capture_images([file.clone()]).unwrap();
        fs::write(&file, "after").unwrap();
        let after = capture_images([file.clone()]).unwrap();
        let record = FileOperationRecord::new(
            FileOperationKind::Create {
                path: file.clone(),
                is_dir: false,
            },
            before,
            after,
        );

        record.undo().unwrap();
        assert!(!file.exists());

        record.redo().unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "after");
    }

    #[test]
    fn journal_keeps_only_latest_undo_and_clears_redo() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.txt");
        let second = temp.path().join("second.txt");

        let first_before = capture_images([first.clone()]).unwrap();
        fs::write(&first, "first").unwrap();
        let first_after = capture_images([first.clone()]).unwrap();
        let first_record = FileOperationRecord::new(
            FileOperationKind::Create {
                path: first,
                is_dir: false,
            },
            first_before,
            first_after,
        );

        let second_before = capture_images([second.clone()]).unwrap();
        fs::write(&second, "second").unwrap();
        let second_after = capture_images([second.clone()]).unwrap();
        let second_record = FileOperationRecord::new(
            FileOperationKind::Create {
                path: second.clone(),
                is_dir: false,
            },
            second_before,
            second_after,
        );

        let mut journal = FileOperationJournal::default();
        journal.commit(first_record.clone());
        journal.redo = Some(first_record);
        journal.commit(second_record);

        assert!(journal.redo.is_none());
        assert!(matches!(
            journal.undo.as_ref().map(|record| &record.kind),
            Some(FileOperationKind::Create { path, .. }) if path == &second
        ));
    }
}
