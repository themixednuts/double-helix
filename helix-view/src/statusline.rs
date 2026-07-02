use std::borrow::Cow;
use std::path::PathBuf;

use crate::document::{Mode, DEFAULT_LANGUAGE_NAME, SCRATCH_BUFFER_NAME};
use crate::editor::WorkspaceDiagnosticCounts;
use crate::traits::{Selectable, TextContent};
use crate::Document;
use crate::ViewId;
use helix_core::diagnostic::Severity;
use helix_core::indent::IndentStyle;
use helix_core::line_ending::LineEnding;
use helix_core::{coords_at_pos, Position};

/// Cursor-derived statusline data for a single viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct CursorStatus {
    pub char_idx: usize,
    pub position: Position,
    pub total_lines: usize,
}

/// Selection-derived statusline data for a single viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct SelectionStatus {
    pub count: usize,
    pub primary_index: usize,
    pub primary_length: usize,
}

/// File/document metadata rendered in the statusline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentStatus<'a> {
    pub file_path: Option<PathBuf>,
    pub file_base_name: Cow<'a, str>,
    pub file_name: Cow<'a, str>,
    pub file_absolute_path: Cow<'a, str>,
    pub modified: bool,
    pub readonly: bool,
    pub encoding_name: Option<&'static str>,
    pub line_ending: LineEnding,
    pub indent_style: IndentStyle,
    pub language_name: Cow<'a, str>,
    pub version_control_head: Option<Cow<'a, str>>,
}

/// Diagnostic counts displayed by statusline elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct DiagnosticCounts {
    pub hints: usize,
    pub info: usize,
    pub warnings: usize,
    pub errors: usize,
}

/// Modal/editor-global data rendered in the statusline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModalStatus {
    pub focused: bool,
    pub mode: Mode,
    pub selected_register: Option<char>,
}

impl Default for ModalStatus {
    fn default() -> Self {
        Self {
            focused: false,
            mode: Mode::Normal,
            selected_register: None,
        }
    }
}

/// Fully typed statusline snapshot collected before rendering.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatuslineSnapshot<'a> {
    pub modal: ModalStatus,
    pub cursor: CursorStatus,
    pub selection: SelectionStatus,
    pub document: DocumentStatus<'a>,
    pub diagnostics: DiagnosticCounts,
    pub workspace_diagnostics: WorkspaceDiagnosticCounts,
    pub spinner_frame: Cow<'a, str>,
    pub lsp_progress: Option<u8>,
    pub current_working_directory: Cow<'a, str>,
    pub function_name: Option<String>,
    /// Names of the language servers attached to the focused
    /// document, in attach order. Empty when the buffer has no LSP
    /// (binary, scratch, unsupported language) so renderers can
    /// skip the element entirely without reserving dead space.
    pub lsp_server_names: Vec<String>,
}

/// Produces cursor-oriented statusline data.
pub trait CursorStatusProvider: TextContent + Selectable {
    fn cursor_status(&self, view_id: ViewId) -> CursorStatus {
        let text = self.text().slice(..);
        let selection = self.selection(view_id);
        let char_idx = selection.primary().cursor(text);

        CursorStatus {
            char_idx,
            position: coords_at_pos(text, char_idx),
            total_lines: text.len_lines(),
        }
    }
}

impl<T> CursorStatusProvider for T where T: TextContent + Selectable {}

/// Produces selection-oriented statusline data.
pub trait SelectionStatusProvider: Selectable {
    fn selection_status(&self, view_id: ViewId) -> SelectionStatus {
        let selection = self.selection(view_id);

        SelectionStatus {
            count: selection.len(),
            primary_index: selection.primary_index(),
            primary_length: selection.primary().len(),
        }
    }
}

impl<T> SelectionStatusProvider for T where T: Selectable {}

/// Produces file/document metadata for the statusline.
pub trait DocumentStatusProvider {
    fn document_status(&self) -> DocumentStatus<'_>;
}

impl DocumentStatusProvider for Document {
    fn document_status(&self) -> DocumentStatus<'_> {
        let relative_path = self
            .relative_path()
            .as_ref()
            .map(|path| Cow::Owned(path.to_string_lossy().into_owned()))
            .unwrap_or_else(|| Cow::Borrowed(SCRATCH_BUFFER_NAME));
        let absolute_path = self
            .path()
            .as_ref()
            .map(|path| Cow::Owned(path.to_string_lossy().into_owned()))
            .unwrap_or_else(|| Cow::Borrowed(SCRATCH_BUFFER_NAME));
        let file_base_name = self
            .relative_path()
            .as_ref()
            .and_then(|path| {
                path.file_name()
                    .map(|name| Cow::Owned(name.to_string_lossy().into_owned()))
            })
            .unwrap_or(Cow::Borrowed(SCRATCH_BUFFER_NAME));
        let encoding_name =
            (self.encoding() != helix_core::encoding::UTF_8).then(|| self.encoding().name());

        DocumentStatus {
            file_path: self.path().cloned(),
            file_base_name,
            file_name: relative_path,
            file_absolute_path: absolute_path,
            modified: self.is_modified(),
            readonly: self.readonly(),
            encoding_name,
            line_ending: self.line_ending(),
            indent_style: self.indent_style(),
            language_name: Cow::Borrowed(self.language_name().unwrap_or(DEFAULT_LANGUAGE_NAME)),
            version_control_head: self
                .version_control_head()
                .as_deref()
                .map(|head| Cow::Owned(head.to_string())),
        }
    }
}

impl DocumentStatus<'_> {
    pub fn into_owned(self) -> DocumentStatus<'static> {
        DocumentStatus {
            file_path: self.file_path,
            file_base_name: Cow::Owned(self.file_base_name.into_owned()),
            file_name: Cow::Owned(self.file_name.into_owned()),
            file_absolute_path: Cow::Owned(self.file_absolute_path.into_owned()),
            modified: self.modified,
            readonly: self.readonly,
            encoding_name: self.encoding_name,
            line_ending: self.line_ending,
            indent_style: self.indent_style,
            language_name: Cow::Owned(self.language_name.into_owned()),
            version_control_head: self
                .version_control_head
                .map(|head| Cow::Owned(head.into_owned())),
        }
    }
}

/// Produces per-document diagnostic counts for the statusline.
pub trait DiagnosticStatusProvider {
    fn diagnostic_counts(&self) -> DiagnosticCounts;
}

impl DiagnosticStatusProvider for Document {
    fn diagnostic_counts(&self) -> DiagnosticCounts {
        self.diagnostics()
            .iter()
            .fold(DiagnosticCounts::default(), |mut counts, diagnostic| {
                match diagnostic.severity {
                    Some(Severity::Hint) | None => counts.hints += 1,
                    Some(Severity::Info) => counts.info += 1,
                    Some(Severity::Warning) => counts.warnings += 1,
                    Some(Severity::Error) => counts.errors += 1,
                }
                counts
            })
    }
}

impl StatuslineSnapshot<'_> {
    pub fn into_owned(self) -> StatuslineSnapshot<'static> {
        StatuslineSnapshot {
            modal: self.modal,
            cursor: self.cursor,
            selection: self.selection,
            document: self.document.into_owned(),
            diagnostics: self.diagnostics,
            workspace_diagnostics: self.workspace_diagnostics,
            spinner_frame: Cow::Owned(self.spinner_frame.into_owned()),
            lsp_progress: self.lsp_progress,
            current_working_directory: Cow::Owned(self.current_working_directory.into_owned()),
            function_name: self.function_name,
            lsp_server_names: self.lsp_server_names,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::{Range, Rope, Selection};

    struct FakeStatuslineSource {
        text: Rope,
        selection: Selection,
    }

    impl TextContent for FakeStatuslineSource {
        fn text(&self) -> &Rope {
            &self.text
        }
    }

    impl Selectable for FakeStatuslineSource {
        fn selection(&self, _view_id: ViewId) -> &Selection {
            &self.selection
        }

        fn set_selection(&mut self, _view_id: ViewId, selection: Selection) {
            self.selection = selection;
        }
    }

    #[test]
    fn cursor_and_selection_status_track_primary_range() {
        let source = FakeStatuslineSource {
            text: Rope::from("alpha\nbeta\n"),
            selection: Selection::new(vec![Range::new(0, 5), Range::new(6, 8)].into(), 1),
        };

        let cursor = source.cursor_status(ViewId::default());
        let selection = source.selection_status(ViewId::default());

        assert_eq!(cursor.char_idx, 7);
        assert_eq!(cursor.position, Position::new(1, 1));
        assert_eq!(cursor.total_lines, 3);
        assert_eq!(selection.count, 2);
        assert_eq!(selection.primary_index, 1);
        assert_eq!(selection.primary_length, 2);
    }
}
