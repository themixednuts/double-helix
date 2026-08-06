use std::path::{Path, PathBuf};

use helix_view::modal_text::ModalTextSelection as LabelSelection;

use super::ExplorerPath;

pub(super) fn selected_cursor(selection: usize) -> u32 {
    u32::try_from(selection).unwrap_or(u32::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LabelEditRange {
    pub(super) start: usize,
    pub(super) end: usize,
}

impl LabelEditRange {
    pub(super) fn from_selection(selection: LabelSelection, label: &str) -> Option<Self> {
        let span = selection.edit_span(label)?;
        Some(Self {
            start: span.start,
            end: span.end,
        })
    }

    pub(super) const fn is_whole(self, label_len: usize) -> bool {
        self.start == 0 && self.end == label_len
    }

    pub(super) fn selected_text(self, label: &str) -> String {
        let start = char_to_byte(label, self.start);
        let end = char_to_byte(label, self.end);
        label[start..end].to_string()
    }

    pub(super) fn remove_from(self, label: &str) -> String {
        let start = char_to_byte(label, self.start);
        let end = char_to_byte(label, self.end);
        let mut edited = String::with_capacity(label.len().saturating_sub(end - start));
        edited.push_str(&label[..start]);
        edited.push_str(&label[end..]);
        edited
    }
}

#[derive(Debug)]
pub(super) enum LabelRenameError {
    Empty,
    PathSeparator,
    DotSegment,
    MissingParent,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum EntryPathError {
    Empty,
    Absolute,
    Traversal,
}

impl std::fmt::Display for EntryPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("path cannot be empty"),
            Self::Absolute => f.write_str("path must be relative to the selected directory"),
            Self::Traversal => f.write_str("path cannot contain . or .. segments"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct EntryPath {
    pub(super) relative: PathBuf,
    pub(super) is_dir: bool,
}

pub(super) fn parse_entry_path(input: &str) -> Result<EntryPath, EntryPathError> {
    let normalized = input.replace('\\', "/");
    let is_dir = normalized.ends_with('/');
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty() {
        return Err(EntryPathError::Empty);
    }

    let bytes = normalized.as_bytes();
    if normalized.starts_with('/') || bytes.get(1).is_some_and(|separator| *separator == b':') {
        return Err(EntryPathError::Absolute);
    }

    let mut relative = PathBuf::new();
    for component in Path::new(normalized).components() {
        match component {
            std::path::Component::Normal(segment) => relative.push(segment),
            std::path::Component::CurDir | std::path::Component::ParentDir => {
                return Err(EntryPathError::Traversal)
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(EntryPathError::Absolute)
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(EntryPathError::Empty);
    }
    Ok(EntryPath { relative, is_dir })
}

impl std::fmt::Display for LabelRenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("file name cannot be empty"),
            Self::PathSeparator => f.write_str("file name cannot contain a path separator"),
            Self::DotSegment => f.write_str("file name cannot be . or .."),
            Self::MissingParent => f.write_str("selected path has no parent directory"),
        }
    }
}

fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

pub(super) fn sibling_path_with_label(
    source: &Path,
    label: &str,
) -> Result<PathBuf, LabelRenameError> {
    validate_label(label)?;
    let parent = source.parent().ok_or(LabelRenameError::MissingParent)?;
    Ok(parent.join(label))
}

pub(super) fn validate_label(label: &str) -> Result<(), LabelRenameError> {
    if label.is_empty() {
        return Err(LabelRenameError::Empty);
    }
    if label.contains('/') || label.contains('\\') {
        return Err(LabelRenameError::PathSeparator);
    }
    if matches!(label, "." | "..") {
        return Err(LabelRenameError::DotSegment);
    }
    Ok(())
}

pub(super) trait DisplayExplorerPath {
    fn display_explorer_path(&self) -> String;
    fn explorer_file_name(&self) -> Option<String>;
    fn relative_explorer_path(&self, base: &Self) -> Option<String>;
}

impl DisplayExplorerPath for Path {
    fn display_explorer_path(&self) -> String {
        helix_stdx::path::display_path(self).into_owned()
    }

    fn explorer_file_name(&self) -> Option<String> {
        self.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }

    fn relative_explorer_path(&self, base: &Self) -> Option<String> {
        let relative = self.strip_prefix(base).ok()?;
        (!relative.as_os_str().is_empty()).then(|| relative.display_explorer_path())
    }
}

impl DisplayExplorerPath for PathBuf {
    fn display_explorer_path(&self) -> String {
        self.as_path().display_explorer_path()
    }

    fn explorer_file_name(&self) -> Option<String> {
        self.as_path().explorer_file_name()
    }

    fn relative_explorer_path(&self, base: &Self) -> Option<String> {
        self.as_path().relative_explorer_path(base)
    }
}

impl DisplayExplorerPath for ExplorerPath {
    fn display_explorer_path(&self) -> String {
        match self {
            ExplorerPath::Local(path) => path.display_explorer_path(),
            ExplorerPath::Remote(_) | ExplorerPath::Collaboration { .. } => self.display(),
        }
    }

    fn explorer_file_name(&self) -> Option<String> {
        self.file_name().map(|name| name.into_owned())
    }

    fn relative_explorer_path(&self, base: &Self) -> Option<String> {
        let relative = self.relative_to(base)?;
        (!relative.is_root()).then(|| relative.display_explorer_path())
    }
}

pub(super) fn display_name<P>(path: &P) -> String
where
    P: DisplayExplorerPath + ?Sized,
{
    path.explorer_file_name()
        .unwrap_or_else(|| path.display_explorer_path())
}

pub(super) fn display_path<P>(path: &P) -> String
where
    P: DisplayExplorerPath + ?Sized,
{
    path.display_explorer_path()
}

pub(super) fn relative_display<P>(base: &P, path: &P) -> String
where
    P: DisplayExplorerPath + ?Sized,
{
    path.relative_explorer_path(base)
        .unwrap_or_else(|| display_name(path))
}
