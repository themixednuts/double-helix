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
    compositor::{Context, PostAction},
    runtime::{
        ui::command::{FileExplorerCommand, ModifiedBufferCheck},
        UiCommand,
    },
    ui::confirmation::Confirmation,
};

use super::{
    input::{CreatePlacement, ExplorerFileOperation, ExplorerOperator, ExplorerPastePlacement},
    model::ExplorerRow,
    path_ops::{
        display_name, parse_entry_path, selected_cursor, sibling_path_with_label, validate_label,
    },
    windows_reserved_basename, windows_reserved_path, ExplorerPath, FileExplorerPanel,
};

/// Ephemeral path segment for an in-progress create row. Never written to disk;
/// the commit path uses the typed buffer under `LabelEditKind::Create::parent`.
const CREATE_ROW_SEGMENT: &str = ".helix-creating";

/// How many target names a ranged-delete confirmation spells out before it
/// falls back to counting the rest.
const DELETE_PROMPT_NAMES: usize = 3;

/// "Move 5 items to trash? (a.rs, b.rs, c.rs, +2 more)" — enough to see what
/// is about to go without a prompt that runs off the screen.
pub(super) fn delete_many_message(targets: &[std::path::PathBuf]) -> String {
    let named = targets
        .iter()
        .take(DELETE_PROMPT_NAMES)
        .map(display_name)
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = targets.len().saturating_sub(DELETE_PROMPT_NAMES);
    if remaining == 0 {
        format!("Move {} items to trash? ({named})", targets.len())
    } else {
        format!(
            "Move {} items to trash? ({named}, +{remaining} more)",
            targets.len()
        )
    }
}

/// Guide columns for a child of `row`, matching what `collect_rows` would
/// have produced. The root row contributes no column — the tree starts its
/// guides one level in.
fn child_ancestors(row: &ExplorerRow) -> Vec<bool> {
    if row.depth == 0 {
        return Vec::new();
    }
    let mut ancestors = row.ancestor_last.clone();
    ancestors.push(row.is_last);
    ancestors
}

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
        source: ExplorerPath,
        original_label: String,
    },
    /// A brand-new row inserted into the tree at this depth — committing
    /// creates the file (or directory if the buffer ends in `/`).
    Create {
        /// Directory the new entry will be created in.
        parent: ExplorerPath,
        /// Selection restored when the create is cancelled or aborted.
        restore_selection: usize,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ExplorerFileClipboard {
    operation: ExplorerFileOperation,
    paths: Box<[ExplorerPath]>,
}

impl FileExplorerPanel {
    pub(super) fn apply_operator_text_object(
        &mut self,
        operator: ExplorerOperator,
        object: LabelTextObject,
        cx: &mut Context,
    ) -> Option<PostAction> {
        self.select_label_text_object(object);
        self.apply_operator_selection_action(operator, cx)
    }

    pub(super) fn apply_operator_motion(
        &mut self,
        operator: ExplorerOperator,
        motion: LabelMotion,
        cx: &mut Context,
    ) -> Option<PostAction> {
        self.move_label_selection(motion, CoreMovement::Extend);
        self.apply_operator_selection_action(operator, cx)
    }

    /// Roll back the most recent file-system mutation made through the
    /// explorer. The editor maintains a deep file-operation history that
    /// covers everything routed through `ApplyMove` (renames triggered from
    /// `i`/`a`/`I`/`A` and from the `c` change-selection operator),
    /// `ApplyCreate`, confirmed deletes, and the clipboard paste operations —
    /// so a single binding (`u` by default) reverts any of them.
    pub(super) fn undo_file_operation(&mut self, cx: &mut Context) {
        let Some(root) = self.root.local_path().map(std::path::Path::to_path_buf) else {
            cx.submit_ui(UiCommand::FileExplorer(
                FileExplorerCommand::ReplayWorkspaceTransaction {
                    root: self.root.clone(),
                    cursor: selected_cursor(self.selection),
                    redo: false,
                },
            ));
            return;
        };
        crate::effect::file_operation::submit(
            cx.editor,
            cx.ingress.clone(),
            helix_view::editor::FileOperationRequest::undo(
                helix_view::editor::FileOperationOrigin::Explorer {
                    root,
                    cursor: selected_cursor(self.selection),
                    select_path: None,
                },
            ),
        );
    }

    pub(super) fn redo_file_operation(&mut self, cx: &mut Context) {
        let Some(root) = self.root.local_path().map(std::path::Path::to_path_buf) else {
            cx.submit_ui(UiCommand::FileExplorer(
                FileExplorerCommand::ReplayWorkspaceTransaction {
                    root: self.root.clone(),
                    cursor: selected_cursor(self.selection),
                    redo: true,
                },
            ));
            return;
        };
        crate::effect::file_operation::submit(
            cx.editor,
            cx.ingress.clone(),
            helix_view::editor::FileOperationRequest::redo(
                helix_view::editor::FileOperationOrigin::Explorer {
                    root,
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

    fn path_register_values(paths: &[ExplorerPath]) -> Result<Vec<String>, ExplorerPath> {
        paths
            .iter()
            .map(|path| match path {
                ExplorerPath::Local(path) => {
                    let relative = helix_stdx::path::get_relative_path(path);
                    let value = relative
                        .to_str()
                        .ok_or_else(|| ExplorerPath::Local(path.clone()))?;
                    Ok(value.replace('\\', "/"))
                }
                ExplorerPath::Remote(path) => Ok(path.to_string()),
                ExplorerPath::Collaboration { path, .. } => Ok(path.to_string()),
            })
            .collect()
    }

    fn write_path_register(&mut self, cx: &mut Context, paths: &[ExplorerPath]) -> bool {
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

    fn apply_operator_selection_action(
        &mut self,
        operator: ExplorerOperator,
        cx: &mut Context,
    ) -> Option<PostAction> {
        match operator {
            ExplorerOperator::Yank => {
                self.set_file_clipboard(ExplorerFileOperation::Copy, cx);
                None
            }
            ExplorerOperator::Delete { yank } => self.delete_label_selection(cx, yank),
            ExplorerOperator::Change { yank } => {
                self.change_label_selection(cx, yank);
                None
            }
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

    /// Delete the selected rows. A single row keeps today's flow (the runtime
    /// builds the "Move X to trash?" confirmation); a multi-row `x` range
    /// gets one confirmation covering every target, returned as a layer for
    /// the caller to push.
    pub(super) fn delete_selected_item(
        &mut self,
        cx: &mut Context,
        yank: bool,
    ) -> Option<PostAction> {
        let targets = self.selected_paths();
        if yank && !self.write_path_register(cx, &targets) {
            return None;
        }
        if targets.len() < 2 {
            self.prompt_delete(cx);
            return None;
        }
        self.confirm_delete_many(cx, &targets)
    }

    pub(super) fn delete_label_selection(
        &mut self,
        cx: &mut Context,
        yank: bool,
    ) -> Option<PostAction> {
        let row = self.selected().cloned()?;
        let range = self.selected_label_edit_range()?;
        if yank && !self.write_label_register(cx, range.selected_text(&row.label)) {
            return None;
        }

        if range.is_whole(row.label.chars().count()) {
            return self.delete_selected_item(cx, false);
        }

        let new_label = range.remove_from(&row.label);
        self.rename_selected_label(cx, &row, new_label);
        None
    }

    /// One confirmation for a ranged delete. Each target still goes through
    /// `ApplyConfirmedDelete`, so the root-containment check and the
    /// modified-buffer prompt stay exactly as they are for a single delete.
    fn confirm_delete_many(
        &mut self,
        cx: &mut Context,
        targets: &[ExplorerPath],
    ) -> Option<PostAction> {
        let root = self.root.local_path().map(std::path::Path::to_path_buf);
        let Some(root) = root else {
            cx.editor
                .notify_error("Deleting multiple items is not available on this backend yet");
            return None;
        };
        let locals = targets
            .iter()
            .map(|path| path.clone().into_local())
            .collect::<Option<Vec<_>>>();
        let Some(locals) = locals else {
            cx.editor
                .notify_error("Cannot delete across workspace backends");
            return None;
        };

        let message = delete_many_message(&locals);
        let cursor = selected_cursor(self.selection);
        // The range is spent once the deletes are queued — the rows it points
        // at are about to disappear.
        self.collapse_row_selection();

        Some(
            Confirmation::new(message, move |cx| {
                for target in locals {
                    cx.submit_ui(UiCommand::FileExplorer(
                        FileExplorerCommand::ApplyConfirmedDelete {
                            target,
                            root: root.clone(),
                            cursor,
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    ));
                }
            })
            .into_post_action(),
        )
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

    /// Begin an inline create by inserting a new tree row (like editor `o`/`O`)
    /// and editing its empty label.
    ///
    /// Target parent / insert position:
    /// - Expanded directory + below → first child inside it
    /// - Expanded directory + above → sibling above the directory
    /// - Collapsed directory → sibling (never inside a closed folder)
    /// - File → sibling in the file's parent directory
    /// - Root row → child of the explorer root
    ///
    /// `/` in the name commits as nested directories (handled downstream by
    /// the parsed entry path's directory marker).
    pub(super) fn enter_label_edit_create(&mut self, cx: &mut Context, placement: CreatePlacement) {
        if self.label_edit.is_some() {
            return;
        }
        let Some(row) = self.selected().cloned() else {
            return;
        };
        let restore_selection = self.selection;
        let (parent, insert_index, depth, ancestor_last) =
            self.create_insert_target(&row, placement);

        let Ok(path) = parent.join(CREATE_ROW_SEGMENT) else {
            cx.editor.set_error("Cannot create here");
            return;
        };

        let mut rows = self.rows.to_vec();
        let is_last = !matches!(rows.get(insert_index), Some(next) if next.depth == depth);
        if insert_index > 0 {
            if let Some(prev) = rows.get_mut(insert_index - 1) {
                if prev.depth == depth {
                    prev.is_last = false;
                }
            }
        }

        rows.insert(
            insert_index,
            ExplorerRow {
                path,
                label: String::new(),
                is_dir: false,
                depth,
                expanded: false,
                is_last,
                ancestor_last,
                vcs_status: None,
                diagnostic_status: None,
            },
        );
        self.rows = rows.into();
        // Keep the unfiltered tree in sync so a later empty-query filter
        // rebuild does not drop the create row.
        if self.search_query.trim().is_empty() {
            let mut all_rows = self.all_rows.to_vec();
            if insert_index <= all_rows.len() {
                if insert_index > 0 {
                    if let Some(prev) = all_rows.get_mut(insert_index - 1) {
                        if prev.depth == depth {
                            prev.is_last = false;
                        }
                    }
                }
                all_rows.insert(insert_index, self.rows[insert_index].clone());
                self.all_rows = all_rows.into();
            }
        }

        self.seek_to(insert_index);

        // Empty buffer, cursor at 0 — user types the name directly.
        self.label_edit_region.set_text(cx.editor, "", 0);
        self.label_edit_region
            .enter_insert_at(cx.editor, helix_view::edit_region::InsertEntry::AtCurrent);

        self.label_edit = Some(LabelEdit {
            row_index: insert_index,
            kind: LabelEditKind::Create {
                parent,
                restore_selection,
            },
            buffer: String::new(),
            cursor: 0,
        });
        self.sync_label_edit_from_region(cx.editor);
    }

    /// Resolve parent directory, insert index, depth, and tree-guide ancestors
    /// for a create row at `placement` relative to `row`.
    fn create_insert_target(
        &self,
        row: &ExplorerRow,
        placement: CreatePlacement,
    ) -> (ExplorerPath, usize, usize, Vec<bool>) {
        let selection = self.selection;
        // The root row is the tree header, not a guide column: `collect_rows`
        // seeds the recursion with `depth: 1` and no ancestors, so root
        // children draw a connector but no pipe. Matching that here keeps a
        // create row lined up with the siblings it is being inserted among.
        let root_child = || (self.root.clone(), 1usize, Vec::new());

        match placement {
            CreatePlacement::Below if row.is_dir && row.expanded => (
                row.path.clone(),
                selection + 1,
                row.depth + 1,
                child_ancestors(row),
            ),
            CreatePlacement::Below if row.depth == 0 => {
                let (parent, depth, ancestor_last) = root_child();
                (parent, selection + 1, depth, ancestor_last)
            }
            CreatePlacement::Below => {
                let parent = row.path.parent().unwrap_or_else(|| self.root.clone());
                (parent, selection + 1, row.depth, row.ancestor_last.clone())
            }
            CreatePlacement::Above if row.depth == 0 => {
                // Nothing above the root — open as first child instead.
                let (parent, depth, ancestor_last) = root_child();
                (parent, selection + 1, depth, ancestor_last)
            }
            CreatePlacement::Above if row.is_dir && row.expanded => {
                // Sibling above the directory, not a child above the header.
                let parent = row.path.parent().unwrap_or_else(|| self.root.clone());
                (parent, selection, row.depth, row.ancestor_last.clone())
            }
            CreatePlacement::Above => {
                let parent = row.path.parent().unwrap_or_else(|| self.root.clone());
                (parent, selection, row.depth, row.ancestor_last.clone())
            }
        }
    }

    /// Remove an in-progress create row and fix sibling `is_last` flags.
    fn remove_create_row_at(&mut self, index: usize) {
        let remove_from = |rows: &mut Vec<ExplorerRow>| {
            if index >= rows.len() {
                return;
            }
            let removed = rows.remove(index);
            if removed.is_last {
                if let Some(prev_idx) = rows[..index]
                    .iter()
                    .rposition(|row| row.depth == removed.depth)
                {
                    rows[prev_idx].is_last = true;
                }
            }
        };

        let mut rows = self.rows.to_vec();
        remove_from(&mut rows);
        self.rows = rows.into();

        if self.search_query.trim().is_empty() {
            let mut all_rows = self.all_rows.to_vec();
            remove_from(&mut all_rows);
            self.all_rows = all_rows.into();
        }
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
    /// Create edits also remove the inserted tree row and restore selection.
    pub(super) fn cancel_label_edit(&mut self, editor: &mut Editor) {
        if let Some(edit) = self.label_edit.take() {
            if let LabelEditKind::Create {
                restore_selection, ..
            } = edit.kind
            {
                self.remove_create_row_at(edit.row_index);
                self.seek_to(restore_selection.min(self.rows.len().saturating_sub(1)));
            }
        }
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
            if let LabelEditKind::Create {
                restore_selection, ..
            } = edit.kind
            {
                self.remove_create_row_at(edit.row_index);
                self.seek_to(restore_selection.min(self.rows.len().saturating_sub(1)));
            }
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
                if let Some(reserved) = source
                    .local_path()
                    .and_then(windows_reserved_basename)
                    .or_else(|| windows_reserved_path(&entry.relative))
                {
                    cx.editor.set_error(format!(
                        "Cannot rename: '{reserved}' is a reserved Windows device name"
                    ));
                    return;
                }
                let destination = match parent.join_relative(&entry.relative) {
                    Ok(destination) => destination,
                    Err(error) => {
                        cx.editor.set_error(error);
                        return;
                    }
                };

                if let (Some(source), Some(destination)) = (
                    source.remote_path().cloned(),
                    destination.remote_path().cloned(),
                ) {
                    cx.submit_ui(UiCommand::FileExplorer(
                        FileExplorerCommand::ApplyWorkspaceTransaction {
                            root: self.root.clone(),
                            cursor: selected_cursor(self.selection),
                            select_path: Some(ExplorerPath::Remote(destination.clone())),
                            transaction: helix_remote::FileTransaction {
                                operations: vec![helix_remote::FileOperation::Rename {
                                    from: source,
                                    to: destination.clone(),
                                    overwrite: false,
                                }],
                            },
                            success: format!("Renamed remote path to {destination}"),
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    ));
                    return;
                }

                if let (Some(source), Some(destination)) = (
                    source.collaboration_path().cloned(),
                    destination.collaboration_path().cloned(),
                ) {
                    cx.submit_ui(UiCommand::FileExplorer(
                        FileExplorerCommand::ApplyWorkspaceTransaction {
                            root: self.root.clone(),
                            cursor: selected_cursor(self.selection),
                            select_path: self.root.with_workspace_path(destination.clone()),
                            transaction: helix_workspace::FileTransaction {
                                operations: vec![helix_workspace::FileOperation::Rename {
                                    from: source,
                                    to: destination.clone(),
                                    overwrite: false,
                                }],
                            },
                            success: format!("Renamed shared path to {destination}"),
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    ));
                    return;
                }

                let Some(root) = self.root.local_path().map(std::path::Path::to_path_buf) else {
                    cx.editor
                        .notify_error("Cannot rename across workspace backends");
                    return;
                };
                let cursor = selected_cursor(self.selection);
                let Some(source) = source.local_path().map(std::path::Path::to_path_buf) else {
                    cx.editor.notify_error("Remote rename is not available yet");
                    return;
                };
                let Some(destination) = destination.into_local() else {
                    cx.editor.notify_error("Remote rename is not available yet");
                    return;
                };
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
            LabelEditKind::Create {
                parent,
                restore_selection,
            } => {
                // Drop the placeholder row before the filesystem create; a
                // successful ApplyCreate refreshes the real tree afterward.
                self.remove_create_row_at(edit.row_index);
                self.seek_to((*restore_selection).min(self.rows.len().saturating_sub(1)));

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
                let is_dir = entry.is_dir;
                let target = match parent.join_relative(&entry.relative) {
                    Ok(target) => target,
                    Err(error) => {
                        cx.editor.set_error(error);
                        return;
                    }
                };
                if let Some(target) = target.remote_path().cloned() {
                    let operation = if is_dir {
                        helix_remote::FileOperation::CreateDirectory {
                            path: target.clone(),
                        }
                    } else {
                        helix_remote::FileOperation::CreateFile {
                            path: target.clone(),
                            overwrite: false,
                        }
                    };
                    cx.submit_ui(UiCommand::FileExplorer(
                        FileExplorerCommand::ApplyWorkspaceTransaction {
                            root: self.root.clone(),
                            cursor: selected_cursor(self.selection),
                            select_path: Some(ExplorerPath::Remote(target.clone())),
                            transaction: helix_remote::FileTransaction {
                                operations: vec![operation],
                            },
                            success: format!("Created remote path {target}"),
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    ));
                    return;
                }
                if let Some(target) = target.collaboration_path().cloned() {
                    let operation = if is_dir {
                        helix_workspace::FileOperation::CreateDirectory {
                            path: target.clone(),
                        }
                    } else {
                        helix_workspace::FileOperation::CreateFile {
                            path: target.clone(),
                            overwrite: false,
                        }
                    };
                    cx.submit_ui(UiCommand::FileExplorer(
                        FileExplorerCommand::ApplyWorkspaceTransaction {
                            root: self.root.clone(),
                            cursor: selected_cursor(self.selection),
                            select_path: self.root.with_workspace_path(target.clone()),
                            transaction: helix_workspace::FileTransaction {
                                operations: vec![operation],
                            },
                            success: format!("Created shared path {target}"),
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    ));
                    return;
                }
                let Some(root) = self.root.local_path().map(std::path::Path::to_path_buf) else {
                    cx.editor
                        .notify_error("Cannot create across workspace backends");
                    return;
                };
                let Some(target) = target.into_local() else {
                    cx.editor
                        .notify_error("Cannot create across workspace backends");
                    return;
                };
                let cursor = selected_cursor(self.selection);
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
        if let Some(source) = row.path.remote_path().cloned() {
            if let Err(error) = validate_label(&new_label) {
                cx.editor.set_error(error.to_string());
                return;
            }
            let Some(parent) = source.parent() else {
                cx.editor.set_error("Cannot rename root");
                return;
            };
            let destination = match parent.join(new_label) {
                Ok(destination) => destination,
                Err(error) => {
                    cx.editor.set_error(error.to_string());
                    return;
                }
            };
            cx.submit_ui(UiCommand::FileExplorer(
                FileExplorerCommand::ApplyWorkspaceTransaction {
                    root: self.root.clone(),
                    cursor: selected_cursor(self.selection),
                    select_path: Some(ExplorerPath::Remote(destination.clone())),
                    transaction: helix_remote::FileTransaction {
                        operations: vec![helix_remote::FileOperation::Rename {
                            from: source,
                            to: destination.clone(),
                            overwrite: false,
                        }],
                    },
                    success: format!("Renamed remote path to {destination}"),
                    modified_buffer_check: ModifiedBufferCheck::Prompt,
                },
            ));
            return;
        }
        if let Some(source) = row.path.collaboration_path().cloned() {
            if let Err(error) = validate_label(&new_label) {
                cx.editor.set_error(error.to_string());
                return;
            }
            let Some(parent) = source.parent() else {
                cx.editor.set_error("Cannot rename root");
                return;
            };
            let destination = match parent.join(new_label) {
                Ok(destination) => destination,
                Err(error) => {
                    cx.editor.set_error(error.to_string());
                    return;
                }
            };
            cx.submit_ui(UiCommand::FileExplorer(
                FileExplorerCommand::ApplyWorkspaceTransaction {
                    root: self.root.clone(),
                    cursor: selected_cursor(self.selection),
                    select_path: self.root.with_workspace_path(destination.clone()),
                    transaction: helix_workspace::FileTransaction {
                        operations: vec![helix_workspace::FileOperation::Rename {
                            from: source,
                            to: destination.clone(),
                            overwrite: false,
                        }],
                    },
                    success: format!("Renamed shared path to {destination}"),
                    modified_buffer_check: ModifiedBufferCheck::Prompt,
                },
            ));
            return;
        }
        let Some(local_path) = row.path.local_path() else {
            cx.editor
                .notify_error("Cannot rename across workspace backends");
            return;
        };
        let destination = match sibling_path_with_label(local_path, &new_label) {
            Ok(destination) => destination,
            Err(err) => {
                cx.editor.set_error(err.to_string());
                return;
            }
        };
        if row.path.local_path() == Some(destination.as_path()) {
            cx.editor.set_status("File name unchanged");
            return;
        }

        let source = local_path.to_path_buf();
        let Some(root) = self.root.local_path().map(std::path::Path::to_path_buf) else {
            cx.editor.notify_error("Remote rename is not available yet");
            return;
        };
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
        if let Some(destination) = destination_dir.remote_path().cloned() {
            for source in clipboard.paths.iter() {
                let Some(source) = source.remote_path().cloned() else {
                    cx.editor
                        .notify_error("Cannot mix local and remote clipboard paths");
                    return;
                };
                cx.submit_ui(UiCommand::FileExplorer(
                    FileExplorerCommand::ApplyWorkspacePaste {
                        root: self.root.clone(),
                        cursor: selected_cursor(self.selection),
                        source,
                        destination: destination.clone(),
                        move_source: clipboard.operation == ExplorerFileOperation::Move,
                        modified_buffer_check: ModifiedBufferCheck::Prompt,
                    },
                ));
            }
            if clipboard.operation == ExplorerFileOperation::Move {
                self.file_clipboard = None;
            }
            return;
        }
        if let Some(destination) = destination_dir.collaboration_path().cloned() {
            for source in clipboard.paths.iter() {
                let Some(source) = source.collaboration_path().cloned() else {
                    cx.editor
                        .notify_error("Cannot mix workspace backends in the file clipboard");
                    return;
                };
                cx.submit_ui(UiCommand::FileExplorer(
                    FileExplorerCommand::ApplyWorkspacePaste {
                        root: self.root.clone(),
                        cursor: selected_cursor(self.selection),
                        source,
                        destination: destination.clone(),
                        move_source: clipboard.operation == ExplorerFileOperation::Move,
                        modified_buffer_check: ModifiedBufferCheck::Prompt,
                    },
                ));
            }
            if clipboard.operation == ExplorerFileOperation::Move {
                self.file_clipboard = None;
            }
            return;
        }
        let Some(destination_dir) = destination_dir.into_local() else {
            cx.editor
                .notify_error("Cannot paste across workspace backends");
            return;
        };
        let Some(root) = self.root.local_path().map(std::path::Path::to_path_buf) else {
            cx.editor.notify_error("Remote paste is not available yet");
            return;
        };
        let cursor = selected_cursor(self.selection);
        for source in clipboard.paths.iter() {
            let Some(source) = source.local_path().map(std::path::Path::to_path_buf) else {
                cx.editor
                    .notify_error("Cannot mix local and remote clipboard paths");
                return;
            };
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
                    source,
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
        if let Some(target) = target.remote_path().cloned() {
            cx.submit_ui(UiCommand::FileExplorer(
                FileExplorerCommand::PromptWorkspaceDelete {
                    root: self.root.clone(),
                    cursor: selected_cursor(self.selection),
                    target,
                },
            ));
            return;
        }
        if let Some(target) = target.collaboration_path().cloned() {
            cx.submit_ui(UiCommand::FileExplorer(
                FileExplorerCommand::PromptWorkspaceDelete {
                    root: self.root.clone(),
                    cursor: selected_cursor(self.selection),
                    target,
                },
            ));
            return;
        }
        let Some(target) = target.into_local() else {
            cx.editor
                .notify_error("Cannot delete across workspace backends");
            return;
        };
        let Some(root) = self.root.local_path().map(std::path::Path::to_path_buf) else {
            cx.editor.notify_error("Remote delete is not available yet");
            return;
        };
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
