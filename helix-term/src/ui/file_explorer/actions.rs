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
    path_ops::{display_path, selected_cursor, sibling_path_with_label, unique_destination},
    windows_reserved_basename, windows_reserved_label, FileExplorerPanel,
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
    /// `ApplyCreate`, `ApplyDelete`, and the clipboard paste operations —
    /// so a single binding (`u` by default) reverts any of them.
    pub(super) fn undo_file_operation(&mut self, cx: &mut Context) {
        match cx.editor.undo_file_operation() {
            Ok(Some(message)) => {
                self.refresh_current(cx.editor);
                self.queue_vcs_refresh(cx);
                cx.editor.set_status(message);
            }
            Ok(None) => cx.editor.set_status("No file operation to undo"),
            Err(err) => cx
                .editor
                .set_error(format!("Unable to undo file operation: {err}")),
        }
    }

    pub(super) fn redo_file_operation(&mut self, cx: &mut Context) {
        match cx.editor.redo_file_operation() {
            Ok(Some(message)) => {
                self.refresh_current(cx.editor);
                self.queue_vcs_refresh(cx);
                cx.editor.set_status(message);
            }
            Ok(None) => cx.editor.set_status("No file operation to redo"),
            Err(err) => cx
                .editor
                .set_error(format!("Unable to redo file operation: {err}")),
        }
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
    /// `apply_create`'s `is_directory_input`).
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
                // Windows treats names like `NUL`, `CON`, `PRN`, `AUX`,
                // `COM1`-`COM9`, `LPT1`-`LPT9` as device shortcuts —
                // `fs::rename` on either side surfaces as a cryptic
                // "Incorrect function" OS error. Catch it explicitly.
                if let Some(reserved) =
                    windows_reserved_basename(source).or_else(|| windows_reserved_label(buffer))
                {
                    cx.editor.set_error(format!(
                        "Cannot rename: '{reserved}' is a reserved Windows device name"
                    ));
                    return;
                }
                // Construct the destination through `Path::new` so the OS
                // sorts out separator style consistently. `parent.join` on
                // Windows yields the native form regardless of slashes in
                // the typed buffer.
                let destination = parent.join(Path::new(buffer));

                // If the user typed sub-paths (`foo/bar.rs` style),
                // ensure all intermediate directories exist before the
                // rename. Each create is its own undo entry, so an undo
                // sequence walks back through dirs as well as the rename.
                if let Some(dest_parent) = destination.parent() {
                    if !dest_parent.exists() {
                        if let Err(err) = cx.editor.create_path_with_history(dest_parent, true) {
                            cx.editor.set_error(format!(
                                "Unable to create directory {}: {err}",
                                dest_parent.display()
                            ));
                            return;
                        }
                    }
                }

                match cx.editor.move_path_with_history(source, &destination) {
                    Ok(()) => {
                        self.refresh_current(cx.editor);
                        self.queue_vcs_refresh(cx);
                        let new_name = destination
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(buffer);
                        cx.editor
                            .set_status(format!("Renamed {original_label} → {new_name}"));
                    }
                    Err(err) => {
                        cx.editor.set_error(format!(
                            "Unable to rename {} → {}: {err}",
                            source.display(),
                            destination.display()
                        ));
                    }
                }
            }
            LabelEditKind::Create { parent } => {
                if let Some(reserved) = windows_reserved_label(buffer) {
                    cx.editor.set_error(format!(
                        "Cannot create: '{reserved}' is a reserved Windows device name"
                    ));
                    return;
                }
                let target = parent.join(Path::new(buffer));
                let is_dir = buffer.ends_with('/') || buffer.ends_with('\\');
                match cx.editor.create_path_with_history(&target, is_dir) {
                    Ok(()) => {
                        self.refresh_current(cx.editor);
                        self.queue_vcs_refresh(cx);
                        cx.editor.set_status(format!(
                            "Created {} {}",
                            if is_dir { "directory" } else { "file" },
                            target.display()
                        ));
                    }
                    Err(err) => {
                        cx.editor
                            .set_error(format!("Unable to create {}: {err}", target.display()));
                    }
                }
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
        let input = display_path(&destination);
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::ApplyMove {
                source,
                root,
                cursor,
                input,
                destination,
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
        if let Err(err) = std::fs::create_dir_all(&destination_dir) {
            cx.editor.set_error(format!(
                "Unable to create destination directory {}: {err}",
                destination_dir.display()
            ));
            return;
        }

        let mut changed = 0usize;
        for source in clipboard.paths.iter() {
            if clipboard.operation == ExplorerFileOperation::Copy && source.is_dir() {
                cx.editor.set_error(format!(
                    "Copying directories is not supported: {} is a directory",
                    source.display()
                ));
                return;
            }

            if clipboard.operation == ExplorerFileOperation::Move
                && source.parent() == Some(destination_dir.as_path())
            {
                continue;
            }

            let destination = match unique_destination(&destination_dir, source) {
                Ok(destination) => destination,
                Err(err) => {
                    cx.editor.set_error(format!(
                        "Unable to choose destination in {}: {err}",
                        destination_dir.display()
                    ));
                    return;
                }
            };
            let result = match clipboard.operation {
                ExplorerFileOperation::Copy => cx
                    .editor
                    .copy_path_with_history(source, &destination)
                    .map(|_| ()),
                ExplorerFileOperation::Move => {
                    cx.editor.move_path_with_history(source, &destination)
                }
            };

            if let Err(err) = result {
                cx.editor.set_error(format!(
                    "Unable to {} {} -> {}: {err}",
                    clipboard.operation.paste_verb().to_ascii_lowercase(),
                    source.display(),
                    destination.display()
                ));
                return;
            }
            changed = changed.saturating_add(1);
        }

        if clipboard.operation == ExplorerFileOperation::Move {
            self.file_clipboard = None;
        }
        self.refresh_current(cx.editor);
        self.queue_vcs_refresh(cx);
        cx.editor.set_status(format!(
            "{} {} path{} into {}",
            clipboard.operation.paste_verb(),
            changed,
            if changed == 1 { "" } else { "s" },
            destination_dir.display()
        ));
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
