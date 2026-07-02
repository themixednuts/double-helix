use std::{
    collections::{HashMap, HashSet},
    error::Error as _,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use helix_core::{
    movement::Movement as CoreMovement, syntax, unicode::width::UnicodeWidthStr, Position,
};
use helix_view::{
    editor::{Action, CloseError, ClosePolicy},
    graphics::{CursorKind, Rect},
    icons::{Icon, ICONS},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    modal_text::{
        ModalTextMotion as LabelMotion, ModalTextObject as LabelTextObject,
        ModalTextSelection as LabelSelection,
    },
    model::{FocusTarget, PanelId},
    traits::{Bounded, Focusable, Scrollable},
    DocumentId, Editor,
};

use crate::{
    component_traits,
    compositor::{Component, Context, Event, EventResult, PostAction, RenderContext},
    runtime::{ui::command::FileExplorerCommand, UiCommand},
};

mod actions;
mod input;
mod model;
mod path_ops;
mod preview;
mod refresh;
mod render;
mod scan;
#[cfg(test)]
use actions::LabelEditKind;
use actions::{ExplorerFileClipboard, LabelEdit};
#[cfg(test)]
use input::explorer_local_keymap;
use input::{ExplorerAction, ExplorerInput, ExplorerInputEngine};
#[cfg(test)]
use input::{ExplorerFileOperation, ExplorerOperator, ExplorerPastePlacement};
#[cfg(test)]
use model::DiagnosticStatus;
use model::{DiagnosticSnapshot, ExplorerRow, VcsSnapshot, VcsStatus};
#[cfg(test)]
use path_ops::LabelRenameError;
use path_ops::{display_path, selected_cursor, LabelEditRange};
use preview::{ExplorerPreview, PreviewDocumentCache};
#[cfg(test)]
use render::{ExplorerStatusStyles, ExplorerTreeItemStyles};
use scan::ExplorerChild;

pub const ID: &str = "file-explorer-panel";

const HEADER_ROWS: u16 = 1;
/// Single statusline strip beneath the tree (mode chip · summary chips ·
/// counts). Transient error / info messages live in the editor's global
/// status row (rendered by `EditorView` from `cx.status_msg()`) — no
/// need to duplicate them here.
const FOOTER_ROWS: u16 = 1;
pub(crate) const PANEL_WIDTH: u16 = 34;
const FALLBACK_FOLDER_ICON: &str = "";
const FALLBACK_FOLDER_OPEN_ICON: &str = "󰝰";
const FALLBACK_FILE_ICON: &str = "󰈔";
const VCS_ADDED_ICON: &str = "";
const VCS_MODIFIED_ICON: &str = "○";
const VCS_DELETED_ICON: &str = "";
const VCS_RENAMED_ICON: &str = "";
const VCS_CONFLICT_ICON: &str = "";
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);
// Preview requests fire synchronously — `apply_preview_request` filters
// stale ones on arrival, so no debounce is needed to coalesce navigation.

pub struct FileExplorerPanel {
    root: PathBuf,
    rows: Vec<ExplorerRow>,
    expanded_dirs: HashSet<PathBuf>,
    children_cache: HashMap<PathBuf, Vec<ExplorerChild>>,
    vcs_snapshot: VcsSnapshot,
    diagnostic_snapshot: DiagnosticSnapshot,
    input: ExplorerInputEngine,
    file_clipboard: Option<ExplorerFileClipboard>,
    /// Cached row-selection index. Read-only mirror of `self.nav.selection()`;
    /// every write goes through [`Self::sync_nav_to_cache`] after the
    /// nav has updated, so the field tracks the source of truth in
    /// `nav` without forcing every read site to take an extra method
    /// call. Don't `= ` this field directly — call into `self.nav`
    /// and let `sync_nav_to_cache` propagate the new value.
    selection: usize,
    label_selection: LabelSelection,
    /// Cached scroll offset. Read-only mirror of `self.nav.scroll()`;
    /// same rules as `selection`.
    scroll: usize,
    /// Row selection + scroll state machine shared with the picker
    /// and menu (see `helix-view/src/list_nav.rs`). Owns the cursor
    /// math; the explorer just funnels writes through it and reads
    /// the mirrored `selection` / `scroll` fields. Single source of
    /// truth — if `nav` says selection=5, `self.selection` is 5
    /// (enforced by `sync_nav_to_cache`).
    nav: helix_view::list_nav::ListNav,
    area: Rect,
    focused: bool,
    preview: ExplorerPreview,
    preview_cache: PreviewDocumentCache,
    preview_debouncer: Option<crate::runtime::RuntimeUiDebouncer>,
    model_panel_id: Option<PanelId>,
    last_click: Option<ExplorerClick>,
    /// In-place edit state for the currently-selected row.
    ///
    /// When `Some(edit)`, that row's label is replaced in the tree with
    /// `edit.buffer` and the cursor sits at `edit.cursor`. The truth lives
    /// in [`Self::label_edit_region`] — `buffer` / `cursor` are synced
    /// from it after each key dispatch via
    /// [`Self::sync_label_edit_from_region`]. Enter or Esc commits the
    /// change as a file-system operation (rename or create) that goes
    /// through the editor's file-operation history so `u` reverts it.
    label_edit: Option<LabelEdit>,
    /// The actual editable buffer + modal engine for inline rename / create.
    ///
    /// Inserts, deletes, motions, operators, and Insert/Normal mode
    /// transitions are all handled by the editor's modal engine inside
    /// this region — there's no hand-rolled key dispatch. Surfaces that
    /// embed an `EditRegion` all run the same code path, so `a` (and
    /// every other key) behaves exactly the same here as in a normal
    /// buffer. See `helix-view/src/edit_region.rs` for the full contract.
    label_edit_region: helix_view::edit_region::EditRegion,
    /// Active label-jump session (Helix's `gw`-style two-letter jump),
    /// or `None` when no jump is in progress. Same shape as
    /// [`Self::label_edit`]: when this is `Some`, the panel hijacks key
    /// dispatch and routes the next 1–2 keys through the session
    /// before falling back to normal Tree-mode handling. Rendering
    /// paints the session's labels over the first cells of each
    /// visible row. See `helix-view/src/jump_labels.rs` for the
    /// session contract and label-allocation algorithm.
    jump_session: Option<helix_view::jump_labels::JumpSession>,
}

#[derive(Clone, Debug)]
struct ExplorerClick {
    path: PathBuf,
    at: Instant,
}

/// Windows reserved device names — files named these can't be created or
/// renamed because the OS routes them to a device handle.
const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// If `label` (case-insensitive, ignoring the extension) matches a Windows
/// reserved device name, return the matching reserved name; otherwise None.
fn windows_reserved_label(label: &str) -> Option<&'static str> {
    // Strip the extension — `NUL.txt` is still treated as reserved on Windows.
    let stem = label.split('.').next().unwrap_or(label);
    WINDOWS_RESERVED_NAMES
        .iter()
        .find(|reserved| stem.eq_ignore_ascii_case(reserved))
        .copied()
}

/// Like [`windows_reserved_label`] but takes a path and inspects its file
/// name component.
fn windows_reserved_basename(path: &Path) -> Option<&'static str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(windows_reserved_label)
}

/// For a label motion, returns the direction it would "wrap" if it
/// hits the boundary of the current row's label: `+1` for forward
/// (`w` / `e` / `Char(>0)`), `-1` for backward (`b` / `Char(<0)`),
/// `None` for motions that have no row-wrap semantics
/// (`LineStart` / `LineEnd` / `FindChar` — these are intentionally
/// row-local).
fn motion_row_wrap_direction(motion: LabelMotion) -> Option<i32> {
    use helix_view::modal_text::ModalTextMotion;
    match motion {
        ModalTextMotion::Char(n) => {
            if n > 0 {
                Some(1)
            } else if n < 0 {
                Some(-1)
            } else {
                None
            }
        }
        ModalTextMotion::NextWordStart(_) | ModalTextMotion::NextWordEnd(_) => Some(1),
        ModalTextMotion::PrevWordStart(_) | ModalTextMotion::PrevWordEnd(_) => Some(-1),
        ModalTextMotion::LineStart | ModalTextMotion::LineEnd => None,
        ModalTextMotion::FindChar { .. } => None,
        // `ModalTextMotion` is `#[non_exhaustive]`. New variants
        // default to "no row wrap" — we'd rather mis-classify a future
        // motion as row-local than accidentally jump a row on the
        // user's first encounter with it.
        _ => None,
    }
}

fn icon_width(icon: &Icon) -> u16 {
    icon.glyph()
        .width()
        .saturating_add(2)
        .try_into()
        .unwrap_or(u16::MAX)
}

fn text_width(text: &str) -> u16 {
    text.width().try_into().unwrap_or(u16::MAX)
}

fn selected_path_for_log(rows: &[ExplorerRow], selection: usize) -> String {
    rows.get(selection)
        .map(|row| display_path(&row.path))
        .unwrap_or_else(|| String::from("<none>"))
}

fn row_status_width(row: &ExplorerRow) -> u16 {
    let icons = ICONS.load();
    let mut width = 0u16;
    if let Some(status) = row.vcs_status {
        width = width.saturating_add(status_icon_width(status.icon(&icons)));
    }
    if let Some(diagnostic) = row.diagnostic_status {
        width = width.saturating_add(status_icon_width(diagnostic.icon(&icons)));
    }
    width
}

fn status_icon_width(icon: &str) -> u16 {
    text_width(icon).max(1).saturating_add(1)
}

impl FileExplorerPanel {
    pub fn new(root: PathBuf, editor: &Editor) -> Result<Self, std::io::Error> {
        Self::new_with_cursor(root, editor, None)
    }

    pub fn new_with_cursor(
        root: PathBuf,
        editor: &Editor,
        cursor: Option<usize>,
    ) -> Result<Self, std::io::Error> {
        let root = helix_stdx::path::normalize(&root);
        let mut panel = Self {
            root: root.clone(),
            rows: Vec::new(),
            expanded_dirs: HashSet::from([root.clone()]),
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::empty(&root, editor.config().file_explorer.vcs),
            diagnostic_snapshot: DiagnosticSnapshot::empty(
                &root,
                editor.config().file_explorer.diagnostics,
            ),
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_debouncer: None,
            model_panel_id: None,
            last_click: None,
            label_edit: None,
            label_edit_region: helix_view::edit_region::EditRegion::default(),
            jump_session: None,
            nav: helix_view::list_nav::ListNav::new(),
        };
        panel.refresh(editor, None, cursor)?;
        Ok(panel)
    }

    fn visible_height(&self) -> usize {
        self.area.height.saturating_sub(HEADER_ROWS) as usize
    }

    /// Pull `nav`'s state into the cached `selection` / `scroll`
    /// fields. Called after every operation that goes through
    /// `self.nav` so the read-only mirror fields stay accurate
    /// without forcing every read site to take a method-call hop.
    /// Also pushes `nav`'s viewport / item-count from the explorer's
    /// current row list + visible area, so a subsequent call to
    /// `self.nav.move_by(...)` sees fresh constraints.
    fn sync_nav_to_cache(&mut self) {
        self.nav.set_item_count(self.rows.len());
        self.nav.set_viewport_height(self.visible_height());
        self.selection = self.nav.selection();
        self.scroll = self.nav.scroll();
    }

    /// Push the current row count + viewport into `nav` *without*
    /// pulling the selection back. Used at the head of operations
    /// that are about to call `nav.move_by` etc. — guarantees the
    /// nav math sees the freshest constraints before it runs.
    fn prime_nav(&mut self) {
        self.nav.set_item_count(self.rows.len());
        self.nav.set_viewport_height(self.visible_height());
        // After set_item_count the nav may have clamped selection
        // into range; if so mirror that back so direct reads of
        // `self.selection` agree.
        self.selection = self.nav.selection();
        self.scroll = self.nav.scroll();
    }

    fn ensure_selection_visible(&mut self) {
        self.prime_nav();
        self.nav.ensure_visible();
        self.selection = self.nav.selection();
        self.scroll = self.nav.scroll();
    }

    /// Set the row selection to a specific index, routing through
    /// `nav` so the source of truth stays consistent. Clamps to
    /// `0..rows.len()` and re-pulls the scroll to keep the cursor
    /// visible. Use this *anywhere* the explorer wants to jump the
    /// selection to a known row (refresh-restore, parent-jump,
    /// jump-session result, …) instead of writing `self.selection = N`
    /// directly — that bypasses `nav` and lets the two diverge.
    fn seek_to(&mut self, index: usize) {
        self.prime_nav();
        self.nav.set_selection(index);
        self.sync_nav_to_cache();
    }

    fn selected(&self) -> Option<&ExplorerRow> {
        self.rows.get(self.selection)
    }

    fn selected_base_dir(&self) -> PathBuf {
        self.selected()
            .map(|row| {
                if row.is_dir {
                    row.path.clone()
                } else {
                    row.path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.root.clone())
                }
            })
            .unwrap_or_else(|| self.root.clone())
    }

    fn selected_paths(&self) -> Box<[PathBuf]> {
        self.selected()
            .map(|row| vec![row.path.clone()].into_boxed_slice())
            .unwrap_or_default()
    }

    fn selected_label(&self) -> Option<&str> {
        // When an inline edit is in progress on the selected row, the
        // displayed text comes from the edit buffer — selection, cursor, and
        // any rendered tree row all read from here.
        if let Some(edit) = &self.label_edit {
            if edit.row_index == self.selection {
                return Some(edit.buffer.as_str());
            }
        }
        self.selected().map(|row| row.label.as_str())
    }

    fn label_cursor(&self) -> usize {
        // When an inline edit is active, the truth lives in the
        // EditRegion; render code reads the synced `LabelEdit::cursor`
        // mirror so it doesn't need editor borrow. Otherwise fall back
        // to the explorer's tree-mode `label_selection` cursor.
        if let Some(edit) = &self.label_edit {
            return edit.cursor;
        }
        self.selected_label()
            .map_or(0, |label| self.label_selection.cursor(label))
    }

    /// The text that should be displayed for `row` at `row_index` — usually
    /// `row.label`, but swapped for the inline-edit buffer when this row is
    /// being edited.
    fn display_label_for<'a>(&'a self, row: &'a ExplorerRow, row_index: usize) -> &'a str {
        if let Some(edit) = &self.label_edit {
            if edit.row_index == row_index {
                return edit.buffer.as_str();
            }
        }
        row.label.as_str()
    }

    fn clamp_label_selection(&mut self) {
        let Some(label) = self.selected_label().map(str::to_owned) else {
            self.label_selection = LabelSelection::default();
            return;
        };
        self.label_selection = self.label_selection.clamp(&label);
    }

    fn collapse_label_selection_to_cursor(&mut self) {
        let cursor = self.label_cursor();
        self.label_selection = LabelSelection::point(cursor);
    }

    fn move_selection_by(&mut self, delta: isize) {
        self.prime_nav();
        // File explorer uses Clamp — a wrap from the last file to the
        // top of the tree (or vice versa) would feel teleporty.
        // ListNav handles the empty-list case (returns AtBoundary).
        self.nav
            .move_by(delta, helix_view::list_nav::WrapBehavior::Clamp);
        self.sync_nav_to_cache();
        self.clamp_label_selection();
        self.collapse_label_selection_to_cursor();
    }

    fn page_by(&mut self, delta: isize) {
        self.prime_nav();
        // Use FullViewport pages to match the previous file-explorer
        // behavior (the tree was paging by full visible_height).
        self.nav.page_by(
            delta,
            helix_view::list_nav::PageSize::FullViewport,
            helix_view::list_nav::WrapBehavior::Clamp,
        );
        self.sync_nav_to_cache();
        self.clamp_label_selection();
        self.collapse_label_selection_to_cursor();
    }

    fn select_first(&mut self) {
        self.prime_nav();
        self.nav.to_first();
        self.sync_nav_to_cache();
        self.clamp_label_selection();
        self.collapse_label_selection_to_cursor();
    }

    fn select_last(&mut self) {
        self.prime_nav();
        self.nav.to_last();
        self.sync_nav_to_cache();
        self.clamp_label_selection();
        self.collapse_label_selection_to_cursor();
    }

    fn select_diagnostic_at(&mut self, index: usize) {
        self.seek_to(index);
        self.clamp_label_selection();
        self.collapse_label_selection_to_cursor();
    }

    fn select_first_diagnostic(&mut self) {
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| row.diagnostic_status.is_some())
        {
            self.select_diagnostic_at(index);
        }
    }

    fn select_last_diagnostic(&mut self) {
        if let Some(index) = self
            .rows
            .iter()
            .rposition(|row| row.diagnostic_status.is_some())
        {
            self.select_diagnostic_at(index);
        }
    }

    fn select_next_diagnostic(&mut self) {
        if let Some(index) = self
            .rows
            .iter()
            .enumerate()
            .skip(self.selection.saturating_add(1))
            .find_map(|(index, row)| row.diagnostic_status.is_some().then_some(index))
        {
            self.select_diagnostic_at(index);
        }
    }

    fn select_previous_diagnostic(&mut self) {
        if let Some(index) = self
            .rows
            .iter()
            .enumerate()
            .take(self.selection)
            .rev()
            .find_map(|(index, row)| row.diagnostic_status.is_some().then_some(index))
        {
            self.select_diagnostic_at(index);
        }
    }

    fn move_label_selection(&mut self, motion: LabelMotion, movement: CoreMovement) {
        let Some(label) = self.selected_label().map(str::to_owned) else {
            return;
        };
        let before = self.label_selection;
        let after = self.label_selection.apply_motion(&label, motion, movement);
        self.label_selection = after;

        // Row-wrap behavior: when a word motion would have moved the
        // cursor but didn't (we were already at the boundary), spill
        // into the next/prev row. This makes `w`/`b`/`e` feel like the
        // editor's word motions across "lines" — except in the tree
        // each row is one line. The user explicitly asked for this:
        // motions should wrap row-to-row rather than dead-ending at the
        // current label's boundary.
        //
        // We only wrap when the in-label motion is a true no-op
        // (after.cursor == before.cursor). A partial-progress motion
        // (e.g. `w` from col 3 to col 6 on a 10-char label) just lands
        // mid-label and stops there, same as the editor does mid-line.
        if before.cursor(&label) != after.cursor(&label) {
            return;
        }
        let Some(direction) = motion_row_wrap_direction(motion) else {
            return;
        };
        self.wrap_to_adjacent_row(direction, movement);
    }

    /// Move the row selection by `direction` (positive for next row,
    /// negative for previous) and place the label cursor at the
    /// row-entry edge so the user's next keystroke continues naturally.
    /// Used by [`Self::move_label_selection`] when a word motion would
    /// otherwise stall at the current label's boundary.
    fn wrap_to_adjacent_row(&mut self, direction: i32, _movement: CoreMovement) {
        if self.rows.is_empty() {
            return;
        }
        let next_index = if direction > 0 {
            self.selection.saturating_add(1)
        } else {
            self.selection.saturating_sub(1)
        };
        if next_index == self.selection {
            return; // already at first/last row
        }
        if next_index >= self.rows.len() {
            return; // out of bounds going forward
        }
        self.seek_to(next_index);

        // Land the label cursor at the row-entry edge. Cross-row label
        // selection is not represented, so wrapping resets to the new label.
        self.label_selection = LabelSelection::point(0);
    }

    fn select_label_text_object(&mut self, object: LabelTextObject) {
        let Some(label) = self.selected_label().map(str::to_owned) else {
            return;
        };
        let count = self.input.count.map(NonZeroUsize::get).unwrap_or(1);
        self.label_selection = self
            .label_selection
            .select_text_object(&label, object, count);
    }

    fn select_whole_label(&mut self) {
        let Some(label) = self.selected_label().map(str::to_owned) else {
            return;
        };
        self.label_selection = LabelSelection::all(&label);
    }

    fn flip_label_selection(&mut self) {
        let Some(label) = self.selected_label().map(str::to_owned) else {
            return;
        };
        self.label_selection = self.label_selection.flip().clamp(&label);
    }

    fn selected_label_edit_range(&self) -> Option<LabelEditRange> {
        let label = self.selected_label()?;
        LabelEditRange::from_selection(self.label_selection, label)
    }

    fn toggle_selected_dir(&mut self, editor: &Editor) {
        let Some(row) = self.selected().filter(|row| row.is_dir).cloned() else {
            return;
        };
        if row.expanded {
            self.collapse_dir_preserving_descendant_state(&row.path);
        } else {
            self.expanded_dirs.insert(row.path);
        }
        if let Err(err) = self.refresh_preserving_tree(editor, None, Some(self.selection)) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    fn collapse_or_select_parent(&mut self, editor: &Editor) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        if row.is_dir && row.expanded {
            self.collapse_dir_preserving_descendant_state(&row.path);
            if let Err(err) = self.refresh_preserving_tree(editor, None, Some(self.selection)) {
                log::error!("failed to refresh file explorer: {err}");
            }
            return;
        }

        if row.depth == 0 {
            return;
        }

        if let Some(parent_index) = self.rows[..self.selection]
            .iter()
            .rposition(|candidate| candidate.depth + 1 == row.depth)
        {
            self.seek_to(parent_index);
            self.clamp_label_selection();
            self.collapse_label_selection_to_cursor();
        }
    }

    fn root_parent(&mut self, editor: &Editor) {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else {
            return;
        };
        if let Err(err) = self.refresh_preserving_tree(editor, Some(parent), Some(0)) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    fn go_workspace_root(&mut self, editor: &Editor) {
        let root = helix_loader::find_workspace().0;
        if let Err(err) = self.refresh_preserving_tree(editor, Some(root), Some(0)) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    fn open_selected(&mut self, cx: &mut Context, action: Action) {
        let Some(row) = self.selected().cloned() else {
            return;
        };

        if row.is_dir {
            self.toggle_selected_dir(cx.editor);
            return;
        }

        match cx.editor.open(&row.path, action) {
            Ok(_) => {
                self.preview = ExplorerPreview::None;
                self.focused = false;
            }
            Err(err) => {
                let message = err
                    .source()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("unable to open \"{}\"", row.path.display()));
                cx.editor.set_error(message);
            }
        }
    }

    pub fn preview_selected_file(&mut self, editor: &mut Editor) {
        let preview_start = Instant::now();
        let documents_before = editor.document_count();
        let component_documents_before = editor.component_docs.len();
        let focused_doc_before = editor.focused_document_id();
        let focused_view_before = editor.focused_view_id();
        let Some(row) = self.selected().filter(|row| !row.is_dir).cloned() else {
            log::info!(
                "[file_explorer] preview_skip reason=no_selected_file selection={} selected={} preview={:?} documents={} component_documents={} elapsed_us={}",
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
                self.preview,
                documents_before,
                component_documents_before,
                preview_start.elapsed().as_micros(),
            );
            return;
        };
        let path = helix_stdx::path::canonicalize(&row.path);
        let current_path = editor
            .tree
            .try_get(editor.tree.focus)
            .and_then(|view| editor.document(view.doc))
            .and_then(|doc| doc.path())
            .map(helix_stdx::path::canonicalize);
        if current_path.as_deref() == Some(path.as_path()) {
            log::info!(
                "[file_explorer] preview_skip reason=already_current selection={} path={} focused_view={:?} focused_doc={:?} preview={:?} documents={} component_documents={} elapsed_us={}",
                self.selection,
                display_path(&path),
                focused_view_before,
                focused_doc_before,
                self.preview,
                documents_before,
                component_documents_before,
                preview_start.elapsed().as_micros(),
            );
            return;
        }

        let focus = editor.model.focus;
        let existing_doc = editor.document_id_by_path(&path);
        log::info!(
            "[file_explorer] preview_open_start selection={} path={} current_path={} focused_view={:?} focused_doc={:?} existing_doc={:?} preview_before={:?} preview_cache_entries={} documents_before={} component_documents_before={}",
            self.selection,
            display_path(&path),
            current_path
                .as_deref()
                .map(display_path)
                .unwrap_or_else(|| String::from("<scratch>")),
            focused_view_before,
            focused_doc_before,
            existing_doc,
            self.preview,
            self.preview_cache.len(),
            documents_before,
            component_documents_before,
        );
        let open_start = Instant::now();
        let cached_preview = if existing_doc.is_none() {
            self.preview_cache.take(&path)
        } else {
            None
        };
        let restored_from_cache = cached_preview.is_some();
        let open_result = if let Some(doc) = cached_preview {
            Ok(editor.restore_preview_document(doc, Action::Replace))
        } else {
            editor.open_preview(&path, Action::Replace)
        };
        match open_result {
            Ok(doc_id) => {
                let open_elapsed = open_start.elapsed();
                log::info!(
                    "[file_explorer] preview_open_done path={} doc={:?} existing_doc={:?} restored_from_cache={} open_us={} preview_cache_entries={} documents_after_open={} component_documents_after_open={}",
                    display_path(&path),
                    doc_id,
                    existing_doc,
                    restored_from_cache,
                    open_elapsed.as_micros(),
                    self.preview_cache.len(),
                    editor.document_count(),
                    editor.component_docs.len(),
                );
                self.replace_preview_document(editor, doc_id, existing_doc.is_none());
                editor.model.focus = focus;
                self.focused = true;
                log::info!(
                    "[file_explorer] preview_done path={} doc={:?} restored_from_cache={} preview_after={:?} preview_cache_entries={} restored_focus={:?} focused_view_before={:?} focused_doc_before={:?} focused_view_after={:?} focused_doc_after={:?} documents_after={} component_documents_after={} total_us={}",
                    display_path(&path),
                    doc_id,
                    restored_from_cache,
                    self.preview,
                    self.preview_cache.len(),
                    focus,
                    focused_view_before,
                    focused_doc_before,
                    editor.focused_view_id(),
                    editor.focused_document_id(),
                    editor.document_count(),
                    editor.component_docs.len(),
                    preview_start.elapsed().as_micros(),
                );
            }
            Err(err) => {
                let message = err
                    .source()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("unable to preview \"{}\"", path.display()));
                editor.set_error(message);
                log::info!(
                    "[file_explorer] preview_error path={} existing_doc={:?} restored_from_cache={} preview={:?} preview_cache_entries={} documents={} component_documents={} total_us={}",
                    display_path(&path),
                    existing_doc,
                    restored_from_cache,
                    self.preview,
                    self.preview_cache.len(),
                    editor.document_count(),
                    editor.component_docs.len(),
                    preview_start.elapsed().as_micros(),
                );
            }
        }
    }

    fn queue_preview_syntax_refresh(
        &self,
        editor: &Editor,
        ingress: crate::runtime::RuntimeIngress,
        doc_id: DocumentId,
        path: PathBuf,
    ) {
        let Some(doc) = editor.document(doc_id) else {
            return;
        };
        if doc.has_syntax() {
            return;
        }
        let Some(language) = doc.language_config().map(|config| config.language()) else {
            return;
        };

        let version = doc.version();
        let text = doc.text().clone();
        let loader = editor.syn_loader.load();
        let block = editor.runtime().block().clone();
        let path = helix_stdx::path::canonicalize(path);
        log::info!(
            "[file_explorer] preview_syntax_queued doc={:?} path={} version={} bytes={}",
            doc_id,
            display_path(&path),
            version,
            text.len_bytes(),
        );

        block
            .spawn(move || {
                let start = Instant::now();
                let syntax = match helix_core::Syntax::new_with_timeout(
                    text.slice(..),
                    language,
                    &loader,
                    syntax::BACKGROUND_PARSE_TIMEOUT,
                ) {
                    Ok(syntax) => syntax,
                    Err(err) => {
                        log::info!(
                            "[file_explorer] preview_syntax_failed doc={:?} path={} version={} error={} elapsed_us={}",
                            doc_id,
                            display_path(&path),
                            version,
                            err,
                            start.elapsed().as_micros(),
                        );
                        return;
                    }
                };

                log::info!(
                    "[file_explorer] preview_syntax_done doc={:?} path={} version={} elapsed_us={}",
                    doc_id,
                    display_path(&path),
                    version,
                    start.elapsed().as_micros(),
                );
                ingress.ui(UiCommand::Document(
                    crate::runtime::ui::command::DocumentCommand::ApplySyntax {
                        document: doc_id,
                        path,
                        version,
                        syntax,
                    },
                ));
            })
            .detach();
    }

    fn replace_preview_document(
        &mut self,
        editor: &mut Editor,
        current_preview: DocumentId,
        owned_by_preview: bool,
    ) {
        let previous = std::mem::replace(&mut self.preview, ExplorerPreview::None);
        log::info!(
            "[file_explorer] preview_replace_start previous={:?} current={:?} owned_by_preview={} preview_cache_entries={} documents_before={} component_documents_before={}",
            previous,
            current_preview,
            owned_by_preview,
            self.preview_cache.len(),
            editor.document_count(),
            editor.component_docs.len(),
        );
        if let ExplorerPreview::Owned(previous_preview) = previous {
            if previous_preview != current_preview && editor.contains_document(previous_preview) {
                let close_start = Instant::now();
                if let Some(doc) = editor.take_preview_document(previous_preview) {
                    if let Some(path) = doc.path().map(Path::to_path_buf) {
                        self.preview_cache
                            .insert(helix_stdx::path::canonicalize(&path), doc);
                        log::info!(
                            "[file_explorer] preview_close_done previous={:?} result=cached elapsed_us={} preview_cache_entries={} documents_after={} component_documents_after={}",
                            previous_preview,
                            close_start.elapsed().as_micros(),
                            self.preview_cache.len(),
                            editor.document_count(),
                            editor.component_docs.len(),
                        );
                    } else {
                        log::info!(
                            "[file_explorer] preview_close_done previous={:?} result=detached_no_path elapsed_us={} preview_cache_entries={} documents_after={} component_documents_after={}",
                            previous_preview,
                            close_start.elapsed().as_micros(),
                            self.preview_cache.len(),
                            editor.document_count(),
                            editor.component_docs.len(),
                        );
                    }
                } else if let Err(err) =
                    editor.close_document(previous_preview, ClosePolicy::ProtectModified)
                {
                    let reason = match err {
                        CloseError::DoesNotExist => String::from("document no longer exists"),
                        CloseError::BufferModified(name) => {
                            format!("document is modified: {name}")
                        }
                        CloseError::SaveError(err) => format!("save failed: {err}"),
                    };
                    log::warn!(
                        "[file_explorer] unable to close previous preview document {:?}: {}",
                        previous_preview,
                        reason
                    );
                    log::info!(
                        "[file_explorer] preview_close_done previous={:?} result=error elapsed_us={} preview_cache_entries={} documents_after={} component_documents_after={}",
                        previous_preview,
                        close_start.elapsed().as_micros(),
                        self.preview_cache.len(),
                        editor.document_count(),
                        editor.component_docs.len(),
                    );
                } else {
                    log::info!(
                        "[file_explorer] preview_close_done previous={:?} result=closed elapsed_us={} preview_cache_entries={} documents_after={} component_documents_after={}",
                        previous_preview,
                        close_start.elapsed().as_micros(),
                        self.preview_cache.len(),
                        editor.document_count(),
                        editor.component_docs.len(),
                    );
                }
            }
        }

        if owned_by_preview {
            self.preview = ExplorerPreview::Owned(current_preview);
        }
        log::info!(
            "[file_explorer] preview_replace_done current={:?} preview_after={:?} preview_cache_entries={} documents_after={} component_documents_after={}",
            current_preview,
            self.preview,
            self.preview_cache.len(),
            editor.document_count(),
            editor.component_docs.len(),
        );
    }

    /// Start a label-jump session over the currently visible rows.
    ///
    /// Calculates how many rows fit in the list area, builds a session
    /// with that target count, and stores it on the panel so the next
    /// 1–2 keystrokes route through it via [`Self::handle_jump_key`].
    /// Each visible row's target ID is its on-screen index
    /// (`screen_row = absolute_row - self.scroll`), keeping the
    /// session decoupled from how rows are stored. If there are no
    /// visible rows the call is a no-op — there's nothing to jump to.
    fn start_jump_session(&mut self, editor: &Editor) {
        let Some(list) = Self::list_area(self.area) else {
            return;
        };
        let visible_count =
            (self.rows.len().saturating_sub(self.scroll)).min(list.height as usize) as u32;
        if visible_count == 0 {
            return;
        }
        // Use the editor's configured alphabet so the user's
        // `editor.jump-label-alphabet` setting affects every surface
        // that runs label jumps — the file explorer, the editor's
        // own `gw`, and any future picker label feature.
        // `.clone()` is cheap (a `Vec<char>` of a-z is 26 chars).
        let alphabet = editor.config().jump_label_alphabet.clone();
        if alphabet.is_empty() {
            // Defensive — an empty alphabet would mean no labels can
            // ever be generated. Skip rather than spawn a dead session.
            return;
        }
        self.jump_session = Some(helix_view::jump_labels::JumpSession::new(
            visible_count,
            alphabet,
        ));
    }

    /// Dispatch a key into the active jump session. Returns `true` if
    /// the session is still alive (host should keep eating keys); the
    /// session is cleared automatically on resolution or cancel.
    fn handle_jump_key(&mut self, key: KeyEvent) -> bool {
        use helix_view::jump_labels::JumpSignal;
        let Some(session) = self.jump_session.as_mut() else {
            return false;
        };
        let signal = session.feed_key(key);
        match signal {
            JumpSignal::Pending => true,
            JumpSignal::Selected(target_id) => {
                let absolute = self.scroll.saturating_add(target_id as usize);
                if absolute < self.rows.len() {
                    self.seek_to(absolute);
                }
                self.jump_session = None;
                false
            }
            JumpSignal::Cancelled => {
                self.jump_session = None;
                false
            }
        }
    }

    fn close(&mut self, cx: &mut Context) -> EventResult {
        self.cancel_preview_request();
        if let ExplorerPreview::Owned(doc_id) = self.preview {
            cx.editor.promote_preview_document(doc_id);
        }
        self.preview = ExplorerPreview::None;
        if let Some(id) = self.model_panel_id.take() {
            cx.editor.model.remove_panel(id);
        }
        EventResult::Consumed(Some(PostAction::RemoveById(ID)))
    }

    fn cancel_preview_request(&mut self) {
        if let Some(debouncer) = &self.preview_debouncer {
            debouncer.cancel();
        }
    }

    pub fn queue_selected_preview(
        &mut self,
        _editor: &Editor,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        let Some(row) = self.selected().filter(|row| !row.is_dir).cloned() else {
            self.cancel_preview_request();
            log::info!(
                "[file_explorer] preview_queue_skip reason=no_selected_file selection={} selected={}",
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
            );
            return;
        };
        let root = self.root.clone();
        let path = row.path.clone();
        let cursor = selected_cursor(self.selection);
        // Preview requests fire synchronously now — tree navigation should
        // feel instant. `apply_preview_request` performs a staleness check
        // against the current selection on the receiving side, so older
        // requests that arrive after the user has moved on are skipped
        // there instead of being coalesced here.
        log::info!(
            "[file_explorer] preview_queued root={} path={} cursor={}",
            display_path(&root),
            display_path(&path),
            cursor,
        );
        ingress.ui(UiCommand::FileExplorer(
            FileExplorerCommand::PreviewSelection { root, path, cursor },
        ));
    }

    pub fn apply_preview_request(
        &mut self,
        editor: &mut Editor,
        ingress: crate::runtime::RuntimeIngress,
        root: PathBuf,
        path: PathBuf,
        cursor: u32,
    ) {
        let start = Instant::now();
        let requested_root = helix_stdx::path::normalize(root);
        let requested_path = helix_stdx::path::normalize(path);
        let cursor = usize::try_from(cursor).unwrap_or(usize::MAX);
        let selected_path = self
            .selected()
            .map(|row| helix_stdx::path::normalize(&row.path));
        if requested_root != self.root {
            log::info!(
                "[file_explorer] preview_request_skip reason=root_mismatch requested_root={} current_root={} path={} cursor={} elapsed_us={}",
                display_path(&requested_root),
                display_path(&self.root),
                display_path(&requested_path),
                cursor,
                start.elapsed().as_micros(),
            );
            return;
        }
        if self.selection != cursor || selected_path.as_deref() != Some(requested_path.as_path()) {
            log::info!(
                "[file_explorer] preview_request_skip reason=stale requested_path={} current_selected={} requested_cursor={} current_cursor={} elapsed_us={}",
                display_path(&requested_path),
                selected_path
                    .as_deref()
                    .map(display_path)
                    .unwrap_or_else(|| String::from("<none>")),
                cursor,
                self.selection,
                start.elapsed().as_micros(),
            );
            return;
        }
        let panel_focused = self.model_panel_id.map_or(self.focused, |panel_id| {
            editor.model.focus == FocusTarget::Panel(panel_id)
        });
        if !self.focused || !panel_focused {
            log::info!(
                "[file_explorer] preview_request_skip reason=not_focused path={} cursor={} panel_id={:?} focus={:?} elapsed_us={}",
                display_path(&requested_path),
                cursor,
                self.model_panel_id,
                editor.model.focus,
                start.elapsed().as_micros(),
            );
            return;
        }
        log::info!(
            "[file_explorer] preview_request_apply path={} cursor={} elapsed_us={}",
            display_path(&requested_path),
            cursor,
            start.elapsed().as_micros(),
        );
        self.preview_selected_file(editor);
        if let ExplorerPreview::Owned(doc_id) = self.preview {
            self.queue_preview_syntax_refresh(editor, ingress, doc_id, requested_path);
        }
    }

    fn execute_action(&mut self, action: ExplorerAction, cx: &mut Context) -> EventResult {
        let start = Instant::now();
        let rows_before = self.rows.len();
        let selection_before = self.selection;
        let selected_before = selected_path_for_log(&self.rows, self.selection);
        let cache_before = self.children_cache.len();
        let expanded_before = self.expanded_dirs.len();

        match action {
            ExplorerAction::Close => return self.close(cx),
            ExplorerAction::MoveSelection(delta) => self.move_selection_by(delta),
            ExplorerAction::Page(delta) => self.page_by(delta),
            ExplorerAction::SelectFirst => self.select_first(),
            ExplorerAction::SelectLast => self.select_last(),
            ExplorerAction::Open(action) => self.open_selected(cx, action),
            ExplorerAction::ToggleDirectory => self.toggle_selected_dir(cx.editor),
            ExplorerAction::CollapseOrSelectParent => self.collapse_or_select_parent(cx.editor),
            ExplorerAction::RootParent => self.root_parent(cx.editor),
            ExplorerAction::GoWorkspaceRoot => self.go_workspace_root(cx.editor),
            ExplorerAction::UndoFileOperation => self.undo_file_operation(cx),
            ExplorerAction::RedoFileOperation => self.redo_file_operation(cx),
            ExplorerAction::Refresh => {
                self.refresh_current(cx.editor);
                self.queue_vcs_refresh(cx);
            }
            ExplorerAction::ShowHelp => {
                cx.editor.autoinfo = self.input.root_infobox();
            }
            ExplorerAction::SelectFirstDiagnostic => self.select_first_diagnostic(),
            ExplorerAction::SelectLastDiagnostic => self.select_last_diagnostic(),
            ExplorerAction::SelectNextDiagnostic => self.select_next_diagnostic(),
            ExplorerAction::SelectPreviousDiagnostic => self.select_previous_diagnostic(),
            ExplorerAction::MoveLabelSelection(motion, movement) => {
                self.move_label_selection(motion, movement)
            }
            ExplorerAction::SelectLabelTextObject(object) => self.select_label_text_object(object),
            ExplorerAction::SelectWholeLabel => self.select_whole_label(),
            ExplorerAction::CollapseLabelSelection => self.collapse_label_selection_to_cursor(),
            ExplorerAction::FlipLabelSelection => self.flip_label_selection(),
            ExplorerAction::SetMode(mode) => {
                self.input.mode = mode;
            }
            ExplorerAction::ClipboardOperation(operation) => self.set_file_clipboard(operation, cx),
            ExplorerAction::PasteClipboard(placement) => self.paste_file_clipboard(cx, placement),
            ExplorerAction::ApplyOperatorTextObject(operator, object) => {
                self.apply_operator_text_object(operator, object, cx)
            }
            ExplorerAction::ApplyOperatorMotion(operator, motion) => {
                self.apply_operator_motion(operator, motion, cx)
            }
            ExplorerAction::BeginOperator(_) => {}
            ExplorerAction::DeleteLabelSelection { yank } => self.delete_label_selection(cx, yank),
            ExplorerAction::ChangeLabelSelection { yank } => self.change_label_selection(cx, yank),
            ExplorerAction::DeleteSelectedItem { yank } => self.delete_selected_item(cx, yank),
            ExplorerAction::EnterLabelEdit(entry) => {
                if self.label_edit.is_some() {
                    // Already editing — re-entering from Normal mode just
                    // moves the cursor inside the existing buffer and
                    // flips the region back into Insert mode. The cursor
                    // transform lives in [`EditRegion::enter_insert_at`].
                    self.label_edit_region.enter_insert_at(cx.editor, entry);
                    self.sync_label_edit_from_region(cx.editor);
                } else {
                    self.enter_label_edit_rename(cx.editor, entry);
                }
            }
            ExplorerAction::EnterCreate => {
                self.enter_label_edit_create(cx);
            }
            ExplorerAction::StartJumpSession => {
                self.start_jump_session(cx.editor);
            }
            ExplorerAction::DelegateToEditor => {}
            ExplorerAction::Noop => {}
        }

        if matches!(action, ExplorerAction::Open(_)) {
            self.cancel_preview_request();
        } else {
            self.queue_selected_preview(cx.editor, cx.ingress.clone());
        }

        log::info!(
            "[file_explorer] action action={:?} elapsed_us={} rows_before={} rows_after={} selection_before={} selection_after={} selected_before={} selected_after={} cache_before={} cache_after={} expanded_before={} expanded_after={}",
            action,
            start.elapsed().as_micros(),
            rows_before,
            self.rows.len(),
            selection_before,
            self.selection,
            selected_before,
            selected_path_for_log(&self.rows, self.selection),
            cache_before,
            self.children_cache.len(),
            expanded_before,
            self.expanded_dirs.len()
        );

        EventResult::Consumed(None)
    }

    /// Dispatch a key into the label-edit region while an inline rename
    /// or create is in progress.
    ///
    /// All the editor's Insert-mode goodies — `Backspace`, `Ctrl-W`,
    /// `Ctrl-U`, arrow keys, `Home`/`End`, word motions in Normal mode,
    /// operators like `d`/`c`/`y` against the label buffer, in-buffer
    /// undo with `u` — are inherited from [`EditRegion::dispatch`].
    /// The file explorer just provides the policy
    /// ([`HostPolicy::single_line_commit`]) and acts on the resulting
    /// [`DispatchSignal`]. This is the entire reason for the abstraction:
    /// there's no longer a hand-rolled keymap that can drift from the
    /// editor's.
    fn handle_label_edit_key(&mut self, key: KeyEvent, cx: &mut Context) -> EventResult {
        use helix_view::edit_region::{DispatchSignal, HostPolicy};

        let signal =
            self.label_edit_region
                .dispatch(cx.editor, key, HostPolicy::single_line_commit());
        // Mirror region state into the cached `LabelEdit` fields after
        // every dispatch, regardless of outcome — render code may run
        // before the next key arrives and needs the buffer / cursor to
        // be fresh.
        self.sync_label_edit_from_region(cx.editor);

        match signal {
            DispatchSignal::Submit => self.commit_label_edit(cx),
            DispatchSignal::Cancel => self.cancel_label_edit(cx.editor),
            DispatchSignal::Consumed => {}
            DispatchSignal::Bubble => {
                // While editing, the file explorer eats unbound keys.
                // Bubbling to the global editor mid-rename would let `:`
                // open the cmdline halfway through typing a filename —
                // never what the user wants.
            }
        }
        EventResult::Consumed(None)
    }

    fn handle_key(&mut self, key: KeyEvent, cx: &mut Context) -> EventResult {
        let start = Instant::now();

        // While a jump-label session is active, the next 1–2 keys go
        // straight into the session — they're label characters, not
        // commands. Order matters: jump session has higher priority
        // than label_edit, because if the user managed to start both
        // they want the most recent modal action to be honored.
        // (In practice they're mutually exclusive — but the order
        // makes the precedence explicit.)
        if self.jump_session.is_some() {
            self.handle_jump_key(key);
            return EventResult::Consumed(None);
        }

        // While an inline edit is in progress, keys flow through the
        // EditRegion's unified dispatch path — same code path as the
        // assistant input, the (future) cmdline, and everything else
        // that embeds an `EditRegion`. The policy
        // ([`HostPolicy::single_line_commit`]) turns Enter and Esc into
        // commits, Ctrl-C into cancel, and filters newlines from input.
        if self.label_edit.is_some() {
            return self.handle_label_edit_key(key, cx);
        }

        self.input.prepare_keymaps(cx.editor);
        let input = self.input.translate(key);
        cx.editor.frontend_mut().focused_modal_input = self.input.modal_input_state();
        log::info!(
            "[file_explorer] key key={:?} input={:?} translate_us={}",
            key,
            input,
            start.elapsed().as_micros()
        );

        let result = match input {
            ExplorerInput::Pending(info) => {
                cx.editor.autoinfo = info;
                EventResult::Consumed(None)
            }
            ExplorerInput::Execute(action) => {
                if matches!(action, ExplorerAction::DelegateToEditor) {
                    self.input.finish_command();
                    cx.editor.frontend_mut().focused_modal_input = self.input.modal_input_state();
                    return EventResult::Ignored(None);
                }
                cx.editor.autoinfo = None;
                let result = self.execute_action(action, cx);
                self.input.finish_command();
                cx.editor.frontend_mut().focused_modal_input = self.input.modal_input_state();
                result
            }
        };
        log::info!(
            "[file_explorer] key_done key={:?} total_us={} rows={} selection={} selected={}",
            key,
            start.elapsed().as_micros(),
            self.rows.len(),
            self.selection,
            selected_path_for_log(&self.rows, self.selection)
        );
        result
    }

    fn list_area(area: Rect) -> Option<Rect> {
        if area.width == 0 || area.height <= HEADER_ROWS + FOOTER_ROWS {
            return None;
        }

        let inner = crate::widgets::Panel::edge(
            crate::widgets::PanelStyle::default(),
            crate::widgets::PanelEdge::Right,
        )
        .content_area(area);
        if inner.width == 0 {
            return None;
        }

        let list = inner
            .clip_top(HEADER_ROWS)
            .clip_bottom(FOOTER_ROWS)
            .clip_left(1);
        (list.width > 0 && list.height > 0).then_some(list)
    }

    fn row_index_at_mouse(&self, event: &MouseEvent) -> Option<usize> {
        let list = Self::list_area(self.area)?;
        if !list.contains(event.column, event.row) {
            return None;
        }

        let index = self
            .scroll
            .saturating_add(event.row.saturating_sub(list.y) as usize);
        (index < self.rows.len()).then_some(index)
    }

    fn is_double_click(&self, path: &Path, now: Instant) -> bool {
        self.last_click.as_ref().is_some_and(|click| {
            click.path == path && now.duration_since(click.at) <= DOUBLE_CLICK_WINDOW
        })
    }

    fn handle_mouse_at(
        &mut self,
        event: &MouseEvent,
        cx: &mut Context,
        now: Instant,
    ) -> EventResult {
        let start = Instant::now();
        if matches!(
            event.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) && self.area.contains(event.column, event.row)
        {
            let lines = cx.editor.config().scroll_lines.unsigned_abs().max(1);
            let delta = match event.kind {
                MouseEventKind::ScrollUp => -(lines as isize),
                MouseEventKind::ScrollDown => lines as isize,
                _ => unreachable!(),
            };
            self.move_selection_by(delta);
            self.queue_selected_preview(cx.editor, cx.ingress.clone());
            log::info!(
                "[file_explorer] mouse_scroll delta={} scroll={} selection={} selected={} elapsed_us={}",
                delta,
                self.scroll,
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
                start.elapsed().as_micros()
            );
            return EventResult::Consumed(None);
        }

        if !matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
            return EventResult::Ignored(None);
        }
        if !self.area.contains(event.column, event.row) {
            return EventResult::Ignored(None);
        }

        if let Some(index) = self.row_index_at_mouse(event) {
            let path = self.rows[index].path.clone();
            let double_click = self.is_double_click(&path, now);
            self.seek_to(index);
            self.clamp_label_selection();
            self.collapse_label_selection_to_cursor();

            if double_click {
                self.last_click = None;
                self.cancel_preview_request();
                self.open_selected(cx, Action::Replace);
            } else {
                self.last_click = Some(ExplorerClick { path, at: now });
                self.queue_selected_preview(cx.editor, cx.ingress.clone());
            }
            log::info!(
                "[file_explorer] mouse_select row={} index={} double_click={} selected={} elapsed_us={}",
                event.row,
                index,
                double_click,
                selected_path_for_log(&self.rows, self.selection),
                start.elapsed().as_micros()
            );
        } else {
            log::info!(
                "[file_explorer] mouse_empty row={} col={} scroll={} rows={} elapsed_us={}",
                event.row,
                event.column,
                self.scroll,
                self.rows.len(),
                start.elapsed().as_micros()
            );
        }
        EventResult::Consumed(None)
    }

    fn handle_mouse(&mut self, event: &MouseEvent, cx: &mut Context) -> EventResult {
        self.handle_mouse_at(event, cx, Instant::now())
    }

    fn row_icon_width(&self, row: &ExplorerRow) -> u16 {
        let icons = ICONS.load();
        if row.is_dir {
            if let Some(icon) = if row.expanded {
                icons.kind().folder_open()
            } else {
                icons.kind().folder()
            } {
                return icon_width(&icon);
            }

            if let Some(icon) = if row.expanded {
                icons.mime().directory_open()
            } else {
                icons.mime().directory()
            } {
                return text_width(icon).saturating_add(2);
            }

            let fallback = if row.expanded {
                FALLBACK_FOLDER_OPEN_ICON
            } else {
                FALLBACK_FOLDER_ICON
            };
            return text_width(fallback).saturating_add(2);
        }

        if let Some(icon) = icons
            .mime()
            .get(Some(&row.path), None)
            .or_else(|| icons.mime().get_or_default(Some(&row.path), None))
        {
            icon_width(icon)
        } else {
            text_width(FALLBACK_FILE_ICON).saturating_add(2)
        }
    }

    fn row_label_offset(&self, row: &ExplorerRow, show_icons: bool) -> u16 {
        let icon_width = if show_icons {
            self.row_icon_width(row)
        } else {
            0
        };
        crate::widgets::tree_list_label_offset(row.ancestor_last.len(), row.depth, icon_width)
    }

    fn cursor_position(&self, area: Rect, editor: &Editor) -> Option<Position> {
        if !self.focused || self.rows.is_empty() {
            return None;
        }

        let list = Self::list_area(area)?;
        if self.selection < self.scroll {
            return None;
        }

        let screen_row = self.selection - self.scroll;
        if screen_row >= list.height as usize {
            return None;
        }

        let row = self.rows.get(self.selection)?;
        let status_width = row_status_width(row);
        let content_width = list.width.saturating_sub(status_width);
        let label_offset = self.row_label_offset(row, editor.config().file_explorer.icons);
        if label_offset >= content_width {
            return None;
        }
        // Use the displayed label length (buffer when editing, row.label
        // otherwise) so the cursor lands at the right column.
        let display_label = self.display_label_for(row, self.selection);
        let label_cursor: u16 = self
            .label_cursor()
            .min(display_label.chars().count())
            .try_into()
            .unwrap_or(u16::MAX);
        let label_cursor = label_cursor.min(content_width.saturating_sub(label_offset + 1));

        Some(Position::new(
            list.y.saturating_add(screen_row as u16) as usize,
            list.x
                .saturating_add(label_offset)
                .saturating_add(label_cursor) as usize,
        ))
    }
}

impl Focusable for FileExplorerPanel {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl Bounded for FileExplorerPanel {
    fn area(&self) -> Rect {
        self.area
    }

    fn set_area(&mut self, area: Rect) {
        self.area = area;
    }
}

impl Scrollable for FileExplorerPanel {
    fn scroll(&self) -> usize {
        self.scroll
    }

    fn scroll_to(&mut self, offset: usize) {
        // Pure scroll: viewport moves, selection doesn't. Used by
        // mouse-wheel handlers. Goes through `nav.set_scroll` (which
        // clamps internally) so the field mirror stays in sync.
        self.nav.set_item_count(self.rows.len());
        self.nav.set_viewport_height(self.visible_height());
        self.nav.set_scroll(offset);
        self.scroll = self.nav.scroll();
    }

    fn content_height(&self) -> usize {
        self.rows.len()
    }
}

impl Component for FileExplorerPanel {
    fn sync(&mut self, editor: &mut Editor) {
        let start = Instant::now();
        if self.refresh_diagnostic_snapshot(editor) {
            let selection = Some(self.selection);
            if let Err(err) = self.refresh_preserving_tree(editor, None, selection) {
                log::error!("failed to refresh file explorer diagnostics: {err}");
            }
        }
        self.sync_to_model(editor);
        log::info!(
            "[file_explorer] sync rows={} selection={} selected={} focused={} preview={:?} focused_view={:?} focused_doc={:?} documents={} component_documents={} diagnostic_entries={} elapsed_us={}",
            self.rows.len(),
            self.selection,
            selected_path_for_log(&self.rows, self.selection),
            self.focused,
            self.preview,
            editor.focused_view_id(),
            editor.focused_document_id(),
            editor.document_count(),
            editor.component_docs.len(),
            self.diagnostic_snapshot.len(),
            start.elapsed().as_micros()
        );
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if !self.focused {
            return EventResult::Ignored(None);
        }

        match event {
            Event::Key(key) => self.handle_key(*key, cx),
            Event::Mouse(mouse) => self.handle_mouse(mouse, cx),
            Event::Resize(..) => EventResult::Consumed(None),
            Event::Paste(_) | Event::IdleTimeout | Event::FocusGained | Event::FocusLost => {
                EventResult::Ignored(None)
            }
        }
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let cursor_start = Instant::now();
        if let Some(position) = self.cursor_position(area, editor) {
            // While editing a label, mirror the editor's configured cursor
            // shape for the explorer's own input mode (Insert → bar, etc.)
            // so renaming feels like editing a normal buffer. Outside of
            // edit mode the cursor is a visual marker for the selected
            // row — keep it as Block.
            let kind = if self.label_edit.is_some() {
                editor.config().cursor_shape.from_mode(self.input.mode)
            } else {
                CursorKind::Block
            };
            log::info!(
                "[file_explorer] cursor pos={},{} kind={:?} area={}x{}+{},{} selection={} selected={} focused={} preview={:?} label_cursor={} input_mode={:?} editing={} documents={} elapsed_us={}",
                position.col,
                position.row,
                kind,
                area.width,
                area.height,
                area.x,
                area.y,
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
                self.focused,
                self.preview,
                self.label_cursor(),
                self.input.mode,
                self.label_edit.is_some(),
                editor.document_count(),
                cursor_start.elapsed().as_micros(),
            );
            return (Some(position), kind);
        }

        if self.focused {
            log::info!(
                "[file_explorer] cursor pos={},{} kind={:?} reason=focused_without_visible_label area={}x{}+{},{} selection={} selected={} preview={:?} documents={} elapsed_us={}",
                area.x,
                area.y,
                CursorKind::Hidden,
                area.width,
                area.height,
                area.x,
                area.y,
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
                self.preview,
                editor.document_count(),
                cursor_start.elapsed().as_micros(),
            );
            return (
                Some(Position::new(area.y as usize, area.x as usize)),
                CursorKind::Hidden,
            );
        }

        log::info!(
            "[file_explorer] cursor pos=<none> kind={:?} reason=not_focused area={}x{}+{},{} selection={} selected={} preview={:?} documents={} elapsed_us={}",
            CursorKind::Hidden,
            area.width,
            area.height,
            area.x,
            area.y,
            self.selection,
            selected_path_for_log(&self.rows, self.selection),
            self.preview,
            editor.document_count(),
            cursor_start.elapsed().as_micros(),
        );
        (None, CursorKind::Hidden)
    }

    fn render(&mut self, area: Rect, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        self.render_surface(area, surface, cx);
    }

    fn id(&self) -> Option<&str> {
        Some(ID)
    }

    fn layout_role(&self) -> crate::compositor::LayoutRole {
        crate::compositor::LayoutRole::Docked
    }

    fn panel_id(&self) -> Option<PanelId> {
        self.model_panel_id
    }

    component_traits!(focusable, scrollable);
}

#[cfg(test)]
mod tests {
    use super::path_ops::{display_name, relative_display, sibling_path_with_label};
    use super::*;
    use crate::test_support::fs::TempFs;
    use crate::{alt, ctrl, key};
    use arc_swap::ArcSwap;
    use helix_core::{
        diagnostic::{DiagnosticProvider, LanguageServerId, Severity as DiagnosticSeverity},
        movement::Direction,
        Uri,
    };
    use helix_lsp::lsp::{self, DiagnosticSeverity as LspDiagnosticSeverity};
    use helix_vcs::FileChange;
    use helix_view::{
        document::Mode,
        editor::EditingEngineConfig,
        engine::ModalInputState,
        keymap::{ModalIntent, ModalIntentTrie},
        model::{PanelSide, TreePanelModel},
        theme::Style,
    };
    use std::{fs, sync::Arc};
    use tui::ratatui::{buffer::Buffer, layout::Rect as RatatuiRect};

    fn test_editor(width: u16, height: u16, runtime: helix_runtime::Runtime) -> Editor {
        test_editor_with_engine(width, height, runtime, EditingEngineConfig::Helix)
    }

    fn test_editor_with_engine(
        width: u16,
        height: u16,
        runtime: helix_runtime::Runtime,
        editing_engine: EditingEngineConfig,
    ) -> Editor {
        let theme_loader = helix_view::theme::Loader::new(helix_loader::runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = helix_view::editor::Config {
            editing_engine,
            ..Default::default()
        };
        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = helix_view::handlers::Handlers::dummy();
        let mut editor = Editor::new(
            Rect::new(0, 0, width, height),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(
                config,
                |c: &helix_view::editor::Config| c,
            )),
            runtime,
            handlers,
        );
        editor.frontend_mut().modal_keymaps = Arc::new(ArcSwap::from_pointee(
            crate::keymap::to_component_modal_keymaps(&crate::keymap::default()),
        ));
        editor.frontend_mut().semantic_modal_keymaps = Arc::new(ArcSwap::from_pointee(
            crate::keymap::to_semantic_modal_keymaps(&crate::keymap::default()),
        ));
        std::sync::Arc::new(helix_modal::ModalEngineFactory::default()).install(&mut editor);
        editor.new_file(Action::VerticalSplit);
        editor
    }

    fn with_context<R>(
        editor: &mut Editor,
        runtime: &helix_runtime::test::RuntimeTest,
        f: impl FnOnce(&mut Context<'_>) -> R,
    ) -> R {
        let (ingress, _ingress_rx) =
            crate::runtime::RuntimeIngress::channel(runtime.runtime().work().clone());
        let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
        let idle_reset = crate::runtime::IdleResetGate::new().handle();
        let mut exit_tasks = crate::runtime::ExitTaskSet::default();
        let exit_task_work = editor.work();
        let redraw = editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events,
        };
        let mut cx = Context::new(
            editor,
            &mut exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            idle_reset,
            None,
        );
        f(&mut cx)
    }

    fn row_index_by_name(panel: &FileExplorerPanel, name: &str) -> usize {
        panel
            .rows
            .iter()
            .position(|row| display_name(&row.path) == name)
            .unwrap_or_else(|| panic!("row not found: {name}"))
    }

    fn mouse_down(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: helix_view::input::KeyModifiers::NONE,
        }
    }

    fn press_key(
        panel: &mut FileExplorerPanel,
        editor: &mut Editor,
        runtime: &helix_runtime::test::RuntimeTest,
        key: KeyEvent,
    ) {
        with_context(editor, runtime, |cx| {
            assert!(matches!(
                panel.handle_event(&Event::Key(key), cx),
                EventResult::Consumed(_)
            ));
        });
    }

    fn redo_key(editing_engine: EditingEngineConfig) -> KeyEvent {
        match editing_engine {
            EditingEngineConfig::Helix => key!('U'),
            EditingEngineConfig::Vim => ctrl!('r'),
        }
    }

    fn lsp_diagnostic(severity: LspDiagnosticSeverity) -> lsp::Diagnostic {
        lsp::Diagnostic {
            range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 1)),
            severity: Some(severity),
            code: None,
            code_description: None,
            source: Some("test".to_string()),
            message: "diagnostic".to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn diagnostic_provider() -> DiagnosticProvider {
        DiagnosticProvider::Lsp {
            server_id: LanguageServerId::default(),
            identifier: Some(Arc::<str>::from("test")),
        }
    }

    fn add_diagnostic(editor: &mut Editor, path: &Path, severity: LspDiagnosticSeverity) {
        let path = helix_stdx::path::normalize(path);
        editor
            .diagnostics
            .entry(Uri::from(path))
            .or_default()
            .push((lsp_diagnostic(severity), diagnostic_provider()));
    }

    fn row_tree_item_styles(
        label_selection: Option<std::ops::Range<usize>>,
        show_icons: bool,
    ) -> ExplorerTreeItemStyles {
        ExplorerTreeItemStyles {
            base: Style::default(),
            directory: Style::default(),
            label_selection,
            show_icons,
        }
    }

    fn empty_status_styles() -> ExplorerStatusStyles {
        ExplorerStatusStyles {
            added: Style::default(),
            modified: Style::default(),
            deleted: Style::default(),
            renamed: Style::default(),
            conflict: Style::default(),
            diagnostic_hint: Style::default(),
            diagnostic_info: Style::default(),
            diagnostic_warning: Style::default(),
            diagnostic_error: Style::default(),
        }
    }

    fn render_tree_row(
        panel: &FileExplorerPanel,
        row: &ExplorerRow,
        styles: ExplorerTreeItemStyles,
        selection: Style,
    ) -> (Buffer, String) {
        let icons = ICONS.load();
        let item = panel.tree_item(row, 0, true, false, styles, &icons, empty_status_styles());
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 80, 1));
        crate::widgets::tree_list(
            &mut surface,
            Rect::new(0, 0, 80, 1),
            &[item],
            crate::widgets::TreeListStyles {
                selection,
                ..crate::widgets::TreeListStyles::default()
            },
            None,
        );
        let mut text = String::new();
        for x in 0..80 {
            text.push_str(surface[(x, 0)].symbol());
        }
        (surface, text)
    }

    #[test]
    fn modal_engine_resolves_go_prefix_commands() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert!(matches!(
            input.translate(key!('g')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('e')),
            ExplorerInput::Execute(ExplorerAction::SelectLast)
        );

        assert!(matches!(
            input.translate(key!('g')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('g')),
            ExplorerInput::Execute(ExplorerAction::SelectFirst)
        );
    }

    #[test]
    fn modal_engine_uses_editor_open_line_keys_for_create() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        // `o` / `O` now start an inline-edit create (buffer + cursor in the
        // row), not the legacy bottom-of-screen prompt.
        assert_eq!(
            input.translate(key!('o')),
            ExplorerInput::Execute(ExplorerAction::EnterCreate)
        );
        assert_eq!(
            input.translate(key!('O')),
            ExplorerInput::Execute(ExplorerAction::EnterCreate)
        );
        input.prepare_test_keymaps(EditingEngineConfig::Vim);
        assert_eq!(
            input.translate(key!('o')),
            ExplorerInput::Execute(ExplorerAction::EnterCreate)
        );
        assert_eq!(
            input.translate(key!('n')),
            ExplorerInput::Execute(ExplorerAction::Noop)
        );
    }

    #[test]
    fn explorer_local_keymap_uses_component_intents() {
        let local = explorer_local_keymap(EditingEngineConfig::Helix);
        let Some(ModalIntentTrie::Binding(binding)) = local.search(&[key!('?')]) else {
            panic!("local explorer help binding should exist");
        };
        assert!(matches!(binding.intent(), ModalIntent::Component(_)));
    }

    #[test]
    fn explorer_local_keymap_has_list_navigation_aliases() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(ctrl!('p')),
            ExplorerInput::Execute(ExplorerAction::MoveSelection(-1))
        );
        assert_eq!(
            input.translate(ctrl!('n')),
            ExplorerInput::Execute(ExplorerAction::MoveSelection(1))
        );
        assert_eq!(
            input.translate(key!(Tab)),
            ExplorerInput::Execute(ExplorerAction::ToggleDirectory)
        );
    }

    #[test]
    fn modal_engine_help_uses_explorer_keymap() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(key!('?')),
            ExplorerInput::Execute(ExplorerAction::ShowHelp)
        );

        let info = input.root_infobox().expect("explorer keymap has help");
        assert_eq!(info.title.as_ref(), "File Explorer");
        assert!(info.text.contains("Show explorer key bindings"));
        // `gw → Go to workspace root` used to be in the local keymap
        // but was removed because it shadowed Helix's `goto_word`
        // binding in a confusing way (it looked like `gw` was sending
        // the cursor "to root"). If you want to restore the
        // workspace-root operation, bind it to a key that doesn't
        // overlap with a default editor command and adjust this test
        // to assert its new label.
    }

    #[test]
    fn explorer_undo_uses_editor_semantic_binding() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(key!('u')),
            ExplorerInput::Execute(ExplorerAction::UndoFileOperation)
        );
        assert_eq!(
            input.translate(key!('r')),
            ExplorerInput::Execute(ExplorerAction::Noop)
        );
        assert_eq!(
            input.translate(key!('U')),
            ExplorerInput::Execute(ExplorerAction::RedoFileOperation)
        );
        input.prepare_test_keymaps(EditingEngineConfig::Vim);
        assert_eq!(
            input.translate(ctrl!('r')),
            ExplorerInput::Execute(ExplorerAction::RedoFileOperation)
        );
    }

    #[test]
    fn modal_engine_uses_editor_diagnostic_navigation_bindings() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert!(matches!(
            input.translate(key!(']')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('d')),
            ExplorerInput::Execute(ExplorerAction::SelectNextDiagnostic)
        );

        assert!(matches!(
            input.translate(key!('[')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('d')),
            ExplorerInput::Execute(ExplorerAction::SelectPreviousDiagnostic)
        );
    }

    #[test]
    fn modal_engine_applies_count_to_row_movement() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(input.translate(key!('5')), ExplorerInput::Pending(None));
        assert_eq!(
            input.modal_input_state().count.map(NonZeroUsize::get),
            Some(5)
        );
        assert_eq!(
            input.translate(key!('j')),
            ExplorerInput::Execute(ExplorerAction::MoveSelection(5))
        );
        input.finish_command();
        assert_eq!(input.modal_input_state(), ModalInputState::default());
    }

    #[test]
    fn modal_engine_maps_editor_word_and_char_motions_to_label_selection() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(key!('l')),
            ExplorerInput::Execute(ExplorerAction::MoveLabelSelection(
                LabelMotion::Char(1),
                CoreMovement::Move
            ))
        );
        assert_eq!(
            input.translate(key!('h')),
            ExplorerInput::Execute(ExplorerAction::MoveLabelSelection(
                LabelMotion::Char(-1),
                CoreMovement::Move
            ))
        );
        assert_eq!(
            input.translate(key!('w')),
            ExplorerInput::Execute(ExplorerAction::MoveLabelSelection(
                LabelMotion::NextWordStart(1),
                CoreMovement::Move
            ))
        );
        assert_eq!(
            input.translate(key!('b')),
            ExplorerInput::Execute(ExplorerAction::MoveLabelSelection(
                LabelMotion::PrevWordStart(1),
                CoreMovement::Move
            ))
        );
        assert_eq!(
            input.translate(key!('e')),
            ExplorerInput::Execute(ExplorerAction::MoveLabelSelection(
                LabelMotion::NextWordEnd(1),
                CoreMovement::Move
            ))
        );
    }

    #[test]
    fn modal_engine_maps_editor_find_char_fallbacks_to_label_selection() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert!(matches!(
            input.translate(key!('f')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('a')),
            ExplorerInput::Execute(ExplorerAction::MoveLabelSelection(
                LabelMotion::FindChar {
                    ch: 'a',
                    direction: Direction::Forward,
                    inclusive: true,
                    count: 1,
                },
                CoreMovement::Move,
            ))
        );

        input.finish_command();
        assert!(matches!(
            input.translate(key!('T')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('.')),
            ExplorerInput::Execute(ExplorerAction::MoveLabelSelection(
                LabelMotion::FindChar {
                    ch: '.',
                    direction: Direction::Backward,
                    inclusive: false,
                    count: 1,
                },
                CoreMovement::Move,
            ))
        );
    }

    #[test]
    fn modal_engine_select_mode_uses_extend_motions_for_label_selection() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(key!('v')),
            ExplorerInput::Execute(ExplorerAction::SetMode(Mode::Select))
        );
        input.mode = Mode::Select;
        assert_eq!(
            input.translate(key!('w')),
            ExplorerInput::Execute(ExplorerAction::MoveLabelSelection(
                LabelMotion::NextWordStart(1),
                CoreMovement::Extend
            ))
        );
        assert_eq!(
            input.translate(key!(Esc)),
            ExplorerInput::Execute(ExplorerAction::SetMode(Mode::Normal))
        );
    }

    #[test]
    fn modal_engine_uses_editor_text_objects_for_label_selection() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert!(matches!(
            input.translate(key!('m')),
            ExplorerInput::Pending(Some(_))
        ));
        assert!(matches!(
            input.translate(key!('i')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('w')),
            ExplorerInput::Execute(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::InsideWord
            ))
        );

        input.finish_command();
        assert!(matches!(
            input.translate(key!('m')),
            ExplorerInput::Pending(Some(_))
        ));
        assert!(matches!(
            input.translate(key!('a')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('p')),
            ExplorerInput::Execute(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::AroundParagraph
            ))
        );

        input.finish_command();
        assert!(matches!(
            input.translate(key!('m')),
            ExplorerInput::Pending(Some(_))
        ));
        assert!(matches!(
            input.translate(key!('i')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('(')),
            ExplorerInput::Execute(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::InsideSurroundingPair('(')
            ))
        );
    }

    #[test]
    fn helix_modal_engine_deletes_current_label_selection() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(key!('d')),
            ExplorerInput::Execute(ExplorerAction::DeleteLabelSelection { yank: true })
        );
        input.finish_command();
        assert_eq!(
            input.translate(alt!('d')),
            ExplorerInput::Execute(ExplorerAction::DeleteLabelSelection { yank: false })
        );
    }

    #[test]
    fn helix_modal_engine_selects_whole_label_with_line_selection() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(key!('x')),
            ExplorerInput::Execute(ExplorerAction::SelectWholeLabel)
        );
        input.finish_command();
        assert_eq!(
            input.translate(key!('X')),
            ExplorerInput::Execute(ExplorerAction::SelectWholeLabel)
        );
        input.finish_command();
        assert_eq!(
            input.translate(key!('%')),
            ExplorerInput::Execute(ExplorerAction::SelectWholeLabel)
        );
        input.finish_command();
        assert_eq!(
            input.translate(key!(';')),
            ExplorerInput::Execute(ExplorerAction::CollapseLabelSelection)
        );
        input.finish_command();
        assert_eq!(
            input.translate(alt!(';')),
            ExplorerInput::Execute(ExplorerAction::FlipLabelSelection)
        );
    }

    #[test]
    fn helix_modal_engine_applies_change_after_text_object_selection() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert!(matches!(
            input.translate(key!('m')),
            ExplorerInput::Pending(Some(_))
        ));
        assert!(matches!(
            input.translate(key!('i')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('w')),
            ExplorerInput::Execute(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::InsideWord
            ))
        );
        input.finish_command();
        assert_eq!(
            input.translate(key!('c')),
            ExplorerInput::Execute(ExplorerAction::ChangeLabelSelection { yank: true })
        );
    }

    #[test]
    fn modal_engine_uses_editor_paste_keys_for_file_paste() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(key!('p')),
            ExplorerInput::Execute(ExplorerAction::PasteClipboard(
                ExplorerPastePlacement::After
            ))
        );
        input.finish_command();
        assert_eq!(
            input.translate(key!('P')),
            ExplorerInput::Execute(ExplorerAction::PasteClipboard(
                ExplorerPastePlacement::Before
            ))
        );
    }

    #[test]
    fn modal_engine_tracks_register_for_yank_path() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(input.translate(key!('"')), ExplorerInput::Pending(None));
        assert_eq!(input.translate(key!('a')), ExplorerInput::Pending(None));
        assert_eq!(input.modal_input_state().selected_register, Some('a'));
        assert_eq!(
            input.translate(key!('y')),
            ExplorerInput::Execute(ExplorerAction::ClipboardOperation(
                ExplorerFileOperation::Copy
            ))
        );
    }

    #[test]
    fn modal_engine_delegates_command_mode_to_editor() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(key!(':')),
            ExplorerInput::Execute(ExplorerAction::DelegateToEditor)
        );
    }

    #[test]
    fn vim_modal_engine_uses_doubled_operator_for_yank_path() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Vim);

        assert_eq!(input.translate(key!('y')), ExplorerInput::Pending(None));
        assert_eq!(
            input.translate(key!('y')),
            ExplorerInput::Execute(ExplorerAction::ClipboardOperation(
                ExplorerFileOperation::Copy
            ))
        );
    }

    #[test]
    fn vim_modal_engine_uses_doubled_delete_for_whole_item_delete() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Vim);

        assert_eq!(input.translate(key!('d')), ExplorerInput::Pending(None));
        assert_eq!(
            input.translate(key!('d')),
            ExplorerInput::Execute(ExplorerAction::DeleteSelectedItem { yank: true })
        );
    }

    #[test]
    fn vim_modal_engine_applies_operator_to_motion_and_text_object() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Vim);

        assert_eq!(input.translate(key!('c')), ExplorerInput::Pending(None));
        assert_eq!(
            input.translate(key!('w')),
            ExplorerInput::Execute(ExplorerAction::ApplyOperatorMotion(
                ExplorerOperator::Change { yank: true },
                LabelMotion::NextWordStart(1)
            ))
        );

        input.finish_command();
        assert_eq!(input.translate(key!('d')), ExplorerInput::Pending(None));
        assert_eq!(input.translate(key!('i')), ExplorerInput::Pending(None));
        assert_eq!(
            input.translate(key!('w')),
            ExplorerInput::Execute(ExplorerAction::ApplyOperatorTextObject(
                ExplorerOperator::Delete { yank: true },
                LabelTextObject::InsideWord
            ))
        );

        input.finish_command();
        assert_eq!(input.translate(key!('d')), ExplorerInput::Pending(None));
        assert!(matches!(
            input.translate(key!('f')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('.')),
            ExplorerInput::Execute(ExplorerAction::ApplyOperatorMotion(
                ExplorerOperator::Delete { yank: true },
                LabelMotion::FindChar {
                    ch: '.',
                    direction: Direction::Forward,
                    inclusive: true,
                    count: 1,
                }
            ))
        );

        input.finish_command();
        assert_eq!(input.translate(key!('c')), ExplorerInput::Pending(None));
        assert_eq!(input.translate(key!('i')), ExplorerInput::Pending(None));
        assert_eq!(
            input.translate(key!('(')),
            ExplorerInput::Execute(ExplorerAction::ApplyOperatorTextObject(
                ExplorerOperator::Change { yank: true },
                LabelTextObject::InsideSurroundingPair('(')
            ))
        );
    }

    #[test]
    fn explorer_modal_filesystem_scenario_helix() {
        run_explorer_modal_filesystem_scenario(EditingEngineConfig::Helix);
    }

    #[test]
    fn explorer_modal_filesystem_scenario_vim() {
        run_explorer_modal_filesystem_scenario(EditingEngineConfig::Vim);
    }

    fn run_explorer_modal_filesystem_scenario(editing_engine: EditingEngineConfig) {
        let fs = TempFs::new();
        fs.dir("docs")
            .dir("src")
            .file("alpha-beta.rs", "alpha")
            .file("src/main.rs", "main");
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor_with_engine(100, 30, rt.runtime(), editing_engine);
            let mut panel = FileExplorerPanel::new(fs.root().to_path_buf(), &editor).unwrap();
            panel.area = Rect::new(0, 0, 40, 12);

            assert_eq!(
                panel.selected().map(|row| row.path.as_path()),
                Some(fs.root())
            );
            press_key(&mut panel, &mut editor, &rt, key!('j'));
            assert_eq!(display_name(&panel.rows[panel.selection].path), "docs");

            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");
            panel.label_selection = LabelSelection::point(0);
            let label = "alpha-beta.rs";

            let mut expected = LabelSelection::point(0).apply_motion(
                label,
                LabelMotion::NextWordStart(1),
                CoreMovement::Move,
            );
            press_key(&mut panel, &mut editor, &rt, key!('w'));
            assert_eq!(panel.label_cursor(), expected.cursor(label));
            assert_eq!(panel.label_selection, expected);

            expected =
                expected.apply_motion(label, LabelMotion::NextWordEnd(1), CoreMovement::Move);
            press_key(&mut panel, &mut editor, &rt, key!('e'));
            assert_eq!(panel.label_cursor(), expected.cursor(label));
            assert_eq!(panel.label_selection, expected);

            expected =
                expected.apply_motion(label, LabelMotion::PrevWordStart(1), CoreMovement::Move);
            press_key(&mut panel, &mut editor, &rt, key!('b'));
            assert_eq!(panel.label_cursor(), expected.cursor(label));
            assert_eq!(panel.label_selection, expected);

            press_key(&mut panel, &mut editor, &rt, key!('m'));
            press_key(&mut panel, &mut editor, &rt, key!('i'));
            press_key(&mut panel, &mut editor, &rt, key!('w'));
            assert_eq!(panel.label_selection.span(label), Some(0..5));

            press_key(&mut panel, &mut editor, &rt, key!('"'));
            press_key(&mut panel, &mut editor, &rt, key!('a'));
            press_key(&mut panel, &mut editor, &rt, key!('y'));
            if editing_engine == EditingEngineConfig::Vim {
                press_key(&mut panel, &mut editor, &rt, key!('y'));
            }
            let yanked = editor
                .registers
                .read('a', &editor)
                .expect("register a should contain yanked path")
                .map(|value| value.into_owned())
                .collect::<Vec<_>>();
            assert_eq!(yanked.len(), 1);
            assert!(
                yanked[0].ends_with("alpha-beta.rs"),
                "unexpected yanked path: {:?}",
                yanked
            );

            panel.selection = row_index_by_name(&panel, "docs");
            press_key(&mut panel, &mut editor, &rt, key!('p'));
            fs.assert_exists("docs/alpha-beta.rs");

            editor
                .create_path_with_history(&fs.path("created.rs"), false)
                .unwrap();
            panel.refresh_current(&editor);
            fs.assert_exists("created.rs");
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "created.rs"));

            editor
                .move_path_with_history(&fs.path("created.rs"), &fs.path("docs/moved.rs"))
                .unwrap();
            panel.refresh_current(&editor);
            fs.assert_missing("created.rs");
            fs.assert_exists("docs/moved.rs");
            panel.selection = row_index_by_name(&panel, "docs");
            panel.toggle_selected_dir(&editor);
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "moved.rs"));

            editor
                .copy_path_with_history(&fs.path("docs/moved.rs"), &fs.path("docs/copy.rs"))
                .unwrap();
            panel.refresh_current(&editor);
            fs.assert_exists("docs/copy.rs");
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "copy.rs"));

            press_key(&mut panel, &mut editor, &rt, key!('u'));
            fs.assert_missing("docs/copy.rs");
            assert!(!panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "copy.rs"));

            press_key(&mut panel, &mut editor, &rt, redo_key(editing_engine));
            fs.assert_exists("docs/copy.rs");
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "copy.rs"));
        });
    }

    #[test]
    fn panel_builds_sorted_directory_tree() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("README.md"), "").unwrap();
        fs::write(temp.path().join("src").join("main.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());

            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            assert_eq!(panel.rows.len(), 3);
            assert_eq!(display_name(&panel.rows[0].path), display_name(temp.path()));
            assert!(panel.rows[0].is_dir);
            assert!(panel.rows[0].expanded);
            assert_eq!(display_name(&panel.rows[1].path), "src");
            assert!(panel.rows[1].is_dir);
            assert_eq!(display_name(&panel.rows[2].path), "README.md");

            panel.selection = 1;
            panel.toggle_selected_dir(&editor);
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "main.rs" && row.depth == 2));
        });
    }

    #[test]
    fn refresh_selecting_path_follows_renamed_file() {
        let temp = tempfile::tempdir().unwrap();
        let alpha = temp.path().join("alpha.rs");
        let omega = temp.path().join("omega.rs");
        fs::write(&alpha, "").unwrap();
        fs::write(temp.path().join("middle.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            fs::rename(&alpha, &omega).unwrap();
            panel
                .refresh_selecting_path(&editor, None, &omega, panel.selection)
                .unwrap();

            assert_eq!(display_name(&panel.rows[panel.selection].path), "omega.rs");
        });
    }

    #[test]
    fn scratch_preview_keeps_explorer_focused() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main.rs");
        fs::write(&main, "fn main() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "main.rs");

            panel.preview_selected_file(&mut editor);

            assert_eq!(
                editor
                    .focused_document()
                    .and_then(|doc| doc.path())
                    .map(PathBuf::as_path),
                Some(main.as_path())
            );
            assert!(editor
                .focused_document()
                .is_some_and(|doc| doc.is_preview()));
            assert!(panel.focused);
        });
    }

    #[test]
    fn preview_selected_file_reuses_single_preview_document() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("alpha.rs");
        let second = temp.path().join("beta.rs");
        fs::write(&first, "fn alpha() {}\n").unwrap();
        fs::write(&second, "fn beta() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            panel.selection = row_index_by_name(&panel, "alpha.rs");
            panel.preview_selected_file(&mut editor);
            let first_doc = editor.focused_document_id();
            assert_eq!(editor.document_count(), 1);
            assert!(editor.contains_document(first_doc));
            assert!(editor
                .document(first_doc)
                .is_some_and(|doc| doc.is_preview()));
            assert!(matches!(panel.preview, ExplorerPreview::Owned(id) if id == first_doc));

            panel.selection = row_index_by_name(&panel, "beta.rs");
            panel.preview_selected_file(&mut editor);
            let second_doc = editor.focused_document_id();

            assert_ne!(first_doc, second_doc);
            assert_eq!(editor.document_count(), 1);
            assert!(!editor.contains_document(first_doc));
            assert!(panel.preview_cache.contains_path(&first));
            assert!(editor.contains_document(second_doc));
            assert_eq!(
                editor
                    .focused_document()
                    .and_then(|doc| doc.path())
                    .map(PathBuf::as_path),
                Some(second.as_path())
            );
            assert!(editor
                .document(second_doc)
                .is_some_and(|doc| doc.is_preview()));
            assert!(matches!(panel.preview, ExplorerPreview::Owned(id) if id == second_doc));
            assert!(panel.focused);

            panel.selection = row_index_by_name(&panel, "alpha.rs");
            panel.preview_selected_file(&mut editor);
            let restored_first_doc = editor.focused_document_id();

            assert_eq!(first_doc, restored_first_doc);
            assert_eq!(editor.document_count(), 1);
            assert!(!editor.contains_document(second_doc));
            assert!(panel.preview_cache.contains_path(&second));
            assert!(!panel.preview_cache.contains_path(&first));
            assert_eq!(
                editor
                    .focused_document()
                    .and_then(|doc| doc.path())
                    .map(PathBuf::as_path),
                Some(first.as_path())
            );
            assert!(editor
                .document(restored_first_doc)
                .is_some_and(|doc| doc.is_preview()));
            assert!(
                matches!(panel.preview, ExplorerPreview::Owned(id) if id == restored_first_doc)
            );
        });
    }

    #[test]
    fn queued_preview_fires_synchronously_for_each_selection() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("alpha.rs");
        let second = temp.path().join("beta.rs");
        fs::write(&first, "fn alpha() {}\n").unwrap();
        fs::write(&second, "fn beta() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::new_paused();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let (ingress, mut rx) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().work().clone());

            // Each `queue_selected_preview` now sends immediately — no
            // debounce/coalesce. Staleness is filtered on the receiving
            // side (`apply_preview_request`) against the panel's current
            // selection, so the user-visible behaviour is still "only the
            // latest preview opens" — but it opens with zero added latency.
            panel.selection = row_index_by_name(&panel, "alpha.rs");
            panel.queue_selected_preview(&editor, ingress.clone());
            panel.selection = row_index_by_name(&panel, "beta.rs");
            panel.queue_selected_preview(&editor, ingress);

            tokio::task::yield_now().await;

            let mut seen = Vec::new();
            while let Ok(delivery) = rx.try_recv() {
                if let crate::runtime::ingress::RuntimeDelivery::Ui(UiCommand::FileExplorer(
                    FileExplorerCommand::PreviewSelection { path, cursor, .. },
                )) = delivery
                {
                    seen.push((helix_stdx::path::normalize(path), cursor));
                }
            }
            assert_eq!(
                seen,
                vec![
                    (
                        helix_stdx::path::normalize(&first),
                        u32::try_from(row_index_by_name(&panel, "alpha.rs")).unwrap()
                    ),
                    (
                        helix_stdx::path::normalize(&second),
                        u32::try_from(row_index_by_name(&panel, "beta.rs")).unwrap()
                    ),
                ],
                "both preview requests should fire in order; staleness is filtered on the receiving side"
            );
        });
    }

    #[test]
    fn stale_preview_request_does_not_open_document() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("alpha.rs");
        let second = temp.path().join("beta.rs");
        fs::write(&first, "fn alpha() {}\n").unwrap();
        fs::write(&second, "fn beta() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let (ingress, _ingress_rx) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().work().clone());
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            panel.apply_preview_request(
                &mut editor,
                ingress,
                temp.path().to_path_buf(),
                second.clone(),
                selected_cursor(panel.selection),
            );

            assert_ne!(
                editor
                    .focused_document()
                    .and_then(|doc| doc.path())
                    .map(PathBuf::as_path),
                Some(second.as_path())
            );
        });
    }

    #[test]
    fn expand_collapse_reuses_cached_children_until_explicit_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("main.rs"), "").unwrap();
        fs::write(temp.path().join("README.md"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "src");
            panel.toggle_selected_dir(&editor);

            let src = helix_stdx::path::normalize(src);
            assert!(panel.children_cache.contains_key(&src));
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "main.rs"));
            let cached_directories = panel.children_cache.len();

            panel.toggle_selected_dir(&editor);
            assert!(!panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "main.rs"));
            panel.toggle_selected_dir(&editor);
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "main.rs"));
            assert_eq!(panel.children_cache.len(), cached_directories);

            fs::write(src.join("lib.rs"), "").unwrap();
            panel.refresh_current(&editor);
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "lib.rs"));
        });
    }

    #[test]
    fn expand_collapse_reuses_vcs_snapshot_until_explicit_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        let file = src.join("main.rs");
        fs::create_dir(&src).unwrap();
        fs::write(&file, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.vcs_snapshot = VcsSnapshot::from_changes(
                temp.path(),
                [FileChange::Modified { path: file.clone() }],
            );
            panel
                .refresh_preserving_tree(&editor, None, Some(panel.selection))
                .unwrap();

            let src_index = row_index_by_name(&panel, "src");
            assert_eq!(panel.rows[src_index].vcs_status, Some(VcsStatus::Modified));

            panel.selection = src_index;
            panel.toggle_selected_dir(&editor);
            let src_index = row_index_by_name(&panel, "src");
            assert_eq!(panel.rows[src_index].vcs_status, Some(VcsStatus::Modified));

            panel.refresh_current(&editor);
            let src_index = row_index_by_name(&panel, "src");
            assert_eq!(panel.rows[src_index].vcs_status, None);
        });
    }

    #[test]
    fn panel_follows_current_file_on_open() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src").join("nested")).unwrap();
        let current = temp.path().join("src").join("nested").join("main.rs");
        fs::write(&current, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            editor.open(&current, Action::Replace).unwrap();

            let panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "main.rs"));
            assert_eq!(
                panel.selected().map(|row| row.path.as_path()),
                Some(current.as_path())
            );
        });
    }

    #[test]
    fn collapsing_directory_is_recursive_and_does_not_refollow_current_file() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src").join("nested").join("deep")).unwrap();
        fs::write(temp.path().join("src").join("other.rs"), "").unwrap();
        fs::write(temp.path().join("src").join("nested").join("side.rs"), "").unwrap();
        let current = temp
            .path()
            .join("src")
            .join("nested")
            .join("deep")
            .join("main.rs");
        fs::write(&current, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            editor.open(&current, Action::Replace).unwrap();
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            assert!(panel.rows.iter().any(|row| row.path == current));
            panel.selection = row_index_by_name(&panel, "src");
            panel.collapse_or_select_parent(&editor);

            let src = helix_stdx::path::normalize(temp.path().join("src"));
            let nested = src.join("nested");
            let deep = nested.join("deep");
            assert!(panel
                .rows
                .iter()
                .any(|row| row.path == src && row.is_dir && !row.expanded));
            assert!(!panel.rows.iter().any(|row| row.path == nested));
            assert!(!panel.rows.iter().any(|row| row.path == deep));
            assert!(!panel.rows.iter().any(|row| row.path == current));
            assert!(!panel.expanded_dirs.contains(&src));
            assert!(panel.expanded_dirs.contains(&nested));
            assert!(panel.expanded_dirs.contains(&deep));

            panel.toggle_selected_dir(&editor);
            assert!(panel
                .rows
                .iter()
                .any(|row| row.path == nested && row.is_dir && row.expanded));
            assert!(panel
                .rows
                .iter()
                .any(|row| row.path == deep && row.is_dir && row.expanded));
            assert!(panel.rows.iter().any(|row| row.path == current));
        });
    }

    #[test]
    fn double_click_toggles_directory() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src").join("main.rs"), "").unwrap();
        fs::write(temp.path().join("README.md"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.area = Rect::new(0, 0, 40, 10);
            panel.selection = row_index_by_name(&panel, "src");
            assert!(!panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "main.rs"));

            let list = FileExplorerPanel::list_area(panel.area).unwrap();
            let click_row = list.y + (panel.selection - panel.scroll) as u16;
            let event = mouse_down(list.x, click_row);
            let first_click = Instant::now();

            let (ingress, _ingress_rx) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().work().clone());
            let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
            let idle_reset = crate::runtime::IdleResetGate::new().handle();
            let mut exit_tasks = crate::runtime::ExitTaskSet::default();
            let exit_task_work = editor.work();
            let redraw = editor.redraw_handle();
            let notifier = crate::handlers::local::Notifier {
                redraw: redraw.clone(),
                plugin_events,
            };
            let mut cx = Context::new(
                &mut editor,
                &mut exit_tasks,
                exit_task_work,
                notifier,
                ingress,
                idle_reset,
                None,
            );

            assert!(matches!(
                panel.handle_mouse_at(&event, &mut cx, first_click),
                EventResult::Consumed(None)
            ));
            assert!(matches!(
                panel.handle_mouse_at(&event, &mut cx, first_click + Duration::from_millis(100)),
                EventResult::Consumed(None)
            ));
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "main.rs"));
        });
    }

    #[test]
    fn focused_panel_cursor_tracks_selected_label() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src").join("main.rs"), "").unwrap();
        fs::write(temp.path().join("README.md"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let area = Rect::new(0, 0, 40, 10);
            panel.area = area;
            panel.selection = row_index_by_name(&panel, "src");
            panel.ensure_selection_visible();

            let list = FileExplorerPanel::list_area(area).unwrap();
            let row = panel.selected().unwrap();
            let expected = Position::new(
                list.y
                    .saturating_add((panel.selection - panel.scroll) as u16)
                    as usize,
                list.x.saturating_add(
                    panel.row_label_offset(row, editor.config().file_explorer.icons),
                ) as usize,
            );
            let (position, kind) = panel.cursor(area, &editor);

            assert_eq!(kind, CursorKind::Block);
            assert_eq!(position, Some(expected));
            assert!(expected.col > list.x as usize);

            let (position, kind) = panel.cursor(Rect::new(0, 0, 1, 1), &editor);
            assert_eq!(kind, CursorKind::Hidden);
            assert_eq!(position, Some(Position::new(0, 0)));

            panel.focused = false;
            let (position, kind) = panel.cursor(area, &editor);
            assert_eq!(kind, CursorKind::Hidden);
            assert_eq!(position, None);
        });
    }

    #[test]
    fn label_edit_cursor_follows_configured_insert_shape() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("README.md"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            // Build an editor configured with `insert = "bar"` from the
            // start — mirrors the user's config.toml so the test reflects
            // the real wiring.
            let cursor_cfg: helix_view::editor::Config =
                toml::from_str("[cursor-shape]\ninsert = \"bar\"\n").unwrap();
            let theme_loader = helix_view::theme::Loader::new(helix_loader::runtime_dirs());
            let syn_loader = helix_core::config::default_lang_loader();
            let config = helix_view::editor::Config {
                cursor_shape: cursor_cfg.cursor_shape,
                ..Default::default()
            };
            let config = Arc::new(ArcSwap::from_pointee(config));
            let handlers = helix_view::handlers::Handlers::dummy();
            let mut editor = Editor::new(
                Rect::new(0, 0, 100, 30),
                Arc::new(theme_loader),
                Arc::new(ArcSwap::from_pointee(syn_loader)),
                Arc::new(arc_swap::access::Map::new(
                    config,
                    |c: &helix_view::editor::Config| c,
                )),
                rt.runtime(),
                handlers,
            );

            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let area = Rect::new(0, 0, 40, 10);
            panel.area = area;
            panel.selection = row_index_by_name(&panel, "README.md");
            panel.ensure_selection_visible();

            // Sanity: not editing → Block (visual selection marker).
            let (_, kind) = panel.cursor(area, &editor);
            assert_eq!(kind, CursorKind::Block);

            // Begin inline rename. Mode flips to Insert.
            panel.enter_label_edit_rename(
                &mut editor,
                helix_view::edit_region::InsertEntry::AtCurrent,
            );
            assert_eq!(panel.input.mode, Mode::Insert);

            let (_, kind) = panel.cursor(area, &editor);
            assert_eq!(
                kind,
                CursorKind::Bar,
                "Insert-mode label edit should mirror the editor's configured Insert cursor shape",
            );
        });
    }

    #[test]
    fn label_word_motions_stay_inside_selected_name() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("alpha-beta.rs");
        fs::write(&file, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");

            let label = "alpha-beta.rs";

            let mut expected = LabelSelection::default().apply_motion(
                label,
                LabelMotion::NextWordStart(1),
                CoreMovement::Move,
            );
            panel.move_label_selection(LabelMotion::NextWordStart(1), CoreMovement::Move);
            assert_eq!(panel.label_cursor(), expected.cursor(label));
            assert_eq!(panel.label_selection, expected);

            expected =
                expected.apply_motion(label, LabelMotion::NextWordEnd(1), CoreMovement::Move);
            panel.move_label_selection(LabelMotion::NextWordEnd(1), CoreMovement::Move);
            assert_eq!(panel.label_cursor(), expected.cursor(label));
            assert_eq!(panel.label_selection, expected);

            expected =
                expected.apply_motion(label, LabelMotion::PrevWordStart(1), CoreMovement::Move);
            panel.move_label_selection(LabelMotion::PrevWordStart(1), CoreMovement::Move);
            assert_eq!(panel.label_cursor(), expected.cursor(label));
            assert_eq!(panel.label_selection, expected);

            expected = expected.apply_motion(label, LabelMotion::LineEnd, CoreMovement::Move);
            panel.move_label_selection(LabelMotion::LineEnd, CoreMovement::Move);
            assert_eq!(panel.label_cursor(), "alpha-beta.rs".chars().count() - 1);
            assert_eq!(panel.label_selection, expected);
        });
    }

    /// `w` from the last word of the currently-selected row's label
    /// wraps to the next row. The user explicitly asked for word
    /// motions to behave like the editor's "next line" wrap, treating
    /// each tree row as one line of the explorer "buffer".
    #[test]
    fn w_at_end_of_label_wraps_to_next_row() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        fs::write(temp.path().join("beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");
            let initial_row = panel.selection;
            // Move cursor to the last position on alpha.rs's label so
            // the next `w` has nowhere to go within this row.
            panel.label_selection = LabelSelection::point("alpha.rs".chars().count());
            // Drive `w` motion straight through the explorer's action
            // dispatch path (matches what the modal engine routes).
            panel.move_label_selection(LabelMotion::NextWordStart(1), CoreMovement::Move);

            assert_ne!(
                panel.selection, initial_row,
                "w at end of label should advance the row selection"
            );
            assert_eq!(
                panel.label_cursor(),
                0,
                "after wrap, label cursor lands at column 0 of the new row"
            );
        });
    }

    /// `b` from column 0 wraps to the previous row.
    #[test]
    fn b_at_start_of_label_wraps_to_previous_row() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        fs::write(temp.path().join("beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "beta.rs");
            let initial_row = panel.selection;
            panel.label_selection = LabelSelection::point(0);
            panel.move_label_selection(LabelMotion::PrevWordStart(1), CoreMovement::Move);

            assert!(
                panel.selection < initial_row,
                "b at column 0 should retreat the row selection; \
                 initial={initial_row} after={}",
                panel.selection,
            );
        });
    }

    /// Mid-label, `w` advances within the label and does NOT wrap.
    /// Pinning this down so the wrap behavior doesn't bleed into normal
    /// in-label navigation.
    #[test]
    fn w_mid_label_advances_within_label_no_wrap() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha-beta.rs"), "").unwrap();
        fs::write(temp.path().join("gamma.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");
            let initial_row = panel.selection;
            panel.label_selection = LabelSelection::point(0);

            panel.move_label_selection(
                LabelMotion::NextWordStart(1),
                CoreMovement::Move,
            );

            assert_eq!(
                panel.selection, initial_row,
                "w from start of label must NOT wrap — it has room to advance within \"alpha-beta.rs\""
            );
            assert!(
                panel.label_cursor() > 0,
                "cursor should have advanced within the label",
            );
        });
    }

    /// `w` on the last row of the tree should NOT panic or wrap — it
    /// has nowhere to go.
    #[test]
    fn w_at_last_row_does_not_panic() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            // Select the last row.
            panel.selection = panel.rows.len().saturating_sub(1);
            let label_len = panel
                .selected_label()
                .map(|l| l.chars().count())
                .unwrap_or(0);
            panel.label_selection = LabelSelection::point(label_len);

            panel.move_label_selection(LabelMotion::NextWordStart(1), CoreMovement::Move);

            // No panic, row unchanged because there's no next row.
            assert_eq!(panel.selection, panel.rows.len().saturating_sub(1));
        });
    }

    /// `b` on the first row should NOT panic or wrap before row 0.
    #[test]
    fn b_at_first_row_does_not_panic() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = 0;
            panel.label_selection = LabelSelection::point(0);

            panel.move_label_selection(LabelMotion::PrevWordStart(1), CoreMovement::Move);

            assert_eq!(panel.selection, 0, "no previous row to wrap to");
        });
    }

    /// `gw` in the explorer (no rename active) starts a jump session
    /// over visible rows. Pressing the label for a row jumps to it.
    #[test]
    fn gw_starts_jump_session_and_resolves_to_row() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        fs::write(temp.path().join("beta.rs"), "").unwrap();
        fs::write(temp.path().join("gamma.rs"), "").unwrap();
        fs::write(temp.path().join("delta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.area = Rect::new(0, 0, 40, 10);
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            // Press `gw` → session starts. (`g` is a Pending engine
            // prefix; `w` resolves to `goto_word` → StartJumpSession.)
            press_key(&mut panel, &mut editor, &rt, key!('g'));
            press_key(&mut panel, &mut editor, &rt, key!('w'));
            assert!(panel.jump_session.is_some(), "gw should start a session");

            // First visible row is target_id 0 → label "aa". Press
            // "a" then "a" — should select row 0.
            press_key(&mut panel, &mut editor, &rt, key!('a'));
            assert!(panel.jump_session.is_some(), "still pending after 1 char");
            press_key(&mut panel, &mut editor, &rt, key!('a'));

            assert!(panel.jump_session.is_none(), "session resolved");
            assert_eq!(panel.selection, panel.scroll);
        });
    }

    /// Pressing Esc during a jump session cancels it without changing
    /// the row selection.
    #[test]
    fn jump_session_cancels_on_esc() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        fs::write(temp.path().join("beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.area = Rect::new(0, 0, 40, 10);
            let initial = panel.selection;

            press_key(&mut panel, &mut editor, &rt, key!('g'));
            press_key(&mut panel, &mut editor, &rt, key!('w'));
            assert!(panel.jump_session.is_some());

            press_key(&mut panel, &mut editor, &rt, key!(Esc));
            assert!(panel.jump_session.is_none(), "esc clears the session");
            assert_eq!(
                panel.selection, initial,
                "esc must not change the selection"
            );
        });
    }

    /// A jump session resolves to the second visible row when the user
    /// types the label for target_id 1 — confirms the session uses
    /// on-screen position (not the absolute row index) for IDs.
    #[test]
    fn jump_session_resolves_to_second_visible_row() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        fs::write(temp.path().join("beta.rs"), "").unwrap();
        fs::write(temp.path().join("gamma.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.area = Rect::new(0, 0, 40, 10);

            // Pre-condition: enough room for at least 2 visible rows.
            assert!(panel.rows.len() >= 2);

            press_key(&mut panel, &mut editor, &rt, key!('g'));
            press_key(&mut panel, &mut editor, &rt, key!('w'));

            // target_id 1 → label_indices_for(1) = (1, 0) → label "ba".
            press_key(&mut panel, &mut editor, &rt, key!('b'));
            press_key(&mut panel, &mut editor, &rt, key!('a'));

            assert!(panel.jump_session.is_none());
            assert_eq!(panel.selection, panel.scroll.saturating_add(1));
        });
    }

    /// The jump session honors the editor's
    /// `editor.jump-label-alphabet` config. If the user remaps the
    /// alphabet to something like ['j', 'k', 'l'], then label "jj"
    /// should still resolve to the first target — proving the
    /// session reads from config, not a hard-coded `a..=z`.
    #[test]
    fn jump_session_uses_configured_alphabet() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        fs::write(temp.path().join("beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            // Build an editor from scratch with a custom alphabet
            // (instead of `test_editor` which uses defaults).
            let new_cfg = helix_view::editor::Config {
                jump_label_alphabet: vec!['j', 'k', 'l'],
                ..helix_view::editor::Config::default()
            };
            let theme_loader = helix_view::theme::Loader::new(helix_loader::runtime_dirs());
            let syn_loader = helix_core::config::default_lang_loader();
            let arc_cfg = Arc::new(ArcSwap::from_pointee(new_cfg));
            let handlers = helix_view::handlers::Handlers::dummy();
            let mut editor = Editor::new(
                Rect::new(0, 0, 100, 30),
                Arc::new(theme_loader),
                Arc::new(ArcSwap::from_pointee(syn_loader)),
                Arc::new(arc_swap::access::Map::new(
                    arc_cfg,
                    |c: &helix_view::editor::Config| c,
                )),
                rt.runtime(),
                handlers,
            );
            editor.frontend_mut().modal_keymaps = Arc::new(ArcSwap::from_pointee(
                crate::keymap::to_component_modal_keymaps(&crate::keymap::default()),
            ));
            editor.frontend_mut().semantic_modal_keymaps = Arc::new(ArcSwap::from_pointee(
                crate::keymap::to_semantic_modal_keymaps(&crate::keymap::default()),
            ));
            Arc::new(helix_modal::ModalEngineFactory::default()).install(&mut editor);
            editor.new_file(helix_view::editor::Action::VerticalSplit);

            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.area = Rect::new(0, 0, 40, 10);
            assert!(!panel.rows.is_empty());

            press_key(&mut panel, &mut editor, &rt, key!('g'));
            press_key(&mut panel, &mut editor, &rt, key!('w'));
            assert!(panel.jump_session.is_some());

            // With alphabet=['j','k','l'], target 0 still maps to
            // (first=0, second=0) of the alphabet → 'j', 'j'.
            press_key(&mut panel, &mut editor, &rt, key!('j'));
            press_key(&mut panel, &mut editor, &rt, key!('j'));

            assert!(
                panel.jump_session.is_none(),
                "jj resolved with custom alphabet"
            );
            assert_eq!(panel.selection, panel.scroll);
        });
    }

    /// A non-alphabet key during a jump session cancels it cleanly
    /// (no row change, session cleared). This is what makes the
    /// "press gw, then think better of it, type something else" path
    /// safe — no stale labels rendered after the user moves on.
    #[test]
    fn jump_session_cancels_on_non_alphabet_key() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        fs::write(temp.path().join("beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.area = Rect::new(0, 0, 40, 10);
            let initial = panel.selection;

            press_key(&mut panel, &mut editor, &rt, key!('g'));
            press_key(&mut panel, &mut editor, &rt, key!('w'));

            // '5' isn't in the alphabet → cancel.
            press_key(&mut panel, &mut editor, &rt, key!('5'));
            assert!(panel.jump_session.is_none());
            assert_eq!(panel.selection, initial);
        });
    }

    #[test]
    fn panel_labels_use_forward_slashes() {
        let base = PathBuf::from("workspace");
        let path = base.join("src").join("main").join("java");

        assert_eq!(relative_display(&base, &path), "src/main/java");
    }

    #[test]
    fn label_edit_range_uses_current_character_for_point_selection() {
        let label = "alpha-beta.rs";
        let range = LabelEditRange::from_selection(LabelSelection::point(2), label).unwrap();

        assert_eq!(range, LabelEditRange { start: 2, end: 3 });
        assert_eq!(range.selected_text(label), "p");
        assert_eq!(range.remove_from(label), "alha-beta.rs");
        assert!(!range.is_whole(label.chars().count()));
    }

    #[test]
    fn label_edit_range_can_cover_whole_item() {
        let label = "alpha-beta.rs";
        let range = LabelEditRange::from_selection(LabelSelection::all(label), label).unwrap();

        assert_eq!(range, LabelEditRange { start: 0, end: 13 });
        assert_eq!(range.selected_text(label), label);
        assert!(range.is_whole(label.chars().count()));
    }

    #[test]
    fn sibling_label_rejects_path_segments() {
        let source = PathBuf::from("root").join("alpha.rs");

        assert!(matches!(
            sibling_path_with_label(&source, ""),
            Err(LabelRenameError::Empty)
        ));
        assert!(matches!(
            sibling_path_with_label(&source, "../beta.rs"),
            Err(LabelRenameError::PathSeparator)
        ));
        assert!(matches!(
            sibling_path_with_label(&source, "."),
            Err(LabelRenameError::DotSegment)
        ));
        assert_eq!(
            sibling_path_with_label(&source, "beta.rs").unwrap(),
            PathBuf::from("root").join("beta.rs")
        );
    }

    #[test]
    fn focused_panel_consumes_unmapped_keys() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("main.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let (ingress, _ingress_rx) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().work().clone());
            let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
            let idle_reset = crate::runtime::IdleResetGate::new().handle();
            let mut exit_tasks = crate::runtime::ExitTaskSet::default();
            let exit_task_work = editor.work();
            let redraw = editor.redraw_handle();
            let notifier = crate::handlers::local::Notifier {
                redraw: redraw.clone(),
                plugin_events,
            };
            let mut cx = Context::new(
                &mut editor,
                &mut exit_tasks,
                exit_task_work,
                notifier,
                ingress,
                idle_reset,
                None,
            );

            assert!(matches!(
                panel.handle_event(&Event::Key(key!('i')), &mut cx),
                EventResult::Consumed(None)
            ));
        });
    }

    #[test]
    fn focused_panel_bubbles_command_mode_to_editor() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("main.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            with_context(&mut editor, &rt, |cx| {
                assert!(matches!(
                    panel.handle_event(&Event::Key(key!(':')), cx),
                    EventResult::Ignored(None)
                ));
            });
        });
    }

    #[test]
    fn tree_item_includes_fallback_icons() {
        let row = ExplorerRow {
            path: PathBuf::from("workspace").join("src"),
            label: "src".to_string(),
            is_dir: true,
            depth: 1,
            expanded: true,
            is_last: true,
            ancestor_last: Vec::new(),
            vcs_status: None,
            diagnostic_status: None,
        };
        let panel = FileExplorerPanel {
            root: PathBuf::from("workspace"),
            rows: Vec::new(),
            expanded_dirs: HashSet::new(),
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::default(),
            diagnostic_snapshot: DiagnosticSnapshot::default(),
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_debouncer: None,
            model_panel_id: None,
            last_click: None,
            label_edit: None,
            label_edit_region: helix_view::edit_region::EditRegion::default(),
            jump_session: None,
            nav: helix_view::list_nav::ListNav::new(),
        };

        let (_surface, rendered) = render_tree_row(
            &panel,
            &row,
            row_tree_item_styles(None, true),
            Style::default(),
        );

        assert!(rendered.contains(FALLBACK_FOLDER_OPEN_ICON));
    }

    #[test]
    fn tree_item_highlights_only_selected_label_range() {
        let row = ExplorerRow {
            path: PathBuf::from("alpha-beta.rs"),
            label: "alpha-beta.rs".to_string(),
            is_dir: false,
            depth: 0,
            expanded: false,
            is_last: true,
            ancestor_last: Vec::new(),
            vcs_status: None,
            diagnostic_status: None,
        };
        let panel = FileExplorerPanel {
            root: PathBuf::from("workspace"),
            rows: Vec::new(),
            expanded_dirs: HashSet::new(),
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::default(),
            diagnostic_snapshot: DiagnosticSnapshot::default(),
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_debouncer: None,
            model_panel_id: None,
            last_click: None,
            label_edit: None,
            label_edit_region: helix_view::edit_region::EditRegion::default(),
            jump_session: None,
            nav: helix_view::list_nav::ListNav::new(),
        };
        let selection_style = Style::default().bg(helix_view::graphics::Color::Rgb(20, 40, 80));

        let (surface, _rendered) = render_tree_row(
            &panel,
            &row,
            row_tree_item_styles(Some(0..5), false),
            selection_style,
        );

        // The fuzzy-match highlight only paints inside the label range —
        // the rest of the row keeps the panel background. (The selected-row
        // accent is the tree connector glyph, drawn elsewhere; for a
        // depth-0 row like this fixture there is no connector to colour.)
        let selected = tui::ratatui::to_ratatui_style(selection_style);
        let selected_bg = selected.bg.expect("selection background");
        assert_eq!(surface[(0, 0)].symbol(), "a");
        assert_eq!(surface[(0, 0)].bg, selected_bg);
        assert_eq!(surface[(5, 0)].symbol(), "-");
        assert_ne!(surface[(5, 0)].bg, selected_bg);
    }

    #[test]
    fn tree_item_uses_tree_guides_without_disclosure_arrows() {
        let row = ExplorerRow {
            path: PathBuf::from("workspace").join("src"),
            label: "src".to_string(),
            is_dir: true,
            depth: 2,
            expanded: false,
            is_last: true,
            ancestor_last: vec![false],
            vcs_status: None,
            diagnostic_status: None,
        };
        let panel = FileExplorerPanel {
            root: PathBuf::from("workspace"),
            rows: Vec::new(),
            expanded_dirs: HashSet::new(),
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::default(),
            diagnostic_snapshot: DiagnosticSnapshot::default(),
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_debouncer: None,
            model_panel_id: None,
            last_click: None,
            label_edit: None,
            label_edit_region: helix_view::edit_region::EditRegion::default(),
            jump_session: None,
            nav: helix_view::list_nav::ListNav::new(),
        };

        let (_surface, rendered) = render_tree_row(
            &panel,
            &row,
            row_tree_item_styles(None, true),
            Style::default(),
        );

        assert!(rendered.contains("│ "));
        assert!(rendered.contains("└╴"));
        assert!(!rendered.contains(''));
        assert!(!rendered.contains(''));
    }

    #[test]
    fn tree_item_uses_distinct_folder_icons_for_open_and_closed_dirs() {
        let panel = FileExplorerPanel {
            root: PathBuf::from("workspace"),
            rows: Vec::new(),
            expanded_dirs: HashSet::new(),
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::default(),
            diagnostic_snapshot: DiagnosticSnapshot::default(),
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_debouncer: None,
            model_panel_id: None,
            last_click: None,
            label_edit: None,
            label_edit_region: helix_view::edit_region::EditRegion::default(),
            jump_session: None,
            nav: helix_view::list_nav::ListNav::new(),
        };
        let open_row = ExplorerRow {
            path: PathBuf::from("workspace").join("src"),
            label: "src".to_string(),
            is_dir: true,
            depth: 1,
            expanded: true,
            is_last: true,
            ancestor_last: Vec::new(),
            vcs_status: None,
            diagnostic_status: None,
        };
        let closed_row = ExplorerRow {
            expanded: false,
            ..open_row.clone()
        };

        let (_surface, open) = render_tree_row(
            &panel,
            &open_row,
            row_tree_item_styles(None, true),
            Style::default(),
        );
        let (_surface, closed) = render_tree_row(
            &panel,
            &closed_row,
            row_tree_item_styles(None, true),
            Style::default(),
        );

        assert!(open.contains(FALLBACK_FOLDER_OPEN_ICON));
        assert!(closed.contains(FALLBACK_FOLDER_ICON));
    }

    #[test]
    fn vcs_snapshot_aggregates_status_to_parent_directories() {
        let root = helix_stdx::path::normalize(PathBuf::from("workspace"));
        let file = root.join("src").join("main.rs");
        let snapshot =
            VcsSnapshot::from_changes(&root, [FileChange::Modified { path: file.clone() }]);

        assert_eq!(snapshot.status(&file), Some(VcsStatus::Modified));
        assert_eq!(
            snapshot.status(&root.join("src")),
            Some(VcsStatus::Modified)
        );
        assert_eq!(snapshot.status(&root), Some(VcsStatus::Modified));
    }

    #[test]
    fn vcs_status_merge_prefers_conflicts() {
        assert_eq!(
            VcsStatus::Modified.merge(VcsStatus::Conflict),
            VcsStatus::Conflict
        );
    }

    #[test]
    fn diagnostic_snapshot_aggregates_status_to_parent_directories() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        let main = src.join("main.rs");
        fs::create_dir(&src).unwrap();
        fs::write(&main, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            add_diagnostic(&mut editor, &main, LspDiagnosticSeverity::WARNING);
            add_diagnostic(&mut editor, &main, LspDiagnosticSeverity::ERROR);

            let root = helix_stdx::path::normalize(temp.path());
            let src = helix_stdx::path::normalize(src);
            let main = helix_stdx::path::normalize(main);
            let snapshot = DiagnosticSnapshot::from_editor(&root, &editor);
            let expected = Some(DiagnosticStatus {
                severity: DiagnosticSeverity::Error,
                count: 2,
            });

            assert_eq!(snapshot.status(&main), expected);
            assert_eq!(snapshot.status(&src), expected);
            assert_eq!(snapshot.status(&root), expected);
        });
    }

    #[test]
    fn panel_sync_refreshes_diagnostic_rows() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("main.rs");
        fs::write(&file, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let index = row_index_by_name(&panel, "main.rs");
            assert_eq!(panel.rows[index].diagnostic_status, None);

            add_diagnostic(&mut editor, &file, LspDiagnosticSeverity::WARNING);
            panel.sync(&mut editor);

            let index = row_index_by_name(&panel, "main.rs");
            assert_eq!(
                panel.rows[index].diagnostic_status,
                Some(DiagnosticStatus {
                    severity: DiagnosticSeverity::Warning,
                    count: 1,
                })
            );
        });
    }

    #[test]
    fn diagnostic_navigation_selects_visible_diagnostic_rows() {
        let temp = tempfile::tempdir().unwrap();
        let readme = temp.path().join("README.md");
        let src = temp.path().join("src");
        let main = src.join("main.rs");
        fs::create_dir(&src).unwrap();
        fs::write(&readme, "").unwrap();
        fs::write(&main, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            add_diagnostic(&mut editor, &readme, LspDiagnosticSeverity::WARNING);
            add_diagnostic(&mut editor, &main, LspDiagnosticSeverity::ERROR);
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "src");
            panel.toggle_selected_dir(&editor);

            panel.select_first();
            panel.select_next_diagnostic();
            assert_eq!(display_name(&panel.rows[panel.selection].path), "src");

            panel.select_next_diagnostic();
            assert_eq!(display_name(&panel.rows[panel.selection].path), "main.rs");

            panel.select_last_diagnostic();
            assert_eq!(display_name(&panel.rows[panel.selection].path), "README.md");

            panel.select_previous_diagnostic();
            assert_eq!(display_name(&panel.rows[panel.selection].path), "main.rs");
        });
    }

    #[test]
    fn panel_syncs_docked_tree_model() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("lib.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            panel.sync(&mut editor);

            let panel_id = panel.panel_id().expect("panel id");
            let entry = editor.model.panels.get(panel_id).expect("model panel");
            assert_eq!(entry.side, PanelSide::Left);
            assert!(entry.content.is::<TreePanelModel>());
        });
    }

    // ── Inline label edit ──────────────────────────────────────────────────

    #[test]
    fn pressing_i_enters_inline_edit_with_cursor_at_label_start() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            press_key(&mut panel, &mut editor, &rt, key!('i'));

            let edit = panel.label_edit.as_ref().expect("inline edit started");
            assert_eq!(edit.buffer, "alpha.rs");
            assert!(matches!(edit.kind, LabelEditKind::Rename { .. }));
            assert_eq!(panel.input.mode, Mode::Insert);
            assert_eq!(panel.label_selection.cursor(&edit.buffer), 0);
        });
    }

    /// `a` (append_mode) must land one position past where `i` lands —
    /// mirroring Helix's editor semantics. Before fixing this, both keys
    /// collapsed to `LabelEditEntry::AtCurrent`, so `a` felt identical to
    /// `i` and typed at column 0 instead of column 1.
    #[test]
    fn pressing_a_enters_inline_edit_one_past_i() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            press_key(&mut panel, &mut editor, &rt, key!('a'));

            let edit = panel.label_edit.as_ref().expect("inline edit started");
            assert_eq!(edit.buffer, "alpha.rs");
            assert_eq!(panel.input.mode, Mode::Insert);
            // Cursor lands one past the row's label cursor (which is 0 on
            // a fresh row), so typing produces "a<inserted>lpha.rs". The
            // synced `edit.cursor` mirrors the EditRegion's selection
            // cursor — that's the source of truth in the new model;
            // `label_selection` is only used for tree-mode navigation
            // when no label edit is in progress.
            assert_eq!(edit.cursor, 1);
        });
    }

    /// `I` (insert_at_line_start) lands the cursor at column 0 even if
    /// the row's tree-mode cursor (`label_selection`) is somewhere mid-
    /// label. The transform is owned by
    /// [`helix_view::edit_region::EditRegion::enter_insert_at`].
    #[test]
    fn pressing_capital_i_lands_cursor_at_label_start() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");
            // Pre-move the row cursor (tree-mode cursor on the label) so
            // the entry has somewhere meaningful to flatten from.
            panel.label_selection = LabelSelection::point(3);

            press_key(&mut panel, &mut editor, &rt, key!('I'));

            let edit = panel.label_edit.as_ref().expect("inline edit started");
            assert_eq!(edit.cursor, 0);
        });
    }

    /// `A` (insert_at_line_end) lands the cursor at the end of the label,
    /// regardless of where the row's tree-mode cursor was sitting.
    #[test]
    fn pressing_capital_a_lands_cursor_at_label_end() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");
            panel.label_selection = LabelSelection::point(2);

            press_key(&mut panel, &mut editor, &rt, key!('A'));

            let edit = panel.label_edit.as_ref().expect("inline edit started");
            assert_eq!(edit.cursor, "alpha.rs".chars().count());
        });
    }

    /// `a` clamps to the label end if the row cursor is already at the
    /// last character — appending past the end shouldn't wrap or panic.
    /// The clamp lives in
    /// [`helix_view::edit_region::EditRegion::enter_insert_at`]; this
    /// test pins down that the file explorer inherits it.
    #[test]
    fn pressing_a_at_end_of_label_clamps_to_end() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");
            let len = "alpha.rs".chars().count();
            panel.label_selection = LabelSelection::point(len);

            press_key(&mut panel, &mut editor, &rt, key!('a'));

            let edit = panel.label_edit.as_ref().expect("inline edit started");
            assert_eq!(edit.cursor, len);
        });
    }

    #[test]
    fn typing_in_insert_mode_mutates_buffer_without_touching_disk() {
        let temp = tempfile::tempdir().unwrap();
        let alpha = temp.path().join("alpha.rs");
        fs::write(&alpha, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            // `A` lands the cursor at the end of the label so subsequent
            // chars append. (Used to be `a` plus a manual
            // `label_selection` poke; in the unified-dispatch model that
            // poke no longer does anything — the cursor lives in the
            // EditRegion's document.)
            press_key(&mut panel, &mut editor, &rt, key!('A'));
            press_key(&mut panel, &mut editor, &rt, key!('x'));
            press_key(&mut panel, &mut editor, &rt, key!('y'));
            press_key(&mut panel, &mut editor, &rt, key!('z'));

            let edit = panel.label_edit.as_ref().expect("editing");
            assert_eq!(edit.buffer, "alpha.rsxyz");
            // Disk is unchanged — commit happens on Enter, not on each
            // keystroke.
            assert!(alpha.exists());
        });
    }

    /// `w` in Normal mode advances the cursor by one Helix word.
    /// Reaching Normal mode requires a single Esc from Insert.
    #[test]
    fn w_advances_cursor_by_word_in_label_edit_normal_mode() {
        let temp = tempfile::tempdir().unwrap();
        // Use a hyphenated name so there are real word boundaries to
        // jump across — Helix's word definition treats `-` as a
        // separator between sub-words.
        fs::write(temp.path().join("alpha-beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");

            press_key(&mut panel, &mut editor, &rt, key!('i'));
            press_key(&mut panel, &mut editor, &rt, key!(Esc));
            // Cursor sits at 0 after `i`-then-Esc; `w` should jump past
            // "alpha" to the `-` (or the next word start, depending on
            // engine word semantics). Either way it must advance — a
            // zero-delta `w` would indicate the engine isn't seeing the
            // motion at all.
            let before = panel.label_edit.as_ref().unwrap().cursor;
            press_key(&mut panel, &mut editor, &rt, key!('w'));
            let after = panel.label_edit.as_ref().unwrap().cursor;

            assert!(
                after > before,
                "`w` should advance cursor; before={before} after={after}",
            );
            // Sanity check: the panel should still be editing — `w`
            // mustn't accidentally bubble to the explorer and trigger
            // something unrelated.
            assert!(panel.label_edit.is_some());
            assert_eq!(panel.input.mode, Mode::Normal);
        });
    }

    /// `b` in Normal mode retreats the cursor by one Helix word.
    /// The cursor starts at end of the label (via `A` then Esc).
    #[test]
    fn b_retreats_cursor_by_word_in_label_edit_normal_mode() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha-beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");

            press_key(&mut panel, &mut editor, &rt, key!('A')); // cursor at end
            press_key(&mut panel, &mut editor, &rt, key!(Esc)); // drop to Normal
            let before = panel.label_edit.as_ref().unwrap().cursor;
            press_key(&mut panel, &mut editor, &rt, key!('b'));
            let after = panel.label_edit.as_ref().unwrap().cursor;

            assert!(
                after < before,
                "`b` should retreat cursor; before={before} after={after}",
            );
        });
    }

    /// `w` from the last word must NOT panic, wrap to a phantom next
    /// line, or send the cursor past the buffer's end. The label edit
    /// is a single-line rope; Helix's word-forward motion at EOL
    /// normally wraps to the next line, but with only one line it
    /// should clamp.
    #[test]
    fn w_at_end_of_single_line_label_clamps() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha-beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");

            press_key(&mut panel, &mut editor, &rt, key!('A')); // cursor at end
            press_key(&mut panel, &mut editor, &rt, key!(Esc)); // Normal
            let buffer_len = panel.label_edit.as_ref().unwrap().buffer.chars().count();

            // Hammer `w` more times than the buffer has characters —
            // anything that would walk off the end shows up as a
            // cursor that's > buffer_len.
            for _ in 0..8 {
                press_key(&mut panel, &mut editor, &rt, key!('w'));
            }
            let edit = panel.label_edit.as_ref().expect("still editing");
            assert!(
                edit.cursor <= buffer_len,
                "cursor should never exceed buffer length; cursor={} buffer_len={buffer_len} buffer={:?}",
                edit.cursor,
                edit.buffer,
            );
            assert_eq!(edit.buffer, "alpha-beta.rs", "buffer must not be mutated");
        });
    }

    /// `b` from column 0 should clamp at 0 — no panic, no underflow,
    /// no jumping to a phantom previous line.
    #[test]
    fn b_at_start_of_single_line_label_clamps() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha-beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");

            press_key(&mut panel, &mut editor, &rt, key!('i'));
            press_key(&mut panel, &mut editor, &rt, key!(Esc)); // Normal at 0

            for _ in 0..8 {
                press_key(&mut panel, &mut editor, &rt, key!('b'));
            }
            let edit = panel.label_edit.as_ref().expect("still editing");
            assert_eq!(edit.cursor, 0, "cursor should clamp at 0");
            assert_eq!(edit.buffer, "alpha-beta.rs");
        });
    }

    /// `e` (next word end) advances toward the end of the current /
    /// next word. Together with `w` and `b` this covers the three
    /// primary word motions.
    #[test]
    fn e_advances_to_word_end_in_label_edit_normal_mode() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha-beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");

            press_key(&mut panel, &mut editor, &rt, key!('i'));
            press_key(&mut panel, &mut editor, &rt, key!(Esc));
            let before = panel.label_edit.as_ref().unwrap().cursor;
            press_key(&mut panel, &mut editor, &rt, key!('e'));
            let after = panel.label_edit.as_ref().unwrap().cursor;

            assert!(after > before, "`e` should advance");
        });
    }

    /// `gw` (goto_word) is a Helix Frontend command that asks the host
    /// to render two-character jump labels. The file explorer doesn't
    /// implement label rendering for the inline rename buffer; we want
    /// the dispatch path to:
    /// - leave the buffer untouched (no `g` or `w` inserted)
    /// - leave the cursor where it was (no spurious goto_file_start /
    ///   "jump to root" execution)
    /// - not panic
    ///
    /// The "leaves cursor where it was" assertion is the one that
    /// caught a real bug: in the EditRegion's filtered modal keymap,
    /// the `g` submenu is missing the `w` binding (it's a Frontend
    /// command, filtered by `to_component_modal_keymaps`). The engine
    /// used to resolve the pending-`g`-then-`w` to whatever fallback
    /// was sitting in the trie root, sending the cursor home.
    #[test]
    fn gw_does_not_panic_or_corrupt_or_move_cursor_in_label_edit() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha-beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");

            // Position cursor in the middle of the label so a stray
            // "goto root" would visibly move it.
            press_key(&mut panel, &mut editor, &rt, key!('A')); // end
            press_key(&mut panel, &mut editor, &rt, key!(Esc)); // Normal
            press_key(&mut panel, &mut editor, &rt, key!('b')); // back one word
            let cursor_before = panel.label_edit.as_ref().unwrap().cursor;
            assert!(cursor_before > 0, "cursor must be mid-label for this test");

            press_key(&mut panel, &mut editor, &rt, key!('g'));
            press_key(&mut panel, &mut editor, &rt, key!('w'));

            let edit = panel.label_edit.as_ref().expect("still editing");
            assert_eq!(edit.buffer, "alpha-beta.rs", "buffer must not be mutated");
            assert_eq!(
                edit.cursor, cursor_before,
                "gw is a frontend label-jump command we don't render — the cursor must stay put",
            );
        });
    }

    /// `gg` (goto_file_start) on a single-line label should land the
    /// cursor at position 0 — the editor's analog of "go to top of
    /// document". This is the "go to root" behavior — and it's correct
    /// for `gg`. If the user observed it after pressing `gw`, the keymap
    /// is probably interpreting the second key incorrectly.
    #[test]
    fn gg_moves_cursor_to_start_of_label() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha-beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha-beta.rs");

            press_key(&mut panel, &mut editor, &rt, key!('A'));
            press_key(&mut panel, &mut editor, &rt, key!(Esc));
            let len = panel.label_edit.as_ref().unwrap().buffer.chars().count();
            assert!(panel.label_edit.as_ref().unwrap().cursor > 0);

            press_key(&mut panel, &mut editor, &rt, key!('g'));
            press_key(&mut panel, &mut editor, &rt, key!('g'));

            let edit = panel.label_edit.as_ref().expect("still editing");
            assert_eq!(
                edit.cursor, 0,
                "gg should go to start of buffer (line 0 col 0)"
            );
            assert_eq!(edit.buffer, "alpha-beta.rs", "buffer must not be mutated");
            // Sanity: confirm `len` was > 0 above so the assertion was meaningful.
            assert!(len > 0);
        });
    }

    // `ge` (goto_last_line) on a single-line label is a no-op —
    // line 0 IS the last line, and goto_last_line lands at column 0
    // of the last line. That matches the editor's behavior on a
    // one-line file. The user's request was about `gw` and tree-row
    // word wrapping, not about ge's single-line behavior, so I'm not
    // pinning ge in a test that locks in surprising behavior.

    /// In-buffer undo: type a char, press Esc to Normal, press `u`,
    /// the char should be removed. This is the "now you get this for
    /// free" feature of going through the real engine — there was no
    /// undo support in the previous hand-rolled handler.
    #[test]
    fn undo_in_label_edit_normal_mode_reverts_typed_chars() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            press_key(&mut panel, &mut editor, &rt, key!('A')); // cursor at end
            press_key(&mut panel, &mut editor, &rt, key!('X'));
            press_key(&mut panel, &mut editor, &rt, key!('Y'));
            press_key(&mut panel, &mut editor, &rt, key!('Z'));

            assert_eq!(panel.label_edit.as_ref().unwrap().buffer, "alpha.rsXYZ");

            press_key(&mut panel, &mut editor, &rt, key!(Esc));
            press_key(&mut panel, &mut editor, &rt, key!('u'));

            let edit = panel.label_edit.as_ref().expect("still editing");
            // The exact granularity of undo depends on the engine
            // (per-insertion-block vs per-character), but the buffer
            // must move toward the pre-edit state. `XYZ` typed in one
            // insert session collapses to one undo step.
            assert!(
                edit.buffer.len() < "alpha.rsXYZ".len(),
                "undo should shorten the buffer; got {:?}",
                edit.buffer,
            );
        });
    }

    #[test]
    fn enter_commits_inline_rename_synchronously_to_disk() {
        let temp = tempfile::tempdir().unwrap();
        let alpha = temp.path().join("alpha.rs");
        let beta = temp.path().join("beta.rs");
        fs::write(&alpha, "fn alpha() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            press_key(&mut panel, &mut editor, &rt, key!('i'));
            // Replace the buffer in-place — fastest way to set up a
            // committable rename in test without driving the keymap.
            panel.label_edit.as_mut().unwrap().buffer.clear();
            panel
                .label_edit
                .as_mut()
                .unwrap()
                .buffer
                .push_str("beta.rs");
            panel.label_selection = LabelSelection::point("beta.rs".chars().count());

            with_context(&mut editor, &rt, |cx| panel.commit_label_edit(cx));

            // The rename runs synchronously inside commit_label_edit —
            // disk state should reflect the new name immediately.
            assert!(!alpha.exists(), "old path should be gone");
            assert!(beta.exists(), "new path should exist");
            assert!(panel.label_edit.is_none());
            assert_eq!(panel.input.mode, Mode::Normal);
        });
    }

    #[test]
    fn inline_rename_with_slash_creates_intermediate_directories() {
        let temp = tempfile::tempdir().unwrap();
        let alpha = temp.path().join("alpha.rs");
        fs::write(&alpha, "fn alpha() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            press_key(&mut panel, &mut editor, &rt, key!('i'));
            panel.label_edit.as_mut().unwrap().buffer.clear();
            panel
                .label_edit
                .as_mut()
                .unwrap()
                .buffer
                .push_str("nested/inner/beta.rs");

            with_context(&mut editor, &rt, |cx| panel.commit_label_edit(cx));

            // Typing `nested/inner/beta.rs` should create the directory
            // chain and put the file in its leaf — same intent as typing
            // a path into a save-as dialog in any modern editor.
            let nested = temp.path().join("nested");
            let inner = nested.join("inner");
            let beta = inner.join("beta.rs");
            assert!(nested.is_dir(), "intermediate dir nested/ created");
            assert!(inner.is_dir(), "intermediate dir nested/inner/ created");
            assert!(beta.exists(), "leaf file moved into nested/inner/");
            assert!(!alpha.exists(), "old path is gone");
        });
    }

    #[test]
    fn enter_commits_the_edit_from_insert_in_one_keypress() {
        // After migrating to `EditRegion`, the policy is:
        // - Enter in Insert → commit (one keystroke from typing)
        // - Esc in Insert → drop to Normal (so word motions are reachable)
        // - Esc/Enter in Normal → commit
        //
        // This test pins the "type then Enter" one-keystroke save path.
        // The `esc_then_esc_*` test below pins the two-keystroke path.
        let temp = tempfile::tempdir().unwrap();
        let alpha = temp.path().join("alpha.rs");
        let xalpha = temp.path().join("xalpha.rs");
        fs::write(&alpha, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            press_key(&mut panel, &mut editor, &rt, key!('i'));
            press_key(&mut panel, &mut editor, &rt, key!('x'));
            press_key(&mut panel, &mut editor, &rt, key!(Enter));

            assert!(panel.label_edit.is_none());
            assert_eq!(panel.input.mode, Mode::Normal);
            assert!(!alpha.exists());
            assert!(xalpha.exists());
        });
    }

    #[test]
    fn ctrl_c_hard_cancels_inline_edit() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "alpha.rs");

            press_key(&mut panel, &mut editor, &rt, key!('i'));
            press_key(&mut panel, &mut editor, &rt, key!('z'));
            press_key(&mut panel, &mut editor, &rt, ctrl!('c'));

            assert!(panel.label_edit.is_none());
            assert_eq!(panel.input.mode, Mode::Normal);
        });
    }

    #[test]
    fn o_on_collapsed_directory_targets_explorer_root() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("inner.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "nested");
            // Confirm it's collapsed at the start.
            assert!(!panel.selected().unwrap().expanded);

            press_key(&mut panel, &mut editor, &rt, key!('o'));

            let edit = panel.label_edit.as_ref().expect("create started");
            match &edit.kind {
                LabelEditKind::Create { parent } => {
                    // Collapsed dir → target the explorer root, not inside.
                    assert_eq!(
                        helix_stdx::path::normalize(parent),
                        helix_stdx::path::normalize(temp.path())
                    );
                }
                _ => panic!("expected Create kind, got {:?}", edit.kind),
            }
            // Collapsed dir stays collapsed — we don't expand it.
            assert!(!panel.selected().unwrap().expanded);
        });
    }

    #[test]
    fn o_on_expanded_directory_creates_inside() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "nested");
            // Expand the dir.
            panel.toggle_selected_dir(&editor);
            assert!(panel.selected().unwrap().expanded);

            press_key(&mut panel, &mut editor, &rt, key!('o'));

            let edit = panel.label_edit.as_ref().expect("create started");
            match &edit.kind {
                LabelEditKind::Create { parent } => {
                    assert_eq!(
                        helix_stdx::path::normalize(parent),
                        helix_stdx::path::normalize(&nested)
                    );
                }
                _ => panic!("expected Create kind"),
            }
        });
    }

    #[test]
    fn o_on_file_creates_in_its_parent_directory() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("inner.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "nested");
            panel.toggle_selected_dir(&editor); // expand to reveal inner.rs
            panel.selection = row_index_by_name(&panel, "inner.rs");

            press_key(&mut panel, &mut editor, &rt, key!('o'));

            let edit = panel.label_edit.as_ref().expect("create started");
            match &edit.kind {
                LabelEditKind::Create { parent } => {
                    assert_eq!(
                        helix_stdx::path::normalize(parent),
                        helix_stdx::path::normalize(&nested)
                    );
                }
                _ => panic!("expected Create kind"),
            }
        });
    }

    #[test]
    fn reserved_windows_device_names_are_recognised() {
        // Bare reserved names.
        assert_eq!(windows_reserved_label("NUL"), Some("NUL"));
        assert_eq!(windows_reserved_label("nul"), Some("NUL"));
        assert_eq!(windows_reserved_label("Con"), Some("CON"));
        assert_eq!(windows_reserved_label("PRN"), Some("PRN"));
        assert_eq!(windows_reserved_label("AUX"), Some("AUX"));
        assert_eq!(windows_reserved_label("COM1"), Some("COM1"));
        assert_eq!(windows_reserved_label("lpt9"), Some("LPT9"));
        // With an extension — Windows treats `NUL.txt` as the device too.
        assert_eq!(windows_reserved_label("NUL.txt"), Some("NUL"));
        assert_eq!(windows_reserved_label("con.log"), Some("CON"));
        // Real filenames pass through.
        assert_eq!(windows_reserved_label("file.rs"), None);
        assert_eq!(windows_reserved_label("README.md"), None);
        assert_eq!(windows_reserved_label("nullable.rs"), None);
        assert_eq!(windows_reserved_label("commander.py"), None);
    }

    #[test]
    fn inline_rename_round_trips_through_file_operation_undo() {
        let temp = tempfile::tempdir().unwrap();
        let alpha = temp.path().join("alpha.rs");
        let beta = temp.path().join("beta.rs");
        fs::write(&alpha, "fn alpha() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            // Drive the rename through the editor's file-operation history
            // directly — that's exactly the pipeline ApplyMove uses on the
            // receiving side, so this test verifies undo/redo coverage
            // without needing the full runtime dispatch.
            editor
                .move_path_with_history(&alpha, &beta)
                .expect("rename succeeds");
            assert!(beta.exists() && !alpha.exists());

            // `u` (undo_file_operation) reverts the rename. Helix's status
            // message reads "Undid move <src> -> <dst>".
            let message = editor
                .undo_file_operation()
                .expect("undo succeeds")
                .expect("returns a status message");
            let lowered = message.to_lowercase();
            assert!(
                lowered.contains("undid") || lowered.contains("undo") || lowered.contains("revert"),
                "expected undo status, got: {message}"
            );
            assert!(alpha.exists() && !beta.exists());

            // `U` (redo_file_operation) reapplies it.
            editor
                .redo_file_operation()
                .expect("redo succeeds")
                .expect("returns a status message");
            assert!(beta.exists() && !alpha.exists());
        });
    }
}
