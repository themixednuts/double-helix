use std::path::{Path, PathBuf};

use helix_core::movement::Movement as CoreMovement;
use helix_view::{
    document::Mode,
    modal_text::{
        ModalTextMotion as LabelMotion, ModalTextObject as LabelTextObject,
        ModalTextSelection as LabelSelection,
    },
    traits::Modal,
    Editor,
};

use crate::{
    compositor::Context,
    runtime::{
        ui::command::{FileExplorerCommand, ModifiedBufferCheck},
        UiCommand,
    },
};

use super::{
    input::{ExplorerFileOperation, ExplorerOperator, ExplorerPastePlacement},
    model::ExplorerRow,
    path_ops::{parse_entry_path, selected_cursor, sibling_path_with_label},
    windows_reserved_basename, windows_reserved_path, FileExplorerPanel,
};
#[derive(Clone, Debug)]
pub(super) struct LabelEdit {
    /// Index into `self.rows` of the row being edited.
    pub(super) row_index: usize,
    /// What this edit will produce when committed.
    pub(super) kind: LabelEditKind,
    /// Cached snapshot of [`FileExplorerPanel::label_edit_region`]'s buffer
    /// text. Mirrored after every key dispatch by
    /// [`FileExplorerPanel::sync_label_edit_from_region`]. Render code reads
    /// this as a plain `&str` without needing access to the editor borrow.
    /// Includes any `/` segments — splitting into directories happens at
    /// commit time.
    pub(super) buffer: String,
    /// Cached cursor position (in chars) within `buffer`. Same sync rules
    /// as `buffer`. Used by render code to position the cursor without
    /// re-querying the region's selection.
    pub(super) cursor: usize,
}

#[derive(Clone, Debug)]
pub(super) enum LabelEditKind {
    /// Renaming an existing path. `source` is the on-disk path of the row
    /// being edited; `original_label` is what was there before we started
    /// editing (so cancelling restores it).
    Rename {
        source: PathBuf,
        original_label: String,
    },
    /// A brand-new row inserted into the tree at this depth — committing
    /// creates the file (or directory if the buffer ends in `/`).
    Create {
        /// Directory the new entry will be created in.
        parent: PathBuf,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ExplorerFileClipboard {
    operation: ExplorerFileOperation,
    paths: Box<[PathBuf]>,
}

impl FileExplorerPanel {
    pub(super) fn apply_operator_text_object(
        &mut self,
        operator: ExplorerOperator,
        object: LabelTextObject,
        cx: &mut Context,
    ) {
        self.select_label_text_object(object);
        self.apply_operator_selection_action(operator, cx);
    }

    pub(super) fn apply_operator_motion(
        &mut self,
        operator: ExplorerOperator,
        motion: LabelMotion,
        cx: &mut Context,
    ) {
        self.move_label_selection(motion, CoreMovement::Extend);
        self.apply_operator_selection_action(operator, cx);
    }

    /// Roll back the most recent file-system mutation made through the
    /// explorer. The editor maintains a deep file-operation history that
    /// covers everything routed through `ApplyMove` (renames triggered from
    /// `i`/`a`/`I`/`A` and from the `c` change-selection operator),
    /// `ApplyCreate`, confirmed deletes, and the clipboard paste operations —
    /// so a single binding (`u` by default) reverts any of them.
    pub(super) fn undo_file_operation(&mut self, cx: &mut Context) {
        crate::effect::file_operation::submit(
            cx.editor,
            cx.ingress.clone(),
            helix_view::editor::FileOperationRequest::undo(
                helix_view::editor::FileOperationOrigin::Explorer {
                    root: self.root.clone(),
                    cursor: selected_cursor(self.selection),
                    select_path: None,
                },
            ),
        );
    }

    pub(super) fn redo_file_operation(&mut self, cx: &mut Context) {
        crate::effect::file_operation::submit(
            cx.editor,
            cx.ingress.clone(),
            helix_view::editor::FileOperationRequest::redo(
                helix_view::editor::FileOperationOrigin::Explorer {
                    root: self.root.clone(),
                    cursor: selected_cursor(self.selection),
                    select_path: None,
                },
            ),
        );
    }

    fn selected_register(&self, editor: &Editor) -> char {
        editor
            .frontend()
            .focused_modal_input
            .selected_register
            .unwrap_or(editor.config().default_yank_register)
    }

    fn path_register_values(paths: &[PathBuf]) -> Result<Vec<String>, PathBuf> {
        paths
            .iter()
            .map(|path| {
                let relative = helix_stdx::path::get_relative_path(path);
                let value = relative.to_str().ok_or_else(|| path.clone())?;
                Ok(value.replace('\\', "/"))
            })
            .collect()
    }

    fn write_path_register(&mut self, cx: &mut Context, paths: &[PathBuf]) -> bool {
        let register = self.selected_register(cx.editor);
        let register_values = match Self::path_register_values(paths) {
            Ok(values) => values,
            Err(path) => {
                cx.editor
                    .set_error(format!("Unable to yank non-UTF-8 path {}", path.display()));
                return false;
            }
        };
        match cx.editor.registers.write(register, register_values) {
            Ok(()) => true,
            Err(err) => {
                cx.editor.set_error(err.to_string());
                false
            }
        }
    }

    pub(super) fn set_file_clipboard(
        &mut self,
        operation: ExplorerFileOperation,
        cx: &mut Context,
    ) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let register = self.selected_register(cx.editor);
        let register_values = match Self::path_register_values(&paths) {
            Ok(values) => values,
            Err(path) => {
                cx.editor
                    .set_error(format!("Unable to yank non-UTF-8 path {}", path.display()));
                return;
            }
        };
        match cx.editor.registers.write(register, register_values.clone()) {
            Ok(()) => {
                self.file_clipboard = Some(ExplorerFileClipboard { operation, paths });
                cx.editor.set_status(format!(
                    "{} {} path{} to register {register}",
                    operation.status_verb(),
                    register_values.len(),
                    if register_values.len() == 1 { "" } else { "s" },
                ));
            }
            Err(err) => cx.editor.set_error(err.to_string()),
        };
    }

    fn apply_operator_selection_action(&mut self, operator: ExplorerOperator, cx: &mut Context) {
        match operator {
            ExplorerOperator::Yank => self.set_file_clipboard(ExplorerFileOperation::Copy, cx),
            ExplorerOperator::Delete { yank } => self.delete_label_selection(cx, yank),
            ExplorerOperator::Change { yank } => self.change_label_selection(cx, yank),
        }
    }

    fn write_label_register(&mut self, cx: &mut Context, text: String) -> bool {
        let register = self.selected_register(cx.editor);
        match cx.editor.registers.write(register, vec![text]) {
            Ok(()) => true,
            Err(err) => {
                cx.editor.set_error(err.to_string());
                false
            }
        }
    }

    pub(super) fn delete_selected_item(&mut self, cx: &mut Context, yank: bool) {
        if yank {
            let paths = self.selected_paths();
            if !self.write_path_register(cx, &paths) {
                return;
            }
        }
        self.prompt_delete(cx);
    }

    pub(super) fn delete_label_selection(&mut self, cx: &mut Context, yank: bool) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        let Some(range) = self.selected_label_edit_range() else {
            return;
        };
        if yank && !self.write_label_register(cx, range.selected_text(&row.label)) {
            return;
        }

        if range.is_whole(row.label.chars().count()) {
            self.delete_selected_item(cx, false);
            return;
        }

        let new_label = range.remove_from(&row.label);
        self.rename_selected_label(cx, &row, new_label);
    }

    /// Begin an inline rename of the currently-selected row. The row's
    /// label is seeded into [`Self::label_edit_region`] and Insert mode
    /// is entered with the requested cursor placement (`InsertEntry::AtCurrent`
    /// for `i`, `Append` for `a`, `AtLineStart` for `I`, `AtLineEnd` for
    /// `A`). All cursor math is delegated to the region so the file
    /// explorer's behavior stays in lockstep with the main editor's
    /// `commands::insert_mode` / `append_mode` / etc.
    pub(super) fn enter_label_edit_rename(
        &mut self,
        editor: &mut Editor,
        entry: helix_view::edit_region::InsertEntry,
    ) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        if row.path.parent().is_none() {
            // Root row — refuse to edit.
            return;
        }
        let original_label = row.label.clone();
        // Seed the region's buffer with the row's current label, placing
        // the region cursor where the user's tree-Normal-mode cursor was
        // (so `w w i` lands at the right spot). `enter_insert_at` then
        // applies the per-entry transform on top of that.
        let initial_cursor = self.label_cursor().min(original_label.chars().count());
        self.label_edit_region
            .set_text(editor, &original_label, initial_cursor);
        self.label_edit_region.enter_insert_at(editor, entry);

        self.label_edit = Some(LabelEdit {
            row_index: self.selection,
            kind: LabelEditKind::Rename {
                source: row.path.clone(),
                original_label,
            },
            buffer: String::new(), // populated by sync below
            cursor: 0,
        });
        self.sync_label_edit_from_region(editor);
    }

    /// Begin an inline create on the selected row. Target parent depends
    /// on what's selected:
    /// - Expanded directory → create INSIDE it (the new row appears as a
    ///   visible child)
    /// - Collapsed directory → create at the explorer root, since adding
    ///   inside a closed folder would silently hide the new entry
    /// - File → create as a SIBLING in the file's parent directory
    ///
    /// The buffer starts empty so the user types the name directly. `/`
    /// in the name commits as nested directories (handled downstream by
    /// the parsed entry path's directory marker).
    pub(super) fn enter_label_edit_create(&mut self, cx: &mut Context) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        let parent = if row.is_dir && row.expanded {
            row.path.clone()
        } else if row.is_dir {
            // Collapsed dir — create at the visible root so the new entry
            // doesn't get hidden behind a closed folder.
            self.root.clone()
        } else {
            // File — sibling in its parent. Falls back to root if the
            // file somehow has no parent (shouldn't happen, but defensive).
            row.path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.clone())
        };

        // Empty buffer, cursor at 0 — user types the name directly.
        self.label_edit_region.set_text(cx.editor, "", 0);
        self.label_edit_region
            .enter_insert_at(cx.editor, helix_view::edit_region::InsertEntry::AtCurrent);

        self.label_edit = Some(LabelEdit {
            row_index: self.selection,
            kind: LabelEditKind::Create { parent },
            buffer: String::new(),
            cursor: 0,
        });
        self.sync_label_edit_from_region(cx.editor);
    }

    /// Mirror the region's text and cursor into the cached `LabelEdit`
    /// fields. Called after every operation that may have mutated the
    /// region's buffer (entry into Insert, key dispatch, …) so render
    /// code can read `edit.buffer` / `edit.cursor` without needing
    /// editor access. Also propagates the region's mode into
    /// `self.input.mode` so the explorer's mode chip and the cursor
    /// shape reflect Insert/Normal transitions inside the label.
    pub(super) fn sync_label_edit_from_region(&mut self, editor: &Editor) {
        let Some(edit) = self.label_edit.as_mut() else {
            return;
        };
        if let Some(doc) = self.label_edit_region.document(editor) {
            let text = doc.text().slice(..);
            edit.buffer = doc.text().to_string();
            edit.cursor = doc
                .selection(self.label_edit_region.view_id())
                .primary()
                .cursor(text);
        }
        self.input.mode = self.label_edit_region.mode();
    }

    /// Discard the in-progress label edit and restore Normal mode. No
    /// file-system operation is performed. Clears the underlying
    /// [`Self::label_edit_region`] so a subsequent rename starts from
    /// a clean slate (no leftover undo history from the previous edit).
    pub(super) fn cancel_label_edit(&mut self, editor: &mut Editor) {
        self.label_edit = None;
        self.label_selection = LabelSelection::default();
        self.input.mode = Mode::Normal;
        self.label_edit_region.clear(editor);
    }

    /// Commit the in-progress label edit to disk.
    ///
    /// The buffer is interpreted as a path relative to the edit's parent.
    /// `/` segments become intermediate directories that are auto-created.
    /// All filesystem mutations go through the editor's file-operation
    /// history so `u` reverts them. Runs synchronously so any failure
    /// surfaces immediately in the status line — no async dance, no lost
    /// errors.
    pub(super) fn commit_label_edit(&mut self, cx: &mut Context) {
        let Some(edit) = self.label_edit.take() else {
            return;
        };
        self.input.mode = Mode::Normal;
        self.label_selection = LabelSelection::default();
        // Clear the EditRegion now that we've snapshotted the buffer into
        // `edit.buffer` — the next rename starts from a clean slate
        // (empty doc, no leftover undo history from this edit).
        self.label_edit_region.clear(cx.editor);

        let buffer = edit.buffer.trim();
        if buffer.is_empty() {
            cx.editor.set_error("Name cannot be empty");
            return;
        }

        match &edit.kind {
            LabelEditKind::Rename {
                source,
                original_label,
            } => {
                if buffer == original_label.as_str() {
                    return; // no-op
                }
                let Some(parent) = source.parent() else {
                    cx.editor.set_error("Cannot rename root");
                    return;
                };
                let entry = match parse_entry_path(buffer) {
                    Ok(entry) => entry,
                    Err(error) => {
                        cx.editor.set_error(error.to_string());
                        return;
                    }
                };
                // Windows treats these names as device handles in every path
                // component, including nested create/rename input.
                if let Some(reserved) = windows_reserved_basename(source)
                    .or_else(|| windows_reserved_path(&entry.relative))
                {
                    cx.editor.set_error(format!(
                        "Cannot rename: '{reserved}' is a reserved Windows device name"
                    ));
                    return;
                }
                let destination = parent.join(entry.relative);

                let root = self.root.clone();
                let cursor = selected_cursor(self.selection);
                let source = source.clone();
                cx.spawn_ui(async move {
                    Ok(UiCommand::FileExplorer(FileExplorerCommand::ApplyMove {
                        source,
                        root,
                        cursor,
                        destination: helix_view::editor::FileOperationDestination::Exact(
                            destination,
                        ),
                        modified_buffer_check: ModifiedBufferCheck::Prompt,
                    }))
                });
            }
            LabelEditKind::Create { parent } => {
                let entry = match parse_entry_path(buffer) {
                    Ok(entry) => entry,
                    Err(error) => {
                        cx.editor.set_error(error.to_string());
                        return;
                    }
                };
                if let Some(reserved) = windows_reserved_path(&entry.relative) {
                    cx.editor.set_error(format!(
                        "Cannot create: '{reserved}' is a reserved Windows device name"
                    ));
                    return;
                }
                let target = parent.join(&entry.relative);
                let root = self.root.clone();
                let cursor = selected_cursor(self.selection);
                let is_dir = entry.is_dir;
                cx.spawn_ui(async move {
                    Ok(UiCommand::FileExplorer(FileExplorerCommand::ApplyCreate {
                        root,
                        cursor,
                        is_dir,
                        target,
                        modified_buffer_check: ModifiedBufferCheck::Prompt,
                    }))
                });
            }
        }
    }

    pub(super) fn change_label_selection(&mut self, cx: &mut Context, yank: bool) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        let Some(range) = self.selected_label_edit_range() else {
            return;
        };
        if yank && !self.write_label_register(cx, range.selected_text(&row.label)) {
            return;
        }

        // Drop into inline edit instead of opening the legacy rename prompt
        // — the region is seeded with the label minus the selected range,
        // and the cursor lands at the cut point so typing immediately
        // replaces what was selected.
        let new_label = range.remove_from(&row.label);
        let cursor_pos = range.start;
        self.label_edit_region
            .set_text(cx.editor, &new_label, cursor_pos);
        self.label_edit_region
            .enter_insert_at(cx.editor, helix_view::edit_region::InsertEntry::AtCurrent);
        self.label_edit = Some(LabelEdit {
            row_index: self.selection,
            kind: LabelEditKind::Rename {
                source: row.path.clone(),
                original_label: row.label.clone(),
            },
            buffer: String::new(),
            cursor: 0,
        });
        self.sync_label_edit_from_region(cx.editor);
    }

    fn rename_selected_label(&mut self, cx: &mut Context, row: &ExplorerRow, new_label: String) {
        let destination = match sibling_path_with_label(&row.path, &new_label) {
            Ok(destination) => destination,
            Err(err) => {
                cx.editor.set_error(err.to_string());
                return;
            }
        };
        if destination == row.path {
            cx.editor.set_status("File name unchanged");
            return;
        }

        let source = row.path.clone();
        let root = self.root.clone();
        let cursor = selected_cursor(self.selection);
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::ApplyMove {
                source,
                root,
                cursor,
                destination: helix_view::editor::FileOperationDestination::Exact(destination),
                modified_buffer_check: ModifiedBufferCheck::Prompt,
            }))
        });
    }

    pub(super) fn paste_file_clipboard(
        &mut self,
        cx: &mut Context,
        _placement: ExplorerPastePlacement,
    ) {
        let Some(clipboard) = self.file_clipboard.clone() else {
            cx.editor.set_status("No file operation to paste");
            return;
        };
        let destination_dir = self.selected_base_dir();
        let root = self.root.clone();
        let cursor = selected_cursor(self.selection);
        for source in clipboard.paths.iter() {
            let destination = helix_view::editor::FileOperationDestination::UniqueInDirectory(
                destination_dir.clone(),
            );
            let command = match clipboard.operation {
                ExplorerFileOperation::Copy => FileExplorerCommand::ApplyCopy {
                    source: source.clone(),
                    root: root.clone(),
                    cursor,
                    destination,
                    modified_buffer_check: ModifiedBufferCheck::Prompt,
                },
                ExplorerFileOperation::Move => FileExplorerCommand::ApplyMove {
                    source: source.clone(),
                    root: root.clone(),
                    cursor,
                    destination,
                    modified_buffer_check: ModifiedBufferCheck::Prompt,
                },
            };
            cx.submit_ui(UiCommand::FileExplorer(command));
        }

        if clipboard.operation == ExplorerFileOperation::Move {
            self.file_clipboard = None;
        }
    }

    fn prompt_delete(&self, cx: &mut Context) {
        let Some(row) = self.selected() else {
            return;
        };
        let target = row.path.clone();
        let root = self.root.clone();
        let cursor = selected_cursor(self.selection);
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptDelete {
                target,
                root,
                cursor,
            }))
        });
    }
}
