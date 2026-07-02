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

fn path_is_missing(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err),
    }
}

pub(super) fn unique_destination(
    destination_dir: &Path,
    source: &Path,
) -> std::io::Result<PathBuf> {
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("untitled");
    let candidate = destination_dir.join(file_name);
    if path_is_missing(&candidate)? {
        return Ok(candidate);
    }

    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(file_name);
    let extension = source.extension().and_then(|extension| extension.to_str());
    for index in 1.. {
        let suffix = if index == 1 {
            String::from(" copy")
        } else {
            format!(" copy {index}")
        };
        let name = match extension {
            Some(extension) if !extension.is_empty() => format!("{stem}{suffix}.{extension}"),
            _ => format!("{stem}{suffix}"),
        };
        let candidate = destination_dir.join(name);
        if path_is_missing(&candidate)? {
            return Ok(candidate);
        }
    }

    unreachable!("unbounded copy suffix search should always find a destination")
}

pub(super) fn relative_display(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map(display_path)
        .unwrap_or_else(|| display_name(path))
}
