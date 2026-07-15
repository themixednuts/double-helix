use std::path::{Path, PathBuf};

use helix_view::modal_text::ModalTextSelection as LabelSelection;

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
    if label.is_empty() {
        return Err(LabelRenameError::Empty);
    }
    if label.contains('/') || label.contains('\\') {
        return Err(LabelRenameError::PathSeparator);
    }
    if matches!(label, "." | "..") {
        return Err(LabelRenameError::DotSegment);
    }
    let parent = source.parent().ok_or(LabelRenameError::MissingParent)?;
    Ok(parent.join(label))
}

pub(super) fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| display_path(path))
}

pub(super) fn display_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

pub(super) fn relative_display(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map(display_path)
        .unwrap_or_else(|| display_name(path))
}
