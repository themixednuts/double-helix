//! Adapters that convert `helix-view` / `helix-core` internal types into
//! host-agnostic contract types.
//!
//! These adapters live inside `helix-plugin` (which already depends on
//! `helix-view` and `helix-core`) and form the bridge between editor internals
//! and the public contract. The Lua facade and any future language host call
//! through the contract types; these adapters are the only place that knows
//! about the internal representations.

use std::num::NonZeroU64;

use helix_core::diagnostic::Severity;
use helix_view::{DocumentId, Editor, ViewId};
use slotmap::Key as _;

use super::errors::ContractError;
use super::handles::{DocumentHandle, FloatHandle, PanelHandle, ThreadHandle, ViewHandle};
use super::snapshots;

// ---------------------------------------------------------------------------
// Handle conversion
// ---------------------------------------------------------------------------

/// Convert a `DocumentId` to a contract `DocumentHandle`.
///
/// `DocumentId` is backed by `NonZeroUsize`, so the value is always > 0 and
/// safe to wrap in `NonZeroU64`.
pub fn document_handle(id: DocumentId) -> DocumentHandle {
    let raw = id.value().get() as u64;
    DocumentHandle::from_raw(NonZeroU64::new(raw).expect("DocumentId is always non-zero"))
}

/// Convert a `ViewId` (slotmap key) to a contract `ViewHandle`.
///
/// Slotmap's `as_ffi()` returns a `u64` encoding both index and version.
/// The value is non-zero for valid keys.
pub fn view_handle(id: ViewId) -> ViewHandle {
    let raw = id.data().as_ffi();
    ViewHandle::from_raw(NonZeroU64::new(raw).expect("valid ViewId has non-zero ffi value"))
}

/// Try to resolve a `DocumentHandle` back to a `DocumentId`.
///
/// This iterates the editor's documents to find a matching ID. Returns
/// `ContractError::StaleHandle` if no document matches.
pub fn resolve_document(
    editor: &Editor,
    handle: DocumentHandle,
) -> Result<DocumentId, ContractError> {
    let target = handle.raw().get() as usize;
    editor
        .documents
        .keys()
        .find(|id| id.value().get() == target)
        .copied()
        .ok_or_else(|| ContractError::stale_handle(handle.to_string()))
}

/// Try to resolve a `ViewHandle` back to a `ViewId`.
///
/// Uses slotmap's `from_ffi` to reconstruct the key, then checks that the
/// view actually exists in the tree. Returns `ContractError::StaleHandle` if
/// the view is gone.
pub fn resolve_view(editor: &Editor, handle: ViewHandle) -> Result<ViewId, ContractError> {
    let raw = handle.raw().get();
    let key = ViewId::from(slotmap::KeyData::from_ffi(raw));
    if editor.tree.contains(key) {
        Ok(key)
    } else {
        Err(ContractError::stale_handle(handle.to_string()))
    }
}

/// Convert a `FloatId` (slotmap key) to a contract `FloatHandle`.
pub fn float_handle(id: helix_view::model::FloatId) -> FloatHandle {
    let raw = id.data().as_ffi();
    FloatHandle::from_raw(NonZeroU64::new(raw).expect("valid FloatId has non-zero ffi value"))
}

/// Convert a `PanelId` (slotmap key) to a contract `PanelHandle`.
pub fn panel_handle(id: helix_view::model::PanelId) -> PanelHandle {
    let raw = id.data().as_ffi();
    PanelHandle::from_raw(NonZeroU64::new(raw).expect("valid PanelId has non-zero ffi value"))
}

/// Try to resolve a `FloatHandle` back to a `FloatId`.
pub fn resolve_float(
    model: &helix_view::model::Model,
    handle: FloatHandle,
) -> Result<helix_view::model::FloatId, ContractError> {
    let raw = handle.raw().get();
    let key = helix_view::model::FloatId::from(slotmap::KeyData::from_ffi(raw));
    if model.floats.contains_key(key) {
        Ok(key)
    } else {
        Err(ContractError::stale_handle(handle.to_string()))
    }
}

/// Try to resolve a `PanelHandle` back to a `PanelId`.
pub fn resolve_panel(
    model: &helix_view::model::Model,
    handle: PanelHandle,
) -> Result<helix_view::model::PanelId, ContractError> {
    let raw = handle.raw().get();
    let key = helix_view::model::PanelId::from(slotmap::KeyData::from_ffi(raw));
    if model.panels.contains_key(key) {
        Ok(key)
    } else {
        Err(ContractError::stale_handle(handle.to_string()))
    }
}

/// Convert internal `PanelSide` to contract `PanelSide`.
pub fn panel_side_to_contract(side: helix_view::model::PanelSide) -> super::requests::PanelSide {
    match side {
        helix_view::model::PanelSide::Left => super::requests::PanelSide::Left,
        helix_view::model::PanelSide::Right => super::requests::PanelSide::Right,
        helix_view::model::PanelSide::Bottom => super::requests::PanelSide::Bottom,
    }
}

// ---------------------------------------------------------------------------
// Snapshot builders
// ---------------------------------------------------------------------------

/// Build a `DocumentSnapshot` from a `Document`.
///
/// `view_id` is needed to extract the selection for the relevant view. If
/// the document has no selection for the given view (e.g., view was just
/// created), selections will be empty.
///
/// `mode` is the editor-level mode (not per-document).
pub fn document_snapshot(
    doc: &helix_view::Document,
    view_id: ViewId,
    mode: helix_view::document::Mode,
) -> snapshots::DocumentSnapshot {
    let text = doc.text();
    let selections = doc
        .selections()
        .get(&view_id)
        .map(|sel| {
            sel.ranges()
                .iter()
                .map(|r| char_range_to_selection(text, r))
                .collect()
        })
        .unwrap_or_default();

    snapshots::DocumentSnapshot {
        handle: document_handle(doc.id()),
        path: doc.path().map(|p| p.to_string_lossy().into_owned()),
        language: doc.language_name().map(|s| s.to_string()),
        is_modified: doc.is_modified(),
        line_count: text.len_lines(),
        selections,
        mode: mode_to_contract(mode),
    }
}

/// Build a `ViewSnapshot` from a `View` and its associated `Document`.
pub fn view_snapshot(
    view: &helix_view::View,
    doc: &helix_view::Document,
) -> snapshots::ViewSnapshot {
    let text = doc.text();
    let cursor_char = doc.selection(view.id).primary().cursor(text.slice(..));
    let cursor = char_to_position(text, cursor_char);
    let vp = doc.view_offset(view.id);

    snapshots::ViewSnapshot {
        handle: view_handle(view.id),
        document: document_handle(doc.id()),
        cursor,
        viewport: snapshots::ViewportInfo {
            first_visible_line: text.char_to_line(vp.anchor.min(text.len_chars())),
            height: view.area.height as usize,
            width: view.area.width as usize,
        },
    }
}

/// Build a `WorkspaceSnapshot` from the `Editor`.
pub fn workspace_snapshot(editor: &Editor) -> snapshots::WorkspaceSnapshot {
    let focused_view_id = editor.tree.focus;
    let focused_view = editor.tree.try_get(focused_view_id);

    snapshots::WorkspaceSnapshot {
        focused_document: focused_view.map(|v| document_handle(v.doc)),
        focused_view: focused_view.map(|_| view_handle(focused_view_id)),
        documents: editor
            .documents
            .keys()
            .map(|id| document_handle(*id))
            .collect(),
        views: editor
            .tree
            .views()
            .map(|(view, _)| view_handle(view.id))
            .collect(),
        mode: mode_to_contract(editor.mode),
    }
}

/// Build a `ThemeSnapshot` from the editor's current theme.
pub fn theme_snapshot(editor: &Editor) -> snapshots::ThemeSnapshot {
    let theme = &editor.theme;
    let name = theme.name().to_string();

    // Extract colors from well-known theme scopes.
    let bg = style_fg_color(theme.try_get("ui.background"));
    let fg = style_fg_color(theme.try_get("ui.text"));
    let selection = style_bg_color(theme.try_get("ui.selection"));
    let cursor = style_bg_color(theme.try_get("ui.cursor"));

    snapshots::ThemeSnapshot {
        name,
        bg,
        fg,
        selection,
        cursor,
    }
}

/// Build a `DiagnosticSnapshot` from a document's diagnostics.
pub fn diagnostic_snapshot(doc: &helix_view::Document) -> snapshots::DiagnosticSnapshot {
    let text = doc.text();
    let diagnostics = doc
        .diagnostics()
        .iter()
        .map(|d| {
            let start_char = d.range.start.min(text.len_chars());
            let end_char = d.range.end.min(text.len_chars());
            snapshots::Diagnostic {
                start: char_to_position(text, start_char),
                end: char_to_position(text, end_char),
                message: d.message.clone(),
                severity: severity_to_contract(d.severity),
            }
        })
        .collect();

    snapshots::DiagnosticSnapshot {
        document: document_handle(doc.id()),
        diagnostics,
    }
}

// ---------------------------------------------------------------------------
// Split tree snapshot
// ---------------------------------------------------------------------------

/// Build a recursive `SplitTreeSnapshot` from the editor's split tree.
pub fn split_tree_snapshot(editor: &Editor) -> snapshots::SplitTreeSnapshot {
    use helix_view::tree::{Layout, TopologyNode};

    let root = editor
        .tree
        .visit_topology(
            &|node: TopologyNode<snapshots::SplitNodeSnapshot>| match node {
                TopologyNode::Leaf(view_id) => snapshots::SplitNodeSnapshot::Leaf {
                    view: view_handle(view_id),
                },
                TopologyNode::Container { layout, children } => {
                    let direction = match layout {
                        Layout::Vertical => snapshots::SplitLayoutDirection::Horizontal,
                        Layout::Horizontal => snapshots::SplitLayoutDirection::Vertical,
                    };
                    snapshots::SplitNodeSnapshot::Container {
                        direction,
                        children,
                    }
                }
            },
        )
        .unwrap_or(snapshots::SplitNodeSnapshot::Container {
            direction: snapshots::SplitLayoutDirection::Horizontal,
            children: Vec::new(),
        });

    snapshots::SplitTreeSnapshot { root }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a char index in a Rope to a `Position` (0-based line + column).
pub fn char_to_position(text: &helix_core::Rope, char_idx: usize) -> snapshots::Position {
    let char_idx = char_idx.min(text.len_chars());
    let line = text.char_to_line(char_idx);
    let line_start = text.line_to_char(line);
    snapshots::Position {
        line,
        column: char_idx - line_start,
    }
}

/// Convert a `Position` (0-based line + column) to a char index in a Rope.
/// Clamps out-of-range positions to the document end.
pub fn position_to_char(text: &helix_core::Rope, pos: snapshots::Position) -> usize {
    let line = pos.line.min(text.len_lines().saturating_sub(1));
    let line_start = text.line_to_char(line);
    let line_end = text.line_to_char((line + 1).min(text.len_lines()));
    let line_len = line_end.saturating_sub(line_start);
    line_start + pos.column.min(line_len)
}

/// Format an RGB color as a `#rrggbb` hex string.
pub fn color_to_hex(color: snapshots::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

/// Convert a helix-core selection `Range` (char indices) to a contract
/// `SelectionRange` (line:col positions).
fn char_range_to_selection(
    text: &helix_core::Rope,
    range: &helix_core::selection::Range,
) -> snapshots::SelectionRange {
    snapshots::SelectionRange {
        anchor: char_to_position(text, range.anchor),
        head: char_to_position(text, range.head),
    }
}

/// Convert the internal `Mode` enum to the contract `EditMode`.
pub fn mode_to_contract(mode: helix_view::document::Mode) -> snapshots::EditMode {
    match mode {
        helix_view::document::Mode::Normal => snapshots::EditMode::Normal,
        helix_view::document::Mode::Insert => snapshots::EditMode::Insert,
        helix_view::document::Mode::Select => snapshots::EditMode::Select,
    }
}

/// Convert a mode string to contract `EditMode`.
pub fn mode_str_to_contract(s: &str) -> snapshots::EditMode {
    match s {
        "insert" => snapshots::EditMode::Insert,
        "select" => snapshots::EditMode::Select,
        _ => snapshots::EditMode::Normal,
    }
}

/// Convert helix-core `Severity` to contract `DiagnosticSeverity`.
fn severity_to_contract(sev: Option<Severity>) -> snapshots::DiagnosticSeverity {
    match sev {
        Some(Severity::Error) => snapshots::DiagnosticSeverity::Error,
        Some(Severity::Warning) => snapshots::DiagnosticSeverity::Warning,
        Some(Severity::Info) => snapshots::DiagnosticSeverity::Info,
        Some(Severity::Hint) | None => snapshots::DiagnosticSeverity::Hint,
    }
}

/// Extract the foreground color from an optional `Style`.
fn style_fg_color(style: Option<helix_view::graphics::Style>) -> Option<snapshots::Color> {
    style.and_then(|s| s.fg).and_then(color_to_contract)
}

/// Extract the background color from an optional `Style`.
fn style_bg_color(style: Option<helix_view::graphics::Style>) -> Option<snapshots::Color> {
    style.and_then(|s| s.bg).and_then(color_to_contract)
}

/// Convert a `helix_view::graphics::Color` to a contract `Color`.
///
/// Only RGB colors are representable in the contract; indexed and default
/// colors map to `None`.
fn color_to_contract(color: helix_view::graphics::Color) -> Option<snapshots::Color> {
    match color {
        helix_view::graphics::Color::Rgb(r, g, b) => Some(snapshots::Color { r, g, b }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Assistant adapters
// ---------------------------------------------------------------------------

/// Convert an assistant `thread::Id` to a contract `ThreadHandle`.
pub fn thread_handle(id: helix_view::assistant::thread::Id) -> ThreadHandle {
    ThreadHandle::from_raw(id.value())
}

/// Try to resolve a contract thread handle back to an internal `thread::Id`.
pub fn resolve_thread(handle: ThreadHandle) -> helix_view::assistant::thread::Id {
    helix_view::assistant::thread::Id::new(handle.raw())
}

/// Snapshot the assistant system from the editor.
pub fn assistant_snapshot(editor: &Editor) -> snapshots::AssistantSnapshot {
    let active = editor.assistant.active().map(thread_handle);
    let threads: Vec<snapshots::AssistantThreadSnapshot> = editor
        .assistant
        .threads()
        .map(|t| assistant_thread_snapshot(t, active.is_some_and(|a| a == thread_handle(t.id))))
        .collect();
    snapshots::AssistantSnapshot {
        active_thread: active,
        threads,
        is_ready: !editor.assistant.is_empty(),
    }
}

/// Snapshot a single assistant thread.
pub fn assistant_thread_snapshot(
    thread: &helix_view::assistant::thread::Thread,
    is_active: bool,
) -> snapshots::AssistantThreadSnapshot {
    snapshots::AssistantThreadSnapshot {
        handle: thread_handle(thread.id),
        title: thread.title().map(|s| s.to_string()),
        run: run_to_contract(thread.run()),
        entry_count: thread.entries().len(),
        has_context: !thread.context_items().is_empty(),
        is_active,
        scope_cwd: thread.scope().cwd.display().to_string(),
        follow: follow_to_contract(&thread.follow),
    }
}

/// Convert a `thread::Run` to a contract `AssistantRunState`.
#[allow(unreachable_patterns)]
pub fn run_to_contract(run: &helix_view::assistant::thread::Run) -> snapshots::AssistantRunState {
    match run {
        helix_view::assistant::thread::Run::Idle => snapshots::AssistantRunState::Idle,
        helix_view::assistant::thread::Run::Running => snapshots::AssistantRunState::Running,
        helix_view::assistant::thread::Run::Waiting => snapshots::AssistantRunState::Waiting,
        helix_view::assistant::thread::Run::Failed { .. } => snapshots::AssistantRunState::Failed,
        _ => snapshots::AssistantRunState::Running,
    }
}

/// Convert a `FollowState` to a contract `AssistantFollowState`.
pub fn follow_to_contract(
    follow: &helix_view::collab::FollowState,
) -> snapshots::AssistantFollowState {
    match follow {
        helix_view::collab::FollowState::Off => snapshots::AssistantFollowState::Off,
        helix_view::collab::FollowState::On { .. } => snapshots::AssistantFollowState::On,
        helix_view::collab::FollowState::Paused { .. } => snapshots::AssistantFollowState::Paused,
    }
}

/// Convert a thread's entries to contract snapshots.
pub fn assistant_entries_snapshot(
    thread: &helix_view::assistant::thread::Thread,
) -> Vec<snapshots::AssistantEntrySnapshot> {
    thread
        .entries()
        .iter()
        .map(|entry| {
            let (kind, text) = entry_kind_to_contract(&entry.kind);
            snapshots::AssistantEntrySnapshot {
                id: entry.id.value().get(),
                kind,
                text,
                location_count: entry.locations.len(),
            }
        })
        .collect()
}

/// Convert a thread's context items to contract snapshots.
pub fn assistant_context_snapshot(
    thread: &helix_view::assistant::thread::Thread,
) -> Vec<snapshots::AssistantContextSnapshot> {
    thread
        .context_items()
        .iter()
        .map(|item| {
            let (kind, label) = context_kind_to_contract(&item.kind);
            snapshots::AssistantContextSnapshot {
                id: item.id.as_str().to_string(),
                kind,
                label,
            }
        })
        .collect()
}

/// Convert an entry kind to a (kind_string, optional_text) pair.
pub fn entry_kind_to_contract(
    kind: &helix_view::assistant::thread::EntryKind,
) -> (String, Option<String>) {
    match kind {
        helix_view::assistant::thread::EntryKind::UserPrompt { text } => {
            ("user_prompt".into(), Some(text.clone()))
        }
        helix_view::assistant::thread::EntryKind::AssistantText { text } => {
            ("assistant_text".into(), Some(text.clone()))
        }
        helix_view::assistant::thread::EntryKind::ToolCall(call) => {
            ("tool_call".into(), Some(call.name.clone()))
        }
        helix_view::assistant::thread::EntryKind::Status { text } => {
            ("status".into(), Some(text.clone()))
        }
        helix_view::assistant::thread::EntryKind::ChangeSummary(summary) => (
            "change_summary".into(),
            Some(format!("{} files", summary.files.len())),
        ),
    }
}

/// Convert a context kind to a (kind_string, label_string) pair.
fn context_kind_to_contract(kind: &helix_view::assistant::context::Kind) -> (String, String) {
    match kind {
        helix_view::assistant::context::Kind::Selection(sel) => (
            "selection".into(),
            sel.label
                .clone()
                .unwrap_or_else(|| sel.path.display().to_string()),
        ),
        helix_view::assistant::context::Kind::Symbol(sym) => ("symbol".into(), sym.name.clone()),
        helix_view::assistant::context::Kind::File(file) => {
            ("file".into(), file.path.display().to_string())
        }
        helix_view::assistant::context::Kind::Diagnostics(diag) => (
            "diagnostics".into(),
            format!("diagnostics: {}", diag.path.display()),
        ),
        helix_view::assistant::context::Kind::Diff(diff) => {
            ("diff".into(), format!("diff: {}", diff.path.display()))
        }
    }
}
