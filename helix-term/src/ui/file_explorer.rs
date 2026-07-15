use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(test)]
use std::error::Error as _;

use helix_core::{movement::Movement as CoreMovement, unicode::width::UnicodeWidthStr, Position};
use helix_view::{
    editor::{Action, CloseError, ClosePolicy, FileExplorerConfig},
    graphics::{CursorKind, Rect},
    icons::{Icon, ICONS},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
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
    runtime::{
        ui::{command::FileExplorerCommand, file_explorer::FileExplorerPreviewRequest},
        UiCommand,
    },
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
pub use model::VcsSnapshot;
use model::{DiagnosticSnapshot, ExplorerRow, VcsStatus};
#[cfg(test)]
use path_ops::LabelRenameError;
use path_ops::{display_name, display_path, selected_cursor, LabelEditRange};
use preview::{ExplorerPreview, PreviewDocumentCache};
pub(crate) use refresh::{FileExplorerTreeWork, PreparedFileExplorerTree};
#[cfg(test)]
use render::{ExplorerStatusStyles, ExplorerTreeItemStyles};
use scan::ExplorerChild;

pub const ID: &str = "file-explorer-panel";

const HEADER_ROWS: u16 = 1;
const SEARCH_ROWS: u16 = 1;
/// Single statusline strip beneath the tree (mode chip · summary chips ·
/// counts). Transient error / info messages live in the editor's global
/// status row (rendered by `EditorView` from `cx.status_msg()`) — no
/// need to duplicate them here.
const FOOTER_ROWS: u16 = 1;
pub(crate) const PANEL_WIDTH: u16 = 34;
const SYNC_SLOW_LOG_THRESHOLD: Duration = Duration::from_millis(4);
const FALLBACK_FOLDER_ICON: &str = "";
const FALLBACK_FOLDER_OPEN_ICON: &str = "󰝰";
const FALLBACK_FILE_ICON: &str = "󰈔";
const VCS_ADDED_ICON: &str = "";
const VCS_MODIFIED_ICON: &str = "○";
const VCS_DELETED_ICON: &str = "";
const VCS_RENAMED_ICON: &str = "";
const VCS_CONFLICT_ICON: &str = "";
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);
pub struct FileExplorerPanel {
    root: PathBuf,
    /// Session-local source options. Config reloads seed new panels; explicit
    /// panel toggles remain local and are used by both tree scans and FFF.
    config: FileExplorerConfig,
    all_rows: Arc<[ExplorerRow]>,
    rows: Arc<[ExplorerRow]>,
    search_query: String,
    search_active: bool,
    search_pending: bool,
    search_generation: u64,
    search_results: Option<ExplorerSearchResults>,
    tree_generation: u64,
    tree_pending: bool,
    expanded_dirs: HashSet<PathBuf>,
    search_saved_expanded_dirs: Option<HashSet<PathBuf>>,
    children_cache: HashMap<PathBuf, Vec<ExplorerChild>>,
    vcs_snapshot: VcsSnapshot,
    diagnostic_snapshot: DiagnosticSnapshot,
    diagnostic_snapshot_revision: u64,
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
    preview_generation: u64,
    preview_request: Option<FileExplorerPreviewRequest>,
    preview_promotion: Option<ExplorerPreviewPromotion>,
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

#[derive(Clone, Debug)]
struct ExplorerPreviewPromotion {
    request: FileExplorerPreviewRequest,
    action: Action,
}

#[derive(Clone, Debug)]
struct ExplorerSearchResults {
    query: String,
    paths: Vec<PathBuf>,
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

fn windows_reserved_path(path: &Path) -> Option<&'static str> {
    path.components().find_map(|component| match component {
        std::path::Component::Normal(label) => label.to_str().and_then(windows_reserved_label),
        _ => None,
    })
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

fn filter_explorer_rows(rows: &[ExplorerRow], query: &str) -> Vec<ExplorerRow> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return rows.to_vec();
    }

    let mut include = vec![false; rows.len()];
    for (index, row) in rows.iter().enumerate() {
        if !explorer_row_matches(row, &query) {
            continue;
        }

        include[index] = true;
        let mut depth = row.depth;
        for ancestor_index in (0..index).rev() {
            let ancestor = &rows[ancestor_index];
            if ancestor.depth >= depth {
                continue;
            }
            include[ancestor_index] = true;
            depth = ancestor.depth;
            if depth == 0 {
                break;
            }
        }
    }

    rows.iter()
        .zip(include)
        .filter(|(_, include)| *include)
        .map(|(row, _)| row.clone())
        .collect()
}

fn explorer_row_matches(row: &ExplorerRow, query: &str) -> bool {
    row.label.to_ascii_lowercase().contains(query)
        || row
            .path
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains(query)
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
    #[cfg(test)]
    pub fn new(root: PathBuf, editor: &Editor) -> Result<Self, std::io::Error> {
        Self::new_with_cursor(root, editor, None)
    }

    #[cfg(any(test, feature = "storybook"))]
    pub fn new_with_cursor(
        root: PathBuf,
        editor: &Editor,
        cursor: Option<usize>,
    ) -> Result<Self, std::io::Error> {
        let root = helix_stdx::path::normalize(root);
        let mut panel = Self::new_deferred(root, editor);
        panel.refresh(editor, None, cursor)?;
        Ok(panel)
    }

    /// Constructs a panel from an explorer root normalized at the command boundary.
    pub(crate) fn new_deferred(root: PathBuf, editor: &Editor) -> Self {
        let config = editor.config().file_explorer.clone();
        let panel = Self {
            root: root.clone(),
            config: config.clone(),
            all_rows: Arc::from([]),
            rows: Arc::from([]),
            search_query: String::new(),
            search_active: false,
            search_pending: false,
            search_generation: 0,
            search_results: None,
            tree_generation: 0,
            tree_pending: false,
            expanded_dirs: HashSet::from([root.clone()]),
            search_saved_expanded_dirs: None,
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::empty(&root, config.vcs),
            diagnostic_snapshot: DiagnosticSnapshot::empty(&root, config.diagnostics),
            diagnostic_snapshot_revision: u64::MAX,
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_generation: 0,
            preview_request: None,
            preview_promotion: None,
            model_panel_id: None,
            last_click: None,
            label_edit: None,
            label_edit_region: helix_view::edit_region::EditRegion::default(),
            jump_session: None,
            nav: helix_view::list_nav::ListNav::new(),
        };
        panel.prewarm_search_index(editor);
        panel
    }

    fn visible_height(&self) -> usize {
        self.area
            .height
            .saturating_sub(HEADER_ROWS + SEARCH_ROWS + FOOTER_ROWS) as usize
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

    #[cfg(test)]
    fn ensure_selection_visible(&mut self) {
        self.prime_nav();
        self.nav.ensure_visible();
        self.selection = self.nav.selection();
        self.scroll = self.nav.scroll();
    }

    /// Refresh viewport constraints without pulling an off-screen selection
    /// back into view. Mouse scrolling owns the viewport until a selection
    /// command explicitly asks `ListNav` to reveal the cursor again.
    fn clamp_viewport(&mut self) {
        let scroll = helix_view::list_nav::ListViewport::new(
            self.rows.len(),
            None,
            self.visible_height(),
            self.scroll,
        )
        .clamped_scroll();
        self.nav.set_item_count(self.rows.len());
        self.nav.set_viewport_height(self.visible_height());
        self.nav.set_scroll(scroll);
        self.selection = self.nav.selection();
        self.scroll = self.nav.scroll();
    }

    fn center_selection(&mut self) {
        let start = Instant::now();
        self.prime_nav();
        if self.rows.is_empty() {
            self.sync_nav_to_cache();
            log::info!(
                "[file_explorer] center_selection rows=0 selection={} scroll={} visible_height={} elapsed_us={}",
                self.selection,
                self.scroll,
                self.visible_height(),
                start.elapsed().as_micros(),
            );
            return;
        }

        let visible_height = self.visible_height();
        if visible_height == 0 {
            self.sync_nav_to_cache();
            log::info!(
                "[file_explorer] center_selection rows={} selection={} scroll={} visible_height=0 elapsed_us={}",
                self.rows.len(),
                self.selection,
                self.scroll,
                start.elapsed().as_micros(),
            );
            return;
        }

        let target_scroll = self.selection.saturating_sub(visible_height / 2);
        self.nav.set_scroll(target_scroll);
        self.selection = self.nav.selection();
        self.scroll = self.nav.scroll();
        log::info!(
            "[file_explorer] center_selection rows={} selection={} scroll={} target_scroll={} visible_height={} selected={} elapsed_us={}",
            self.rows.len(),
            self.selection,
            self.scroll,
            target_scroll,
            visible_height,
            selected_path_for_log(&self.rows, self.selection),
            start.elapsed().as_micros(),
        );
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

    fn normalized_search_query(&self) -> String {
        self.search_query.trim().to_ascii_lowercase()
    }

    fn bump_search_generation(&mut self) -> u64 {
        self.search_generation = self.search_generation.wrapping_add(1);
        self.search_generation
    }

    fn search_count_text(&self) -> Option<String> {
        if self.search_query.is_empty() {
            return self.tree_pending.then(|| "loading".to_owned());
        }

        if self.search_pending {
            Some("searching".to_string())
        } else {
            Some(format!("{} results", self.rows.len()))
        }
    }

    pub(crate) fn search_query_for_log(&self) -> &str {
        &self.search_query
    }

    pub(crate) fn search_generation_for_log(&self) -> u64 {
        self.search_generation
    }

    pub(crate) fn search_pending_for_log(&self) -> bool {
        self.search_pending
    }

    pub(crate) fn row_count_for_log(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn selection_for_log(&self) -> usize {
        self.selection
    }

    pub(crate) fn root_for_context(&self) -> &Path {
        &self.root
    }

    pub(crate) fn selected_path_for_log(&self) -> String {
        selected_path_for_log(&self.rows, self.selection)
    }

    fn search_result_rows(&self, matches: &[PathBuf]) -> Vec<ExplorerRow> {
        let start = Instant::now();
        let mut seen = HashSet::with_capacity(matches.len());
        let rows = matches
            .iter()
            .filter_map(|path| {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    self.root.join(path)
                };
                if !path.starts_with(&self.root) || !seen.insert(path.clone()) {
                    return None;
                }

                Some(ExplorerRow {
                    label: path
                        .strip_prefix(&self.root)
                        .ok()
                        .filter(|path| !path.as_os_str().is_empty())
                        .map(display_path)
                        .unwrap_or_else(|| display_name(&path)),
                    path: path.clone(),
                    is_dir: false,
                    depth: 0,
                    expanded: false,
                    is_last: true,
                    ancestor_last: Vec::new(),
                    vcs_status: self.vcs_snapshot.status(&path),
                    diagnostic_status: self.diagnostic_snapshot.status(&path),
                })
            })
            .collect::<Vec<_>>();
        log::info!(
            "[file_explorer] search_rows_built root={} query={:?} matches={} rows={} rejected_or_deduped={} first_row={} elapsed_us={}",
            display_path(&self.root),
            self.search_query,
            matches.len(),
            rows.len(),
            matches.len().saturating_sub(rows.len()),
            rows.first()
                .map(|row| display_path(&row.path))
                .unwrap_or_else(|| String::from("<none>")),
            start.elapsed().as_micros(),
        );
        rows
    }

    fn restore_selection_after_row_update(&mut self, selected_path: Option<PathBuf>) {
        let before = self.selection;
        if self.rows.is_empty() {
            self.label_selection = LabelSelection::default();
            self.seek_to(0);
            log::info!(
                "[file_explorer] selection_restore rows=0 selection_before={} selection_after={} selected_before={} selected_after={}",
                before,
                self.selection,
                selected_path
                    .as_deref()
                    .map(display_path)
                    .unwrap_or_else(|| String::from("<none>")),
                selected_path_for_log(&self.rows, self.selection),
            );
            return;
        }

        let target = selected_path
            .as_ref()
            .and_then(|path| self.rows.iter().position(|row| row.path == *path))
            .unwrap_or_else(|| self.selection.min(self.rows.len() - 1));
        self.seek_to(target);
        self.clamp_label_selection();
        self.collapse_label_selection_to_cursor();
        log::info!(
            "[file_explorer] selection_restore rows={} selection_before={} selection_after={} selected_before={} selected_after={}",
            self.rows.len(),
            before,
            self.selection,
            selected_path
                .as_deref()
                .map(display_path)
                .unwrap_or_else(|| String::from("<none>")),
            selected_path_for_log(&self.rows, self.selection),
        );
    }

    fn apply_search_filter(&mut self, _editor: &Editor) {
        let start = Instant::now();
        let selected_path = self.selected().map(|row| row.path.clone());
        let query = self.normalized_search_query();
        let mode;
        if query.is_empty() {
            mode = "empty";
            self.search_pending = false;
            self.search_results = None;
            if let Some(expanded_dirs) = self.search_saved_expanded_dirs.take() {
                self.expanded_dirs = expanded_dirs;
            }
            self.rows = filter_explorer_rows(&self.all_rows, &self.search_query).into();
        } else if let Some(results) = self
            .search_results
            .as_ref()
            .filter(|results| results.query == query)
        {
            mode = "fff_results";
            self.rows = self.search_result_rows(&results.paths).into();
        } else if self.search_pending {
            mode = "pending";
            self.rows = Arc::from([]);
        } else {
            mode = "visible_tree_filter";
            if self.search_saved_expanded_dirs.is_none() {
                self.search_saved_expanded_dirs = Some(self.expanded_dirs.clone());
            }
            self.rows = filter_explorer_rows(&self.all_rows, &self.search_query).into();
        }

        self.restore_selection_after_row_update(selected_path);
        log::info!(
            "[file_explorer] search_filter_applied mode={} root={} query={:?} generation={} pending={} results_cached={} rows={} all_rows={} expanded_dirs={} selected={} elapsed_us={}",
            mode,
            display_path(&self.root),
            self.search_query,
            self.search_generation,
            self.search_pending,
            self.search_results
                .as_ref()
                .map_or(0, |results| results.paths.len()),
            self.rows.len(),
            self.all_rows.len(),
            self.expanded_dirs.len(),
            selected_path_for_log(&self.rows, self.selection),
            start.elapsed().as_micros(),
        );
    }

    pub(crate) fn accepts_search_request(&self, root: &Path, query: &str, generation: u64) -> bool {
        self.root == root
            && self.search_query.trim().eq_ignore_ascii_case(query)
            && self.search_generation == generation
            && !query.is_empty()
    }

    pub(crate) fn apply_search_results(
        &mut self,
        editor: &Editor,
        root: PathBuf,
        query: String,
        generation: u64,
        matches: Vec<PathBuf>,
    ) -> bool {
        let start = Instant::now();
        let match_count = matches.len();
        let first_match = matches
            .first()
            .map(|path| display_path(path))
            .unwrap_or_else(|| String::from("<none>"));
        if !self.accepts_search_request(&root, &query, generation) {
            log::info!(
                "[file_explorer] search_results_skip root={} current_root={} query={query:?} current_query={:?} generation={} current_generation={} matches={} first_match={} pending={} elapsed_us={}",
                display_path(&root),
                display_path(&self.root),
                self.search_query,
                generation,
                self.search_generation,
                match_count,
                first_match,
                self.search_pending,
                start.elapsed().as_micros(),
            );
            return false;
        }

        self.search_pending = false;
        self.search_results = Some(ExplorerSearchResults {
            query,
            paths: matches,
        });
        self.apply_search_filter(editor);
        log::info!(
            "[file_explorer] search_results_applied root={} query={:?} generation={} matches={} first_match={} rows={} selection={} selected={} elapsed_us={}",
            display_path(&self.root),
            self.search_query,
            self.search_generation,
            match_count,
            first_match,
            self.rows.len(),
            self.selection,
            selected_path_for_log(&self.rows, self.selection),
            start.elapsed().as_micros(),
        );
        true
    }

    fn start_search(&mut self) {
        log::info!(
            "[file_explorer] search_start root={} rows={} selection={} selected={} query_before={:?}",
            display_path(&self.root),
            self.rows.len(),
            self.selection,
            selected_path_for_log(&self.rows, self.selection),
            self.search_query,
        );
        self.search_active = true;
    }

    fn prewarm_search_index(&self, editor: &Editor) {
        let root = self.root.clone();
        let config = self.config.clone();
        editor
            .runtime()
            .block()
            .spawn(move || crate::fff::prewarm_file_explorer(&root, &config))
            .detach();
    }

    pub(crate) fn toggle_source_option(
        &mut self,
        option: crate::ui::file_options::FileSourceOption,
    ) {
        option.toggle_explorer(&mut self.config);
        self.search_results = None;
        log::info!(
            "[file_explorer] source_option_toggled option={option:?} hidden={} ignore={} git_ignore={} git_global={} git_exclude={} parents={} follow_symlinks={} flatten_dirs={}",
            self.config.hidden,
            self.config.ignore,
            self.config.git_ignore,
            self.config.git_global,
            self.config.git_exclude,
            self.config.parents,
            self.config.follow_symlinks,
            self.config.flatten_dirs,
        );
    }

    pub(crate) fn queue_current_search(
        &mut self,
        editor: &Editor,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        self.prewarm_search_index(editor);
        let query = self.normalized_search_query();
        if query.is_empty() {
            return;
        }
        let generation = self.bump_search_generation();
        self.search_pending = true;
        if let Err(error) = ingress.ui(UiCommand::FileExplorer(FileExplorerCommand::StartSearch {
            root: self.root.clone(),
            query,
            generation,
            config: self.config.clone(),
        })) {
            log::error!("file explorer search admission failed: {error}");
            self.search_pending = false;
        }
    }

    fn clear_search(&mut self, editor: &Editor) {
        if self.search_query.is_empty() {
            return;
        }
        let start = Instant::now();
        let query_before = self.search_query.clone();
        let generation_before = self.search_generation;
        self.bump_search_generation();
        self.search_query.clear();
        self.search_pending = false;
        self.search_results = None;
        if let Some(expanded_dirs) = self.search_saved_expanded_dirs.take() {
            self.expanded_dirs = expanded_dirs;
        }
        self.apply_search_filter(editor);
        log::info!(
            "[file_explorer] search_clear root={} query_before={query_before:?} generation_before={} generation_after={} rows={} selection={} selected={} expanded_dirs={} elapsed_us={}",
            display_path(&self.root),
            generation_before,
            self.search_generation,
            self.rows.len(),
            self.selection,
            selected_path_for_log(&self.rows, self.selection),
            self.expanded_dirs.len(),
            start.elapsed().as_micros(),
        );
    }

    fn schedule_search_filter(&mut self, cx: &mut Context) {
        let start = Instant::now();
        let query_before = self.search_query.clone();
        let rows_before = self.rows.len();
        let generation = self.bump_search_generation();
        let query = self.normalized_search_query();
        self.search_results = None;

        if query.is_empty() {
            self.search_pending = false;
            self.apply_search_filter(cx.editor);
            log::info!(
                "[file_explorer] search_schedule_empty root={} query_before={query_before:?} generation={} rows={} elapsed_us={}",
                display_path(&self.root),
                generation,
                self.rows.len(),
                start.elapsed().as_micros(),
            );
            return;
        }

        if self.search_saved_expanded_dirs.is_none() {
            self.search_saved_expanded_dirs = Some(self.expanded_dirs.clone());
        }
        self.search_pending = true;
        self.rows = Arc::from([]);
        self.label_selection = LabelSelection::default();
        self.seek_to(0);

        cx.submit_ui(UiCommand::FileExplorer(FileExplorerCommand::StartSearch {
            root: self.root.clone(),
            query,
            generation,
            config: self.config.clone(),
        }));
        log::info!(
            "[file_explorer] search_queued root={} query={:?} generation={} dispatch=immediate rows_before_clear={} rows_after_clear={} all_rows={} selected={} elapsed_us={}",
            display_path(&self.root),
            query_before,
            generation,
            rows_before,
            self.rows.len(),
            self.all_rows.len(),
            selected_path_for_log(&self.rows, self.selection),
            start.elapsed().as_micros(),
        );
    }

    fn handle_search_key(&mut self, key: KeyEvent, cx: &mut Context) -> Option<EventResult> {
        if self.search_active {
            match key {
                KeyEvent {
                    code: KeyCode::Esc | KeyCode::Enter,
                    modifiers: KeyModifiers::NONE,
                } => {
                    self.search_active = false;
                    return Some(EventResult::Consumed(None));
                }
                KeyEvent {
                    code: KeyCode::Backspace,
                    modifiers: KeyModifiers::NONE,
                } => {
                    self.search_query.pop();
                    self.schedule_search_filter(cx);
                    return Some(EventResult::Consumed(None));
                }
                KeyEvent {
                    code: KeyCode::Char('u'),
                    modifiers: KeyModifiers::CONTROL,
                } => {
                    self.clear_search(cx.editor);
                    return Some(EventResult::Consumed(None));
                }
                KeyEvent {
                    code: KeyCode::Char(ch),
                    modifiers,
                } if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    self.search_query.push(ch);
                    self.schedule_search_filter(cx);
                    return Some(EventResult::Consumed(None));
                }
                _ => return Some(EventResult::Consumed(None)),
            }
        }

        match key {
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
            } => {
                self.start_search();
                Some(EventResult::Consumed(None))
            }
            KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
            } if !self.search_query.is_empty() => {
                self.search_active = false;
                self.clear_search(cx.editor);
                Some(EventResult::Consumed(None))
            }
            _ => None,
        }
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

    fn toggle_selected_dir_state(&mut self) -> bool {
        let Some(row) = self.selected().filter(|row| row.is_dir).cloned() else {
            return false;
        };
        if row.expanded {
            self.collapse_dir_preserving_descendant_state(&row.path);
        } else {
            self.expanded_dirs.insert(row.path);
        }
        true
    }

    #[cfg(test)]
    fn toggle_selected_dir(&mut self, editor: &Editor) {
        if !self.toggle_selected_dir_state() {
            return;
        }
        if let Err(err) = self.refresh_preserving_tree(editor, None, Some(self.selection)) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    fn queue_toggle_selected_dir(&mut self, cx: &mut Context) {
        if !self.toggle_selected_dir_state() {
            return;
        }
        crate::runtime::ui::file_explorer::queue_file_explorer_tree_refresh(
            self,
            cx.editor,
            cx.ingress.clone(),
            None,
            Some(self.selection),
            None,
            false,
            false,
        );
    }

    fn replace_expanded_dirs(&mut self, dirs: HashSet<PathBuf>) {
        self.expanded_dirs = dirs.clone();
        if let Some(saved_dirs) = self.search_saved_expanded_dirs.as_mut() {
            *saved_dirs = dirs;
        }
    }

    #[cfg(test)]
    fn collapse_all_dirs(&mut self, editor: &Editor) {
        let expanded_dirs = HashSet::from([self.root.clone()]);
        self.replace_expanded_dirs(expanded_dirs);
        if let Err(err) = self.refresh_preserving_tree(editor, None, None) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    fn queue_collapse_all_dirs(&mut self, cx: &mut Context) {
        let expanded_dirs = HashSet::from([self.root.clone()]);
        self.replace_expanded_dirs(expanded_dirs);
        crate::runtime::ui::file_explorer::queue_file_explorer_tree_refresh(
            self,
            cx.editor,
            cx.ingress.clone(),
            None,
            None,
            None,
            false,
            false,
        );
    }

    #[cfg(test)]
    fn expand_loaded_dirs(&mut self, editor: &Editor) {
        self.expand_loaded_dirs_state();
        if let Err(err) = self.refresh_preserving_tree(editor, None, None) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    fn expand_loaded_dirs_state(&mut self) {
        let mut expanded_dirs = HashSet::from([self.root.clone()]);
        expanded_dirs.extend(
            self.all_rows
                .iter()
                .filter(|row| row.is_dir)
                .map(|row| row.path.clone()),
        );
        for (parent, children) in &self.children_cache {
            expanded_dirs.insert(parent.clone());
            expanded_dirs.extend(
                children
                    .iter()
                    .filter(|child| child.is_dir)
                    .map(|child| child.path.clone()),
            );
        }
        self.replace_expanded_dirs(expanded_dirs);
    }

    fn queue_expand_loaded_dirs(&mut self, cx: &mut Context) {
        self.expand_loaded_dirs_state();
        crate::runtime::ui::file_explorer::queue_file_explorer_tree_refresh(
            self,
            cx.editor,
            cx.ingress.clone(),
            None,
            None,
            None,
            false,
            false,
        );
    }

    #[cfg(test)]
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

    fn queue_collapse_or_select_parent(&mut self, cx: &mut Context) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        if row.is_dir && row.expanded {
            self.collapse_dir_preserving_descendant_state(&row.path);
            crate::runtime::ui::file_explorer::queue_file_explorer_tree_refresh(
                self,
                cx.editor,
                cx.ingress.clone(),
                None,
                Some(self.selection),
                None,
                false,
                false,
            );
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

    fn queue_root_parent(&mut self, cx: &mut Context) {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else {
            return;
        };
        crate::runtime::ui::file_explorer::queue_file_explorer_tree_refresh(
            self,
            cx.editor,
            cx.ingress.clone(),
            Some(parent),
            Some(0),
            None,
            false,
            false,
        );
    }

    fn queue_go_workspace_root(&mut self, cx: &mut Context) {
        let root = helix_loader::find_workspace().0;
        crate::runtime::ui::file_explorer::queue_file_explorer_tree_refresh(
            self,
            cx.editor,
            cx.ingress.clone(),
            Some(root),
            Some(0),
            None,
            false,
            false,
        );
    }

    fn queue_restore_tree_after_search_open(
        &mut self,
        editor: &Editor,
        ingress: crate::runtime::RuntimeIngress,
        opened_path: &Path,
    ) {
        if self.search_query.is_empty()
            && !self.search_pending
            && self.search_results.is_none()
            && self.search_saved_expanded_dirs.is_none()
        {
            return;
        }

        let query_before = self.search_query.clone();
        let generation_before = self.search_generation;
        self.bump_search_generation();
        self.search_query.clear();
        self.search_active = false;
        self.search_pending = false;
        self.search_results = None;
        if let Some(expanded_dirs) = self.search_saved_expanded_dirs.take() {
            self.expanded_dirs = expanded_dirs;
        }
        self.expand_to_path(opened_path);
        crate::runtime::ui::file_explorer::queue_file_explorer_tree_refresh(
            self,
            editor,
            ingress,
            None,
            None,
            Some(opened_path.to_path_buf()),
            false,
            false,
        );
        log::info!(
            "[file_explorer] search_open_tree_restore_queued path={} query_before={query_before:?} generation_before={} generation_after={} expanded_dirs={}",
            display_path(opened_path),
            generation_before,
            self.search_generation,
            self.expanded_dirs.len(),
        );
    }

    fn open_selected(&mut self, cx: &mut Context, action: Action) -> bool {
        let start = Instant::now();
        let Some(row) = self.selected().cloned() else {
            log::info!(
                "[file_explorer] open_skip reason=no_selection rows={} selection={} query={:?} pending={}",
                self.rows.len(),
                self.selection,
                self.search_query,
                self.search_pending,
            );
            return false;
        };

        log::info!(
            "[file_explorer] open_start path={} is_dir={} action={:?} selection={} rows={} query={:?} pending={} generation={} focused_view_before={:?} focused_doc_before={:?}",
            display_path(&row.path),
            row.is_dir,
            action,
            self.selection,
            self.rows.len(),
            self.search_query,
            self.search_pending,
            self.search_generation,
            cx.editor.focused_view_id(),
            cx.editor.focused_document_id(),
        );

        if row.is_dir {
            self.queue_toggle_selected_dir(cx);
            log::info!(
                "[file_explorer] open_done kind=directory path={} rows={} selection={} selected={} elapsed_us={}",
                display_path(&row.path),
                self.rows.len(),
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
                start.elapsed().as_micros(),
            );
            return false;
        }

        self.center_selection();

        let path = row.path.clone();
        if let Some(doc_id) = self.promote_available_preview(cx.editor, &path, action) {
            self.finish_opened_file(cx.editor, cx.ingress.clone(), &row.path, doc_id, start);
            return false;
        }

        let request = self
            .preview_request
            .as_ref()
            .filter(|request| self.preview_request_matches_selection(request))
            .cloned()
            .or_else(|| self.queue_selected_preview_request(cx.editor, cx.ingress.clone()));
        let Some(request) = request else {
            return false;
        };

        if let Some(prepared) = cx.ingress.take_file_explorer_preview(&request) {
            match prepared.result {
                Ok(document) => {
                    let doc_id = self.promote_prepared_preview(cx.editor, &path, action, document);
                    self.finish_opened_file(
                        cx.editor,
                        cx.ingress.clone(),
                        &row.path,
                        doc_id,
                        start,
                    );
                }
                Err(error) => {
                    cx.editor.set_error(error.clone());
                    log::info!(
                        "[file_explorer] open_error path={} generation={} error={} rows={} selection={} elapsed_us={}",
                        display_path(&row.path),
                        request.generation,
                        error,
                        self.rows.len(),
                        self.selection,
                        start.elapsed().as_micros(),
                    );
                }
            }
            return false;
        }

        self.preview_promotion = Some(ExplorerPreviewPromotion {
            request: request.clone(),
            action,
        });
        log::info!(
            "[file_explorer] open_waiting_for_preview path={} generation={} elapsed_us={}",
            display_path(&row.path),
            request.generation,
            start.elapsed().as_micros(),
        );
        true
    }

    fn finish_opened_file(
        &mut self,
        editor: &mut Editor,
        ingress: crate::runtime::RuntimeIngress,
        path: &Path,
        doc_id: DocumentId,
        start: Instant,
    ) {
        self.queue_restore_tree_after_search_open(editor, ingress, path);
        self.center_selection();
        self.focused = false;
        log::info!(
            "[file_explorer] open_done kind=file path={} doc={:?} preview_cache_entries={} rows={} selection={} scroll={} selected={} focused={} focused_view_after={:?} focused_doc_after={:?} documents={} elapsed_us={}",
            display_path(path),
            doc_id,
            self.preview_cache.len(),
            self.rows.len(),
            self.selection,
            self.scroll,
            selected_path_for_log(&self.rows, self.selection),
            self.focused,
            editor.focused_view_id(),
            editor.focused_document_id(),
            editor.document_count(),
            start.elapsed().as_micros(),
        );
    }

    fn promote_available_preview(
        &mut self,
        editor: &mut Editor,
        path: &Path,
        action: Action,
    ) -> Option<DocumentId> {
        let doc_id = if let Some(doc_id) = editor.document_id_by_path(path) {
            editor.promote_preview_document(doc_id);
            editor.switch(doc_id, action);
            doc_id
        } else {
            let cached = self.preview_cache.take(path)?;
            editor.restore_and_promote_preview_document(cached, action)
        };
        self.replace_preview_document(editor, doc_id, false);
        Some(doc_id)
    }

    fn promote_prepared_preview(
        &mut self,
        editor: &mut Editor,
        path: &Path,
        action: Action,
        prepared: helix_view::editor::PreparedDocumentOpen,
    ) -> DocumentId {
        if let Some(doc_id) = self.promote_available_preview(editor, path, action) {
            return doc_id;
        }

        let doc_id = editor.apply_prepared_document_open(prepared, action);
        self.replace_preview_document(editor, doc_id, true);
        editor.promote_preview_document(doc_id);
        self.replace_preview_document(editor, doc_id, false);
        doc_id
    }

    #[cfg(test)]
    pub fn preview_selected_file(&mut self, editor: &mut Editor) {
        let preview_start = Instant::now();
        let documents_before = editor.document_count();
        let component_documents_before = editor.component_docs.len();
        let focused_doc_before = editor.focused_document_id();
        let focused_view_before = editor.focused_view_id();
        let Some(row) = self.selected().filter(|row| !row.is_dir).cloned() else {
            log::info!(
                "[file_explorer] preview_skip reason=no_selected_file selection={} selected={} query={:?} pending={} generation={} preview={:?} documents={} component_documents={} elapsed_us={}",
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
                self.search_query,
                self.search_pending,
                self.search_generation,
                self.preview,
                documents_before,
                component_documents_before,
                preview_start.elapsed().as_micros(),
            );
            return;
        };
        let path = row.path.clone();
        let current_path = editor
            .tree
            .try_get(editor.tree.focus)
            .and_then(|view| editor.document(view.doc))
            .and_then(|doc| doc.path())
            .cloned();
        if current_path.as_deref() == Some(path.as_path()) {
            log::info!(
                "[file_explorer] preview_skip reason=already_current selection={} path={} query={:?} pending={} generation={} focused_view={:?} focused_doc={:?} preview={:?} documents={} component_documents={} elapsed_us={}",
                self.selection,
                display_path(&path),
                self.search_query,
                self.search_pending,
                self.search_generation,
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
            "[file_explorer] preview_open_start selection={} path={} current_path={} query={:?} pending={} generation={} focused_view={:?} focused_doc={:?} existing_doc={:?} preview_before={:?} preview_cache_entries={} documents_before={} component_documents_before={}",
            self.selection,
            display_path(&path),
            current_path
                .as_deref()
                .map(display_path)
                .unwrap_or_else(|| String::from("<scratch>")),
            self.search_query,
            self.search_pending,
            self.search_generation,
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
                    "[file_explorer] preview_done path={} doc={:?} query={:?} generation={} restored_from_cache={} preview_after={:?} preview_cache_entries={} restored_focus={:?} focused_view_before={:?} focused_doc_before={:?} focused_view_after={:?} focused_doc_after={:?} documents_after={} component_documents_after={} total_us={}",
                    display_path(&path),
                    doc_id,
                    self.search_query,
                    self.search_generation,
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
                        self.preview_cache.insert(path, doc);
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
        self.cancel_preview_request(&cx.ingress);
        if let ExplorerPreview::Owned(doc_id) = self.preview {
            cx.editor.promote_preview_document(doc_id);
        }
        self.preview = ExplorerPreview::None;
        if let Some(id) = self.model_panel_id.take() {
            cx.editor.model.remove_panel(id);
        }
        EventResult::Consumed(Some(PostAction::RemoveById(ID)))
    }

    fn cancel_preview_request(&mut self, ingress: &crate::runtime::RuntimeIngress) {
        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_request = None;
        self.preview_promotion = None;
        ingress.cancel_file_explorer_preview();
    }

    pub fn queue_selected_preview(
        &mut self,
        editor: &Editor,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        let _ = self.queue_selected_preview_request(editor, ingress);
    }

    fn queue_selected_preview_request(
        &mut self,
        _editor: &Editor,
        ingress: crate::runtime::RuntimeIngress,
    ) -> Option<FileExplorerPreviewRequest> {
        let Some(row) = self.selected().filter(|row| !row.is_dir).cloned() else {
            self.cancel_preview_request(&ingress);
            log::info!(
                "[file_explorer] preview_queue_skip reason=no_selected_file selection={} selected={}",
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
            );
            return None;
        };

        let path = row.path.clone();
        let cursor = selected_cursor(self.selection);
        if let Some(request) = self.preview_request.as_ref().filter(|request| {
            request.generation == self.preview_generation
                && request.root == self.root
                && request.path == path
                && request.cursor == cursor
        }) {
            return Some(request.clone());
        }

        ingress.cancel_file_explorer_preview();
        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_promotion = None;
        let request = FileExplorerPreviewRequest {
            root: self.root.clone(),
            path,
            cursor,
            generation: self.preview_generation,
        };
        self.preview_request = Some(request.clone());
        log::info!(
            "[file_explorer] preview_queued root={} path={} cursor={} preview_generation={} query={:?} search_pending={} search_generation={} rows={} focused={}",
            display_path(&request.root),
            display_path(&request.path),
            request.cursor,
            request.generation,
            self.search_query,
            self.search_pending,
            self.search_generation,
            self.rows.len(),
            self.focused,
        );
        if let Err(error) = ingress.ui(UiCommand::FileExplorer(
            FileExplorerCommand::PreviewSelection {
                root: request.root.clone(),
                path: request.path.clone(),
                cursor: request.cursor,
                generation: request.generation,
            },
        )) {
            log::error!("file explorer preview admission failed: {error}");
            self.preview_request = None;
            return None;
        }
        Some(request)
    }

    fn preview_request_matches_selection(&self, request: &FileExplorerPreviewRequest) -> bool {
        self.preview_request.as_ref() == Some(request)
            && request.generation == self.preview_generation
            && request.root == self.root
            && usize::try_from(request.cursor).ok() == Some(self.selection)
            && self
                .selected()
                .is_some_and(|row| !row.is_dir && row.path == request.path)
    }

    fn preview_promotion_action(&self, request: &FileExplorerPreviewRequest) -> Option<Action> {
        self.preview_promotion
            .as_ref()
            .filter(|promotion| promotion.request == *request)
            .map(|promotion| promotion.action)
    }

    fn preview_result_is_focused(
        &self,
        editor: &Editor,
        request: &FileExplorerPreviewRequest,
    ) -> bool {
        if self.preview_promotion_action(request).is_some() {
            return true;
        }
        let panel_focused = self.model_panel_id.map_or(self.focused, |panel_id| {
            editor.model.focus == FocusTarget::Panel(panel_id)
        });
        self.focused && panel_focused
    }

    pub(crate) fn apply_preview_request(
        &mut self,
        editor: &mut Editor,
        ingress: crate::runtime::RuntimeIngress,
        request: FileExplorerPreviewRequest,
    ) {
        let start = Instant::now();
        if !self.preview_request_matches_selection(&request) {
            log::info!(
                "[file_explorer] preview_request_skip reason=stale requested_root={} current_root={} requested_path={} current_selected={} requested_cursor={} current_cursor={} requested_generation={} current_generation={} elapsed_us={}",
                display_path(&request.root),
                display_path(&self.root),
                display_path(&request.path),
                selected_path_for_log(&self.rows, self.selection),
                request.cursor,
                self.selection,
                request.generation,
                self.preview_generation,
                start.elapsed().as_micros(),
            );
            return;
        }
        if !self.preview_result_is_focused(editor, &request) {
            log::info!(
                "[file_explorer] preview_request_skip reason=not_focused path={} generation={} panel_id={:?} focus={:?} elapsed_us={}",
                display_path(&request.path),
                request.generation,
                self.model_panel_id,
                editor.model.focus,
                start.elapsed().as_micros(),
            );
            return;
        }

        if let Some(action) = self.preview_promotion_action(&request) {
            if let Some(doc_id) = self.promote_available_preview(editor, &request.path, action) {
                self.finish_opened_file(editor, ingress.clone(), &request.path, doc_id, start);
                self.cancel_preview_request(&ingress);
                return;
            }
        } else if self
            .show_existing_or_cached_preview(editor, &request.path)
            .is_some()
        {
            return;
        }

        log::info!(
            "[file_explorer] preview_request_start path={} cursor={} generation={} elapsed_us={}",
            display_path(&request.path),
            request.cursor,
            request.generation,
            start.elapsed().as_micros(),
        );
        crate::runtime::ui::file_explorer::queue_file_explorer_preview(editor, ingress, request);
    }

    pub(crate) fn apply_prepared_preview(
        &mut self,
        editor: &mut Editor,
        ingress: crate::runtime::RuntimeIngress,
        request: FileExplorerPreviewRequest,
    ) {
        let start = Instant::now();
        if !self.preview_request_matches_selection(&request)
            || !self.preview_result_is_focused(editor, &request)
        {
            log::info!(
                "[file_explorer] preview_result_skip path={} requested_generation={} current_generation={} selected={} elapsed_us={}",
                display_path(&request.path),
                request.generation,
                self.preview_generation,
                selected_path_for_log(&self.rows, self.selection),
                start.elapsed().as_micros(),
            );
            return;
        }
        let Some(prepared) = ingress.take_file_explorer_preview(&request) else {
            log::info!(
                "[file_explorer] preview_result_skip path={} generation={} reason=not_available elapsed_us={}",
                display_path(&request.path),
                request.generation,
                start.elapsed().as_micros(),
            );
            return;
        };
        debug_assert_eq!(prepared.request, request);

        match prepared.result {
            Ok(document) => {
                if let Some(action) = self.preview_promotion_action(&request) {
                    let doc_id =
                        self.promote_prepared_preview(editor, &request.path, action, document);
                    self.finish_opened_file(editor, ingress.clone(), &request.path, doc_id, start);
                    self.cancel_preview_request(&ingress);
                } else {
                    let doc_id = self.install_prepared_preview(editor, &request.path, document);
                    log::info!(
                        "[file_explorer] preview_result_apply path={} generation={} doc={:?} elapsed_us={}",
                        display_path(&request.path),
                        request.generation,
                        doc_id,
                        start.elapsed().as_micros(),
                    );
                }
            }
            Err(error) => {
                editor.set_error(error.clone());
                log::info!(
                    "[file_explorer] preview_result_error path={} generation={} error={} elapsed_us={}",
                    display_path(&request.path),
                    request.generation,
                    error,
                    start.elapsed().as_micros(),
                );
                self.cancel_preview_request(&ingress);
            }
        }
    }

    fn show_existing_or_cached_preview(
        &mut self,
        editor: &mut Editor,
        path: &Path,
    ) -> Option<DocumentId> {
        let focus = editor.model.focus;
        let (doc_id, owned) = if let Some(doc_id) = editor.document_id_by_path(path) {
            if editor.focused_document_id() != doc_id {
                editor.switch(doc_id, Action::Replace);
            }
            (
                doc_id,
                matches!(self.preview, ExplorerPreview::Owned(owned) if owned == doc_id),
            )
        } else {
            let cached = self.preview_cache.take(path)?;
            (
                editor.restore_preview_document(cached, Action::Replace),
                true,
            )
        };
        self.replace_preview_document(editor, doc_id, owned);
        editor.model.focus = focus;
        self.focused = true;
        Some(doc_id)
    }

    fn install_prepared_preview(
        &mut self,
        editor: &mut Editor,
        path: &Path,
        prepared: helix_view::editor::PreparedDocumentOpen,
    ) -> DocumentId {
        if let Some(doc_id) = self.show_existing_or_cached_preview(editor, path) {
            return doc_id;
        }
        let focus = editor.model.focus;
        let doc_id = editor.apply_prepared_document_open(prepared, Action::Replace);
        self.replace_preview_document(editor, doc_id, true);
        editor.model.focus = focus;
        self.focused = true;
        doc_id
    }

    fn execute_action(&mut self, action: ExplorerAction, cx: &mut Context) -> EventResult {
        let start = Instant::now();
        let rows_before = self.rows.len();
        let selection_before = self.selection;
        let selected_before = selected_path_for_log(&self.rows, self.selection);
        let cache_before = self.children_cache.len();
        let expanded_before = self.expanded_dirs.len();
        let query_before = self.search_query.clone();
        let pending_before = self.search_pending;
        let generation_before = self.search_generation;
        let mut retain_preview_request = false;

        match action {
            ExplorerAction::Close => return self.close(cx),
            ExplorerAction::MoveSelection(delta) => self.move_selection_by(delta),
            ExplorerAction::Page(delta) => self.page_by(delta),
            ExplorerAction::SelectFirst => self.select_first(),
            ExplorerAction::SelectLast => self.select_last(),
            ExplorerAction::Open(action) => retain_preview_request = self.open_selected(cx, action),
            ExplorerAction::ToggleDirectory => self.queue_toggle_selected_dir(cx),
            ExplorerAction::CollapseAll => self.queue_collapse_all_dirs(cx),
            ExplorerAction::ExpandAll => self.queue_expand_loaded_dirs(cx),
            ExplorerAction::CollapseOrSelectParent => self.queue_collapse_or_select_parent(cx),
            ExplorerAction::RootParent => self.queue_root_parent(cx),
            ExplorerAction::GoWorkspaceRoot => self.queue_go_workspace_root(cx),
            ExplorerAction::UndoFileOperation => self.undo_file_operation(cx),
            ExplorerAction::RedoFileOperation => self.redo_file_operation(cx),
            ExplorerAction::Refresh => {
                self.queue_refresh_current(cx);
                self.queue_vcs_refresh(cx);
            }
            ExplorerAction::ShowOptions => {
                let items = crate::ui::file_options::FileSourceOption::explorer_items(&self.config);
                let ingress = cx.ingress.clone();
                let popup = crate::ui::file_options::popup(
                    "file-explorer-source-options",
                    items,
                    move |editor, option| {
                        if let Err(error) = ingress.ui(UiCommand::FileExplorer(
                            FileExplorerCommand::ToggleSourceOption { option },
                        )) {
                            editor.set_error(error.to_string());
                        }
                    },
                );
                return EventResult::Consumed(Some(PostAction::PushLayer(Box::new(popup))));
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
            if !retain_preview_request {
                self.cancel_preview_request(&cx.ingress);
            }
        } else {
            self.queue_selected_preview(cx.editor, cx.ingress.clone());
        }

        log::info!(
            "[file_explorer] action action={:?} elapsed_us={} rows_before={} rows_after={} selection_before={} selection_after={} selected_before={} selected_after={} query_before={query_before:?} query_after={:?} pending_before={} pending_after={} generation_before={} generation_after={} cache_before={} cache_after={} expanded_before={} expanded_after={}",
            action,
            start.elapsed().as_micros(),
            rows_before,
            self.rows.len(),
            selection_before,
            self.selection,
            selected_before,
            selected_path_for_log(&self.rows, self.selection),
            self.search_query,
            pending_before,
            self.search_pending,
            generation_before,
            self.search_generation,
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

        if let Some(result) = self.handle_search_key(key, cx) {
            return result;
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
        if area.width == 0 || area.height <= HEADER_ROWS + SEARCH_ROWS + FOOTER_ROWS {
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
            .clip_top(HEADER_ROWS + SEARCH_ROWS)
            .clip_bottom(FOOTER_ROWS)
            .clip_left(1);
        (list.width > 0 && list.height > 0).then_some(list)
    }

    fn search_input_area(area: Rect) -> Option<Rect> {
        if area.width == 0 || area.height <= HEADER_ROWS + FOOTER_ROWS {
            return None;
        }

        let inner = crate::widgets::Panel::edge(
            crate::widgets::PanelStyle::default(),
            crate::widgets::PanelEdge::Right,
        )
        .content_area(area);
        if inner.width <= 3 || inner.height <= HEADER_ROWS + FOOTER_ROWS {
            return None;
        }

        Some(Rect::new(
            inner.x.saturating_add(3),
            inner.y.saturating_add(HEADER_ROWS),
            inner.width.saturating_sub(4),
            SEARCH_ROWS,
        ))
    }

    fn search_cursor_area(&self, area: Rect) -> Option<Rect> {
        let mut input = Self::search_input_area(area)?;
        if let Some(count) = self.search_count_text() {
            let width = text_width(&count);
            if input.width > width.saturating_add(1) {
                input = input.clip_right(width.saturating_add(2));
            }
        }
        Some(input)
    }

    fn row_index_at_mouse(&self, event: &MouseEvent) -> Option<usize> {
        let mut list = Self::list_area(self.area)?;
        if self.rows.len() > list.height as usize {
            list = list.clip_right(1);
        }
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
            let (target, delta) = match event.kind {
                MouseEventKind::ScrollUp => (self.scroll.saturating_sub(lines), -(lines as isize)),
                MouseEventKind::ScrollDown => (self.scroll.saturating_add(lines), lines as isize),
                _ => unreachable!(),
            };
            self.scroll_to(target);
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
                self.cancel_preview_request(&cx.ingress);
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

    fn cursor_position(&self, area: Rect, _editor: &Editor) -> Option<Position> {
        if !self.focused {
            return None;
        }

        if self.search_active {
            let search_area = self.search_cursor_area(area)?;
            let state = helix_view::layout::text_input_layout(
                search_area,
                &self.search_query,
                self.search_query.len(),
            );
            if state.cursor_in_area {
                return Some(Position::new(
                    state.cursor_y as usize,
                    state.cursor_x as usize,
                ));
            }
            return None;
        }

        if self.rows.is_empty() {
            return None;
        }

        let mut list = Self::list_area(area)?;
        if self.rows.len() > list.height as usize {
            list = list.clip_right(1);
        }
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
        let label_offset = self.row_label_offset(row, self.config.icons);
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
        self.selection = self.nav.selection();
        self.scroll = self.nav.scroll();
    }

    fn content_height(&self) -> usize {
        self.rows.len()
    }
}

impl Component for FileExplorerPanel {
    fn sync(&mut self, _viewport: Rect, editor: &mut Editor) {
        let start = Instant::now();
        if self.refresh_diagnostic_snapshot(editor) {
            self.sync_row_diagnostics();
        }
        self.sync_to_model(editor);
        let elapsed = start.elapsed();
        if elapsed >= SYNC_SLOW_LOG_THRESHOLD {
            log::info!(
                "[file_explorer] sync_slow rows={} all_rows={} selection={} selected={} focused={} query={:?} search_active={} search_pending={} search_generation={} preview={:?} focused_view={:?} focused_doc={:?} documents={} component_documents={} diagnostic_entries={} elapsed_us={}",
                self.rows.len(),
                self.all_rows.len(),
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
                self.focused,
                self.search_query,
                self.search_active,
                self.search_pending,
                self.search_generation,
                self.preview,
                editor.focused_view_id(),
                editor.focused_document_id(),
                editor.document_count(),
                editor.component_docs.len(),
                self.diagnostic_snapshot.len(),
                elapsed.as_micros()
            );
        } else {
            log::trace!(
                "[file_explorer] sync rows={} all_rows={} selection={} selected={} focused={} query={:?} search_active={} search_pending={} search_generation={} preview={:?} focused_view={:?} focused_doc={:?} documents={} component_documents={} diagnostic_entries={} elapsed_us={}",
                self.rows.len(),
                self.all_rows.len(),
                self.selection,
                selected_path_for_log(&self.rows, self.selection),
                self.focused,
                self.search_query,
                self.search_active,
                self.search_pending,
                self.search_generation,
                self.preview,
                editor.focused_view_id(),
                editor.focused_document_id(),
                editor.document_count(),
                editor.component_docs.len(),
                self.diagnostic_snapshot.len(),
                elapsed.as_micros()
            );
        }
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
            let kind = if self.search_active {
                CursorKind::Bar
            } else if self.label_edit.is_some() {
                editor.config().cursor_shape.from_mode(self.input.mode)
            } else {
                CursorKind::Block
            };
            log::trace!(
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
            log::trace!(
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

        log::trace!(
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

    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let snapshot = self.render_snapshot(area, cx);
        crate::render::PreparedRender::deferred(move |cancellation| {
            let mut output = crate::render::RenderOutput::sparse(area);
            snapshot.render_surface(area, output.surface_mut(), cancellation);
            output
        })
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
    use super::path_ops::{
        display_name, parse_entry_path, relative_display, sibling_path_with_label, EntryPathError,
    };
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
        let theme_loader = helix_view::theme::Loader::new(&[]);
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
            crate::runtime::RuntimeIngress::channel(runtime.runtime().clone());
        with_context_and_ingress(editor, ingress, f)
    }

    fn with_context_and_ingress<R>(
        editor: &mut Editor,
        ingress: crate::runtime::RuntimeIngress,
        f: impl FnOnce(&mut Context<'_>) -> R,
    ) -> R {
        let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
        let idle_reset = crate::runtime::IdleResetGate::new().handle();
        let mut exit_tasks = crate::runtime::ExitTaskSet::default();
        let exit_task_work = editor.work();
        let redraw = editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events: plugin_events.into(),
        };
        let mut cx = Context::new(
            editor,
            &mut exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            idle_reset,
            crate::plugin_registry::PluginRuntime::default(),
        );
        f(&mut cx)
    }

    async fn apply_next_tree_refresh(
        panel: &mut FileExplorerPanel,
        editor: &Editor,
        ingress: &crate::runtime::RuntimeIngress,
        receiver: &mut crate::runtime::RuntimeIngressReceiver,
    ) {
        let (root, generation) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match receiver.recv().await {
                    Some(crate::runtime::ingress::RuntimeDelivery::Ui(
                        UiCommand::FileExplorer(FileExplorerCommand::ApplyTree {
                            root,
                            generation,
                        }),
                    )) => break (root, generation),
                    Some(_) => {}
                    None => panic!("runtime ingress closed before tree refresh completed"),
                }
            }
        })
        .await
        .expect("tree refresh completion");
        let prepared = ingress
            .take_file_explorer_tree(&root, generation)
            .expect("prepared tree refresh");
        assert!(panel.apply_prepared_tree(editor, prepared));
    }

    fn prepared_preview_document(
        editor: &Editor,
        path: &Path,
    ) -> helix_view::editor::PreparedDocumentOpen {
        editor
            .prepare_document_open(path, helix_view::editor::DocumentOpenRole::Preview)
            .execute()
            .expect("prepare preview document")
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

    fn mouse_scroll_down(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
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
        let uri = Uri::from(path);
        let mut diagnostics = editor
            .document_diagnostics(&uri)
            .into_iter()
            .map(|(diagnostic, _)| diagnostic)
            .collect::<Vec<_>>();
        diagnostics.push(lsp_diagnostic(severity));
        let provider = diagnostic_provider();
        editor.handle_lsp_diagnostics(&provider, uri, None, diagnostics);
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
    fn deferred_tree_refresh_applies_worker_result() {
        let fs = TempFs::new();
        fs.dir("src").file("src/main.rs", "fn main() {}");
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let editor = test_editor(100, 30, runtime.runtime());
            let mut panel = FileExplorerPanel::new_deferred(fs.root().to_path_buf(), &editor);

            assert!(panel.rows.is_empty());
            assert!(!panel.tree_pending);

            let prepared = panel
                .prepare_tree_refresh(&editor, None, None, None, false, false)
                .execute()
                .unwrap();

            assert!(panel.tree_pending);
            assert!(panel.apply_prepared_tree(&editor, prepared));
            assert!(!panel.tree_pending);
            assert!(panel
                .rows
                .iter()
                .any(|row| row.path == fs.root().join("src")));
        });
    }

    #[test]
    fn stale_tree_refresh_cannot_replace_newer_generation() {
        let fs = TempFs::new();
        fs.file("first.rs", "").file("second.rs", "");
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let editor = test_editor(100, 30, runtime.runtime());
            let mut panel = FileExplorerPanel::new_deferred(fs.root().to_path_buf(), &editor);

            let stale = panel
                .prepare_tree_refresh(&editor, None, None, None, false, false)
                .execute()
                .unwrap();
            let current = panel
                .prepare_tree_refresh(&editor, None, Some(1), None, false, false)
                .execute()
                .unwrap();

            assert!(!panel.apply_prepared_tree(&editor, stale));
            assert!(panel.tree_pending);
            assert!(panel.apply_prepared_tree(&editor, current));
            assert_eq!(panel.selection, 1);
            assert!(!panel.tree_pending);
        });
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
    fn explorer_local_keymap_exposes_source_options() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert_eq!(
            input.translate(alt!('o')),
            ExplorerInput::Execute(ExplorerAction::ShowOptions)
        );
    }

    #[test]
    fn explorer_local_keymap_has_expand_collapse_all_aliases() {
        let mut input = ExplorerInputEngine::default();
        input.prepare_test_keymaps(EditingEngineConfig::Helix);

        assert!(matches!(
            input.translate(key!('z')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('M')),
            ExplorerInput::Execute(ExplorerAction::CollapseAll)
        );
        assert!(matches!(
            input.translate(key!('z')),
            ExplorerInput::Pending(Some(_))
        ));
        assert_eq!(
            input.translate(key!('R')),
            ExplorerInput::Execute(ExplorerAction::ExpandAll)
        );

        let info = input.root_infobox().expect("explorer keymap has help");
        assert!(info.text.contains("Collapse all directories"));
        assert!(info.text.contains("Expand loaded directories"));
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

            run_file_operation(
                &mut editor,
                helix_view::editor::FileOperationRequest::create(
                    helix_view::editor::FileOperationOrigin::Command,
                    fs.path("created.rs"),
                    false,
                ),
            );
            panel.refresh_current(&editor);
            fs.assert_exists("created.rs");
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "created.rs"));

            run_file_operation(
                &mut editor,
                helix_view::editor::FileOperationRequest::move_path(
                    helix_view::editor::FileOperationOrigin::Command,
                    fs.path("created.rs"),
                    fs.path("docs/moved.rs"),
                    true,
                ),
            );
            panel.refresh_current(&editor);
            fs.assert_missing("created.rs");
            fs.assert_exists("docs/moved.rs");
            panel.selection = row_index_by_name(&panel, "docs");
            panel.toggle_selected_dir(&editor);
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "moved.rs"));

            run_file_operation(
                &mut editor,
                helix_view::editor::FileOperationRequest::copy_path(
                    helix_view::editor::FileOperationOrigin::Command,
                    fs.path("docs/moved.rs"),
                    helix_view::editor::FileOperationDestination::Exact(fs.path("docs/copy.rs")),
                ),
            );
            panel.refresh_current(&editor);
            fs.assert_exists("docs/copy.rs");
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "copy.rs"));

            run_file_operation(
                &mut editor,
                helix_view::editor::FileOperationRequest::undo(
                    helix_view::editor::FileOperationOrigin::Command,
                ),
            );
            panel.refresh_current(&editor);
            fs.assert_missing("docs/copy.rs");
            assert!(!panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "copy.rs"));

            run_file_operation(
                &mut editor,
                helix_view::editor::FileOperationRequest::redo(
                    helix_view::editor::FileOperationOrigin::Command,
                ),
            );
            panel.refresh_current(&editor);
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
    fn file_explorer_search_filters_rows_and_keeps_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("README.md"), "").unwrap();
        fs::write(temp.path().join("src").join("main.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.selection = row_index_by_name(&panel, "src");
            panel.toggle_selected_dir(&editor);

            panel.search_query = "main".to_string();
            panel.apply_search_filter(&editor);

            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == display_name(temp.path())));
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "src"));
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "main.rs"));
            assert!(!panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "README.md"));
        });
    }

    #[test]
    fn file_explorer_search_uses_fff_result_rows_and_restores_tree_on_clear() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src").join("nested").join("deep")).unwrap();
        fs::write(temp.path().join("src").join("keep.txt"), "").unwrap();
        fs::write(temp.path().join("src").join("nested").join("keep.txt"), "").unwrap();
        let target = temp
            .path()
            .join("src")
            .join("nested")
            .join("deep")
            .join("needle.rs");
        fs::write(&target, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            let src = helix_stdx::path::normalize(temp.path().join("src"));
            let nested = src.join("nested");
            let deep = nested.join("deep");
            let target = helix_stdx::path::normalize(&target);
            assert!(!panel.expanded_dirs.contains(&src));
            assert!(!panel.rows.iter().any(|row| row.path == target));

            panel.search_query = "needle".to_string();
            panel.apply_search_results(
                &editor,
                helix_stdx::path::normalize(temp.path()),
                "needle".to_string(),
                panel.search_generation,
                vec![target.clone()],
            );

            assert_eq!(panel.rows.len(), 1);
            assert!(panel.rows.iter().any(|row| row.path == target));
            assert_eq!(panel.rows[0].label, "src/nested/deep/needle.rs");
            assert!(!panel.expanded_dirs.contains(&src));
            assert!(!panel.expanded_dirs.contains(&nested));
            assert!(!panel.expanded_dirs.contains(&deep));

            panel.clear_search(&editor);

            assert!(!panel.expanded_dirs.contains(&src));
            assert!(!panel.expanded_dirs.contains(&nested));
            assert!(!panel.expanded_dirs.contains(&deep));
            assert!(panel
                .rows
                .iter()
                .any(|row| row.path == src && row.is_dir && !row.expanded));
            assert!(!panel.rows.iter().any(|row| row.path == target));
        });
    }

    #[test]
    fn file_explorer_search_key_input_filters_and_escape_clears() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        fs::write(temp.path().join("beta.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let all_rows = panel.rows.len();

            press_key(&mut panel, &mut editor, &rt, key!('/'));
            assert!(panel.search_active);

            for ch in ['a', 'l', 'p', 'h', 'a'] {
                press_key(
                    &mut panel,
                    &mut editor,
                    &rt,
                    KeyEvent {
                        code: KeyCode::Char(ch),
                        modifiers: KeyModifiers::NONE,
                    },
                );
            }
            assert_eq!(panel.search_query, "alpha");
            assert!(panel.search_pending);
            panel.apply_search_results(
                &editor,
                helix_stdx::path::normalize(temp.path()),
                "alpha".to_string(),
                panel.search_generation,
                vec![helix_stdx::path::normalize(temp.path().join("alpha.rs"))],
            );
            assert!(!panel.search_pending);
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "alpha.rs"));
            assert!(!panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "beta.rs"));

            press_key(&mut panel, &mut editor, &rt, key!(Esc));
            assert!(!panel.search_active);
            assert_eq!(panel.search_query, "alpha");

            press_key(&mut panel, &mut editor, &rt, key!(Esc));
            assert!(panel.search_query.is_empty());
            assert_eq!(panel.rows.len(), all_rows);
        });
    }

    #[test]
    fn opening_search_result_restores_tree_to_opened_file() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src").join("nested")).unwrap();
        let target = temp.path().join("src").join("nested").join("needle.rs");
        fs::write(&target, "fn needle() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let root = helix_stdx::path::normalize(temp.path());
            let target = helix_stdx::path::normalize(&target);
            let src = root.join("src");
            let nested = src.join("nested");

            panel.search_query = "needle".to_string();
            assert!(panel.apply_search_results(
                &editor,
                root.clone(),
                "needle".to_string(),
                panel.search_generation,
                vec![target.clone()],
            ));
            assert_eq!(panel.rows.len(), 1);

            let (ingress, mut receiver) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            let request = panel
                .queue_selected_preview_request(&editor, ingress.clone())
                .expect("search result preview request");
            ingress.store_file_explorer_preview(
                crate::runtime::ui::file_explorer::PreparedFileExplorerPreview {
                    request,
                    result: Ok(prepared_preview_document(&editor, &target)),
                },
            );
            with_context_and_ingress(&mut editor, ingress.clone(), |cx| {
                panel.open_selected(cx, Action::Replace);
            });
            apply_next_tree_refresh(&mut panel, &editor, &ingress, &mut receiver).await;

            assert!(panel.search_query.is_empty());
            assert!(!panel.search_pending);
            assert!(panel.expanded_dirs.contains(&src));
            assert!(panel.expanded_dirs.contains(&nested));
            assert_eq!(
                panel.selected().map(|row| row.path.as_path()),
                Some(target.as_path())
            );
        });
    }

    #[test]
    fn opening_search_result_centers_opened_file_in_restored_tree() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..12 {
            fs::create_dir(temp.path().join(format!("d{index:02}"))).unwrap();
            fs::create_dir(temp.path().join(format!("z{index:02}"))).unwrap();
        }
        fs::create_dir_all(temp.path().join("src").join("nested")).unwrap();
        let target = temp.path().join("src").join("nested").join("needle.rs");
        fs::write(&target, "fn needle() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.area = Rect::new(0, 0, 40, 9);
            let root = helix_stdx::path::normalize(temp.path());
            let target = helix_stdx::path::normalize(&target);

            panel.search_query = "needle".to_string();
            assert!(panel.apply_search_results(
                &editor,
                root,
                "needle".to_string(),
                panel.search_generation,
                vec![target.clone()],
            ));

            let (ingress, mut receiver) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            let request = panel
                .queue_selected_preview_request(&editor, ingress.clone())
                .expect("search result preview request");
            ingress.store_file_explorer_preview(
                crate::runtime::ui::file_explorer::PreparedFileExplorerPreview {
                    request,
                    result: Ok(prepared_preview_document(&editor, &target)),
                },
            );
            with_context_and_ingress(&mut editor, ingress.clone(), |cx| {
                panel.open_selected(cx, Action::Replace);
            });
            apply_next_tree_refresh(&mut panel, &editor, &ingress, &mut receiver).await;

            assert_eq!(
                panel.selected().map(|row| row.path.as_path()),
                Some(target.as_path())
            );
            let visible_height = panel.visible_height();
            assert_eq!(visible_height, 6);
            assert!(panel.selection >= panel.scroll);
            assert!(panel.selection < panel.scroll + visible_height);
            assert_eq!(panel.selection - panel.scroll, visible_height / 2);
        });
    }

    #[test]
    fn file_explorer_search_input_enqueues_async_search_immediately() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::new_paused();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let (ingress, mut rx) = crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
            let idle_reset = crate::runtime::IdleResetGate::new().handle();
            let mut exit_tasks = crate::runtime::ExitTaskSet::default();
            let exit_task_work = editor.work();
            let redraw = editor.redraw_handle();
            let notifier = crate::handlers::local::Notifier {
                redraw: redraw.clone(),
                plugin_events: plugin_events.into(),
            };

            {
                let mut cx = Context::new(
                    &mut editor,
                    &mut exit_tasks,
                    exit_task_work,
                    notifier,
                    ingress,
                    idle_reset,
                    crate::plugin_registry::PluginRuntime::default(),
                );
                assert!(matches!(
                    panel.handle_event(&Event::Key(key!('/')), &mut cx),
                    EventResult::Consumed(_)
                ));
                assert!(matches!(
                    panel.handle_event(&Event::Key(key!('a')), &mut cx),
                    EventResult::Consumed(_)
                ));
            }

            assert_eq!(panel.search_query, "a");
            assert!(panel.search_pending);
            assert!(panel.rows.is_empty());

            let delivery = rx.try_recv().expect("immediate search command");
            let crate::runtime::ingress::RuntimeDelivery::Ui(UiCommand::FileExplorer(
                FileExplorerCommand::StartSearch {
                    root,
                    query,
                    generation,
                    ..
                },
            )) = delivery
            else {
                panic!("expected file explorer search command");
            };
            assert_eq!(root, helix_stdx::path::normalize(temp.path()));
            assert_eq!(query, "a");
            assert_eq!(generation, panel.search_generation);
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
    fn opening_current_preview_promotes_existing_preview_document() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main.rs");
        fs::write(&main, "fn main() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            panel.selection = row_index_by_name(&panel, "main.rs");
            panel.preview_selected_file(&mut editor);
            let preview_doc = editor.focused_document_id();
            assert!(editor
                .document(preview_doc)
                .is_some_and(|doc| doc.is_preview()));

            with_context(&mut editor, &rt, |cx| {
                panel.open_selected(cx, Action::Replace);
            });

            assert_eq!(editor.focused_document_id(), preview_doc);
            assert!(editor
                .document(preview_doc)
                .is_some_and(|doc| !doc.is_preview()));
            assert_eq!(panel.preview, ExplorerPreview::None);
            assert_eq!(editor.document_count(), 1);
        });
    }

    #[test]
    fn opening_cached_preview_restores_and_promotes_cached_document() {
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

            panel.selection = row_index_by_name(&panel, "beta.rs");
            panel.preview_selected_file(&mut editor);
            let second_doc = editor.focused_document_id();
            assert_ne!(first_doc, second_doc);
            assert!(panel.preview_cache.contains_path(&first));
            assert!(matches!(panel.preview, ExplorerPreview::Owned(id) if id == second_doc));

            panel.selection = row_index_by_name(&panel, "alpha.rs");
            with_context(&mut editor, &rt, |cx| {
                panel.open_selected(cx, Action::Replace);
            });

            assert_eq!(editor.focused_document_id(), first_doc);
            assert!(editor.contains_document(first_doc));
            assert!(!editor.contains_document(second_doc));
            assert!(editor
                .document(first_doc)
                .is_some_and(|doc| !doc.is_preview()));
            assert_eq!(panel.preview, ExplorerPreview::None);
            assert!(!panel.preview_cache.contains_path(&first));
            assert!(panel.preview_cache.contains_path(&second));
            assert_eq!(editor.document_count(), 1);
        });
    }

    #[test]
    fn rapid_selection_churn_dispatches_immediately_and_keeps_latest_generation() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("alpha.rs");
        let second = temp.path().join("beta.rs");
        fs::write(&first, "fn alpha() {}\n").unwrap();
        fs::write(&second, "fn beta() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::new_paused();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let (ingress, mut rx) = crate::runtime::RuntimeIngress::channel(rt.runtime().clone());

            panel.selection = row_index_by_name(&panel, "alpha.rs");
            let first_request = panel
                .queue_selected_preview_request(&editor, ingress.clone())
                .expect("first request");
            panel.selection = row_index_by_name(&panel, "beta.rs");
            let second_request = panel
                .queue_selected_preview_request(&editor, ingress)
                .expect("second request");

            let mut seen = Vec::new();
            while let Ok(delivery) = rx.try_recv() {
                if let crate::runtime::ingress::RuntimeDelivery::Ui(UiCommand::FileExplorer(
                    FileExplorerCommand::PreviewSelection {
                        path,
                        cursor,
                        generation,
                        ..
                    },
                )) = delivery
                {
                    seen.push((path, cursor, generation));
                }
            }
            assert_eq!(
                seen,
                vec![
                    (
                        helix_stdx::path::canonicalize(&first),
                        u32::try_from(row_index_by_name(&panel, "alpha.rs")).unwrap(),
                        first_request.generation,
                    ),
                    (
                        helix_stdx::path::canonicalize(&second),
                        u32::try_from(row_index_by_name(&panel, "beta.rs")).unwrap(),
                        second_request.generation,
                    ),
                ],
                "selection changes should not be delayed"
            );
            assert!(second_request.generation > first_request.generation);
            assert_eq!(panel.preview_request.as_ref(), Some(&second_request));
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
                crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            panel.selection = row_index_by_name(&panel, "alpha.rs");
            let active = panel
                .queue_selected_preview_request(&editor, ingress.clone())
                .expect("active preview request");
            let stale = FileExplorerPreviewRequest {
                path: helix_stdx::path::canonicalize(&second),
                ..active
            };

            panel.apply_preview_request(&mut editor, ingress, stale);

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
    fn stale_prepared_preview_result_is_rejected_without_consuming_it() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("alpha.rs");
        let second = temp.path().join("beta.rs");
        fs::write(&first, "fn alpha() {}\n").unwrap();
        fs::write(&second, "fn beta() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let (ingress, _receiver) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            panel.selection = row_index_by_name(&panel, "alpha.rs");
            let request = panel
                .queue_selected_preview_request(&editor, ingress.clone())
                .expect("preview request");
            panel.selection = row_index_by_name(&panel, "beta.rs");
            let current_request = panel
                .queue_selected_preview_request(&editor, ingress.clone())
                .expect("newer preview request");
            ingress.store_file_explorer_preview(
                crate::runtime::ui::file_explorer::PreparedFileExplorerPreview {
                    request: request.clone(),
                    result: Ok(prepared_preview_document(&editor, &first)),
                },
            );

            panel.apply_prepared_preview(&mut editor, ingress.clone(), request.clone());

            assert_ne!(
                editor
                    .focused_document()
                    .and_then(|document| document.path())
                    .map(PathBuf::as_path),
                Some(first.as_path())
            );
            assert_eq!(panel.preview_request.as_ref(), Some(&current_request));
            assert!(ingress.take_file_explorer_preview(&request).is_some());
        });
    }

    #[test]
    fn cancel_preview_request_clears_matching_prepared_result() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let (ingress, _receiver) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            panel.seek_to(row_index_by_name(&panel, "main.rs"));
            let request = panel
                .queue_selected_preview_request(&editor, ingress.clone())
                .expect("preview request");
            ingress.store_file_explorer_preview(
                crate::runtime::ui::file_explorer::PreparedFileExplorerPreview {
                    request: request.clone(),
                    result: Ok(prepared_preview_document(&editor, &path)),
                },
            );

            panel.cancel_preview_request(&ingress);

            assert!(panel.preview_request.is_none());
            assert!(panel.preview_generation > request.generation);
            assert!(ingress.take_file_explorer_preview(&request).is_none());
        });
    }

    #[test]
    fn first_search_result_queues_preview_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("alpha.rs");
        fs::write(&first, "fn alpha() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::new_paused();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let (ingress, mut receiver) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            panel.search_query = String::from("alpha");
            let generation = panel.search_generation;
            assert!(panel.apply_search_results(
                &editor,
                helix_stdx::path::normalize(temp.path()),
                String::from("alpha"),
                generation,
                vec![first.clone()],
            ));

            panel.queue_selected_preview(&editor, ingress);

            let crate::runtime::ingress::RuntimeDelivery::Ui(UiCommand::FileExplorer(
                FileExplorerCommand::PreviewSelection { path, .. },
            )) = receiver
                .try_recv()
                .expect("preview command without timer advance")
            else {
                panic!("expected immediate first-result preview");
            };
            assert_eq!(path, helix_stdx::path::canonicalize(first));
        });
    }

    #[test]
    fn enter_promotes_matching_prepared_preview_without_reopening_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("main.rs");
        fs::write(&path, "prepared\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            let (ingress, _receiver) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            panel.seek_to(row_index_by_name(&panel, "main.rs"));
            let request = panel
                .queue_selected_preview_request(&editor, ingress.clone())
                .expect("preview request");
            let prepared = prepared_preview_document(&editor, &path);
            assert_eq!(prepared.document().text().to_string(), "prepared\n");
            ingress.store_file_explorer_preview(
                crate::runtime::ui::file_explorer::PreparedFileExplorerPreview {
                    request,
                    result: Ok(prepared),
                },
            );
            fs::write(&path, "changed after preparation\n").unwrap();

            with_context_and_ingress(&mut editor, ingress, |cx| {
                assert!(!panel.open_selected(cx, Action::Replace));
            });

            let promoted_id = editor
                .document_id_by_path(&path)
                .expect("promoted document path");
            assert_eq!(editor.focused_document_id(), promoted_id);
            let document = editor.document(promoted_id).expect("promoted document");
            assert_eq!(document.text().to_string(), "prepared\n");
            assert!(!document.is_preview());
            assert_eq!(panel.preview, ExplorerPreview::None);
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
    fn collapse_and_expand_all_update_loaded_tree_state() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        let nested = src.join("nested");
        let deep = nested.join("deep");
        fs::create_dir_all(&deep).unwrap();
        fs::write(src.join("lib.rs"), "").unwrap();
        fs::write(nested.join("side.rs"), "").unwrap();
        fs::write(deep.join("main.rs"), "").unwrap();
        fs::write(temp.path().join("README.md"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            panel.selection = row_index_by_name(&panel, "src");
            panel.toggle_selected_dir(&editor);
            panel.selection = row_index_by_name(&panel, "nested");
            panel.toggle_selected_dir(&editor);

            let src = helix_stdx::path::normalize(&src);
            let nested = helix_stdx::path::normalize(&nested);
            let deep = helix_stdx::path::normalize(&deep);
            assert!(panel.expanded_dirs.contains(&src));
            assert!(panel.expanded_dirs.contains(&nested));
            assert!(panel.rows.iter().any(|row| row.path == deep));

            panel.collapse_all_dirs(&editor);
            assert!(panel.expanded_dirs.contains(&panel.root));
            assert!(!panel.expanded_dirs.contains(&src));
            assert!(!panel.expanded_dirs.contains(&nested));
            assert!(!panel.rows.iter().any(|row| row.path == deep));

            panel.expand_loaded_dirs(&editor);
            assert!(panel.expanded_dirs.contains(&src));
            assert!(panel.expanded_dirs.contains(&nested));
            assert!(panel.rows.iter().any(|row| row.path == deep));
        });
    }

    #[test]
    fn mouse_wheel_scrolls_viewport_without_moving_selection() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..20 {
            fs::write(temp.path().join(format!("file-{index:02}.txt")), "").unwrap();
        }
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            panel.area = Rect::new(0, 0, 40, 9);
            let list = FileExplorerPanel::list_area(panel.area).unwrap();
            let selection = panel.selection;

            let event = mouse_scroll_down(list.x, list.y);
            with_context(&mut editor, &rt, |cx| {
                assert!(matches!(
                    panel.handle_mouse_at(&event, cx, Instant::now()),
                    EventResult::Consumed(None)
                ));
            });

            assert_eq!(panel.selection, selection);
            assert!(panel.scroll > 0);
            let scroll = panel.scroll;
            panel.clamp_viewport();
            assert_eq!(
                panel.scroll, scroll,
                "render sync must preserve wheel scroll"
            );

            let gutter = mouse_down(list.right().saturating_sub(1), list.y);
            assert_eq!(panel.row_index_at_mouse(&gutter), None);
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

            let (ingress, mut ingress_rx) =
                crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
            let idle_reset = crate::runtime::IdleResetGate::new().handle();
            let mut exit_tasks = crate::runtime::ExitTaskSet::default();
            let exit_task_work = editor.work();
            let redraw = editor.redraw_handle();
            let notifier = crate::handlers::local::Notifier {
                redraw: redraw.clone(),
                plugin_events: plugin_events.into(),
            };
            let mut cx = Context::new(
                &mut editor,
                &mut exit_tasks,
                exit_task_work,
                notifier,
                ingress.clone(),
                idle_reset,
                crate::plugin_registry::PluginRuntime::default(),
            );

            assert!(matches!(
                panel.handle_mouse_at(&event, &mut cx, first_click),
                EventResult::Consumed(None)
            ));
            assert!(matches!(
                panel.handle_mouse_at(&event, &mut cx, first_click + Duration::from_millis(100)),
                EventResult::Consumed(None)
            ));
            drop(cx);
            apply_next_tree_refresh(&mut panel, &editor, &ingress, &mut ingress_rx).await;
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
            let theme_loader = helix_view::theme::Loader::new(&[]);
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
            let theme_loader = helix_view::theme::Loader::new(&[]);
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
    fn inline_entry_paths_are_relative_and_cannot_traverse() {
        assert_eq!(
            parse_entry_path("../outside.rs"),
            Err(EntryPathError::Traversal)
        );
        assert_eq!(
            parse_entry_path("nested/../outside.rs"),
            Err(EntryPathError::Traversal)
        );
        assert_eq!(
            parse_entry_path("/absolute.rs"),
            Err(EntryPathError::Absolute)
        );
        assert_eq!(
            parse_entry_path("C:\\absolute.rs"),
            Err(EntryPathError::Absolute)
        );

        let entry = parse_entry_path("nested/source/").unwrap();
        assert_eq!(entry.relative, PathBuf::from("nested").join("source"));
        assert!(entry.is_dir);
        assert_eq!(
            windows_reserved_path(&PathBuf::from("nested").join("NUL.txt")),
            Some("NUL")
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
                crate::runtime::RuntimeIngress::channel(rt.runtime().clone());
            let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
            let idle_reset = crate::runtime::IdleResetGate::new().handle();
            let mut exit_tasks = crate::runtime::ExitTaskSet::default();
            let exit_task_work = editor.work();
            let redraw = editor.redraw_handle();
            let notifier = crate::handlers::local::Notifier {
                redraw: redraw.clone(),
                plugin_events: plugin_events.into(),
            };
            let mut cx = Context::new(
                &mut editor,
                &mut exit_tasks,
                exit_task_work,
                notifier,
                ingress,
                idle_reset,
                crate::plugin_registry::PluginRuntime::default(),
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
            config: FileExplorerConfig::default(),
            all_rows: Arc::from([]),
            rows: Arc::from([]),
            search_query: String::new(),
            search_active: false,
            search_pending: false,
            search_generation: 0,
            search_results: None,
            tree_generation: 0,
            tree_pending: false,
            expanded_dirs: HashSet::new(),
            search_saved_expanded_dirs: None,
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::default(),
            diagnostic_snapshot: DiagnosticSnapshot::default(),
            diagnostic_snapshot_revision: 0,
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_generation: 0,
            preview_request: None,
            preview_promotion: None,
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
            config: FileExplorerConfig::default(),
            all_rows: Arc::from([]),
            rows: Arc::from([]),
            search_query: String::new(),
            search_active: false,
            search_pending: false,
            search_generation: 0,
            search_results: None,
            tree_generation: 0,
            tree_pending: false,
            expanded_dirs: HashSet::new(),
            search_saved_expanded_dirs: None,
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::default(),
            diagnostic_snapshot: DiagnosticSnapshot::default(),
            diagnostic_snapshot_revision: 0,
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_generation: 0,
            preview_request: None,
            preview_promotion: None,
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
            config: FileExplorerConfig::default(),
            all_rows: Arc::from([]),
            rows: Arc::from([]),
            search_query: String::new(),
            search_active: false,
            search_pending: false,
            search_generation: 0,
            search_results: None,
            tree_generation: 0,
            tree_pending: false,
            expanded_dirs: HashSet::new(),
            search_saved_expanded_dirs: None,
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::default(),
            diagnostic_snapshot: DiagnosticSnapshot::default(),
            diagnostic_snapshot_revision: 0,
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_generation: 0,
            preview_request: None,
            preview_promotion: None,
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
            config: FileExplorerConfig::default(),
            all_rows: Arc::from([]),
            rows: Arc::from([]),
            search_query: String::new(),
            search_active: false,
            search_pending: false,
            search_generation: 0,
            search_results: None,
            tree_generation: 0,
            tree_pending: false,
            expanded_dirs: HashSet::new(),
            search_saved_expanded_dirs: None,
            children_cache: HashMap::new(),
            vcs_snapshot: VcsSnapshot::default(),
            diagnostic_snapshot: DiagnosticSnapshot::default(),
            diagnostic_snapshot_revision: 0,
            input: ExplorerInputEngine::default(),
            file_clipboard: None,
            selection: 0,
            label_selection: LabelSelection::default(),
            scroll: 0,
            area: Rect::default(),
            focused: true,
            preview: ExplorerPreview::None,
            preview_cache: PreviewDocumentCache::default(),
            preview_generation: 0,
            preview_request: None,
            preview_promotion: None,
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
            let snapshot = DiagnosticSnapshot::from_editor(&root, &editor, true);
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

            let unrelated = temp.path().join("unrelated.rs");
            fs::write(&unrelated, "").unwrap();
            panel
                .children_cache
                .remove(&helix_stdx::path::normalize(temp.path()));

            add_diagnostic(&mut editor, &file, LspDiagnosticSeverity::WARNING);
            panel.sync(Rect::new(0, 0, 120, 40), &mut editor);

            let index = row_index_by_name(&panel, "main.rs");
            assert_eq!(
                panel.rows[index].diagnostic_status,
                Some(DiagnosticStatus {
                    severity: DiagnosticSeverity::Warning,
                    count: 1,
                })
            );
            assert!(
                panel.rows.iter().all(|row| row.path != unrelated),
                "diagnostic-only sync rebuilt the file tree"
            );
        });
    }

    #[test]
    fn diagnostic_snapshot_refresh_is_revision_gated() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("main.rs");
        fs::write(&file, "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            assert_eq!(
                panel.diagnostic_snapshot_revision,
                editor.diagnostics_revision()
            );
            assert!(!panel.refresh_diagnostic_snapshot(&editor));

            add_diagnostic(&mut editor, &file, LspDiagnosticSeverity::WARNING);
            assert!(panel.refresh_diagnostic_snapshot(&editor));
            assert_eq!(
                panel.diagnostic_snapshot_revision,
                editor.diagnostics_revision()
            );
            assert!(!panel.refresh_diagnostic_snapshot(&editor));
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

            panel.sync(Rect::new(0, 0, 120, 40), &mut editor);

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
    fn inline_rename_submission_does_not_mutate_disk_on_the_ui_thread() {
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

            // Committing the edit submits the operation. The terminal FIFO
            // owns inspection, LSP will*, and mutation after this point.
            assert!(alpha.exists(), "old path remains until worker completion");
            assert!(!beta.exists(), "new path is not created on the UI thread");
            assert!(panel.label_edit.is_none());
            assert_eq!(panel.input.mode, Mode::Normal);
        });
    }

    #[test]
    fn inline_rename_with_slash_defers_parent_creation_to_the_worker() {
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

            // Parent inspection and creation are part of the blocking
            // operation, so this UI-only path must not touch the filesystem.
            let nested = temp.path().join("nested");
            let inner = nested.join("inner");
            let beta = inner.join("beta.rs");
            assert!(!nested.exists(), "intermediate dir nested/ deferred");
            assert!(!inner.exists(), "intermediate dir nested/inner/ deferred");
            assert!(!beta.exists(), "leaf file move is deferred");
            assert!(alpha.exists(), "old path remains until worker completion");
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
            assert!(alpha.exists());
            assert!(!xalpha.exists());
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

    fn run_file_operation(editor: &mut Editor, request: helix_view::editor::FileOperationRequest) {
        let id = editor.enqueue_file_operation(request);
        let helix_view::editor::FileOperationDispatch::Inspect(inspection) = editor
            .next_file_operation_dispatch()
            .expect("operation starts")
        else {
            panic!("operation should begin with inspection");
        };
        assert_eq!(inspection.id(), id);
        let prepared = inspection.execute().expect("operation inspection succeeds");
        editor
            .accept_file_operation_preparation(prepared)
            .expect("prepared operation accepted");
        let work = editor
            .begin_file_operation_mutation(id)
            .expect("operation mutation starts");
        let completion = editor
            .finish_file_operation(work.execute())
            .expect("operation completion accepted")
            .into_iter()
            .next()
            .expect("operation completion present");
        assert!(
            completion.result.is_ok(),
            "operation failed: {:?}",
            completion.result
        );
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
            run_file_operation(
                &mut editor,
                helix_view::editor::FileOperationRequest::move_path(
                    helix_view::editor::FileOperationOrigin::Command,
                    alpha.clone(),
                    beta.clone(),
                    true,
                ),
            );
            assert!(beta.exists() && !alpha.exists());

            run_file_operation(
                &mut editor,
                helix_view::editor::FileOperationRequest::undo(
                    helix_view::editor::FileOperationOrigin::Command,
                ),
            );
            assert!(alpha.exists() && !beta.exists());

            run_file_operation(
                &mut editor,
                helix_view::editor::FileOperationRequest::redo(
                    helix_view::editor::FileOperationOrigin::Command,
                ),
            );
            assert!(beta.exists() && !alpha.exists());
        });
    }
}
