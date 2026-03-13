//! Component model structs — frontend-agnostic state for picker, prompt, completion.

use std::borrow::Cow;
use std::path::PathBuf;

/// Picker render state — a snapshot of the visible window.
///
/// This is a **render-ready, theme-independent** snapshot. The Picker<T,D> controller
/// in helix-term writes this after each tick. Frontends read it and apply their own
/// theme styles. Highlight indices tell the renderer which graphemes to emphasize
/// (fuzzy match positions); the renderer maps those to the current theme.
///
/// **Why text + highlights, not styled spans?**
/// Styled spans would bake in the current theme. If the theme changes, every snapshot
/// is stale. Storing raw text + highlight positions lets any frontend (terminal, GUI,
/// headless test) apply its own styling without re-computing the match.
#[derive(Debug, Clone, Default)]
pub struct PickerModel {
    /// Current query string.
    pub query: String,
    /// Cursor position within `visible_items` (0-based).
    pub cursor: usize,
    /// Total number of matched items (for count display, e.g. "5/120").
    pub total_matched: usize,
    /// Total number of items including unmatched (for count display).
    pub total_items: usize,
    /// Whether the matcher or injector is still processing items.
    pub is_running: bool,
    /// Column headers. Empty for single-column pickers.
    pub headers: Box<[PickerColumnHeader]>,
    /// Which column header is "active" (cursor is in that query field). `None` for primary.
    pub active_column: Option<usize>,
    /// The visible window of items — already paginated by the controller.
    pub visible_items: Box<[PickerRow]>,
    /// Preview for the currently selected item.
    pub preview: Option<PickerPreview>,
    /// Whether preview pane is enabled.
    pub show_preview: bool,
}

/// Column header for multi-column pickers.
#[derive(Debug, Clone)]
pub struct PickerColumnHeader {
    pub name: Box<str>,
}

/// A single row in the picker's visible window.
#[derive(Debug, Clone)]
pub struct PickerRow {
    /// One cell per visible column.
    pub cells: Box<[PickerCell]>,
    /// Metadata for action dispatch when this row is selected.
    pub data: PickerItemData,
}

/// A single cell in a picker row (one column's content).
#[derive(Debug, Clone)]
pub struct PickerCell {
    /// Plain text content (what the column format function produced).
    pub text: String,
    /// Grapheme indices that should receive fuzzy-match highlighting.
    /// Sorted, deduplicated. The renderer maps these to the theme's highlight style.
    pub highlight_indices: Box<[u32]>,
}

/// Type-specific metadata for picker items. Adding a new picker variant = one variant here.
#[derive(Debug, Clone)]
pub enum PickerItemData {
    /// File picker (file browser, recent files, etc.)
    FilePath { path: PathBuf, is_dir: bool },
    /// Symbol picker (workspace symbols, document symbols)
    Symbol {
        name: String,
        kind: String,
        path: PathBuf,
        line: usize,
    },
    /// Buffer picker (open documents)
    Buffer { doc_id: usize },
    /// Theme picker, command picker, or any picker with no structured metadata.
    Plain,
}

/// What to show in the picker's preview pane.
#[derive(Debug, Clone)]
pub enum PickerPreview {
    /// Preview a file at a path (optionally jumping to a line).
    FilePath { path: PathBuf, line: Option<usize> },
    /// Inline text content (e.g., theme preview, command doc).
    Content(String),
}

/// Prompt render state — snapshot of the prompt input line + completions.
///
/// The Prompt controller in helix-term writes this after each tick. Frontends read it
/// and apply their own theme. Completions are stored as plain text; the renderer
/// decides how to display them (grid, vertical list, etc.).
#[derive(Debug, Clone, Default)]
pub struct PromptModel {
    /// The prompt label (e.g. ":", "/", "Search:", "Cmdline").
    pub prompt_text: Cow<'static, str>,
    /// Current input text.
    pub input: String,
    /// Byte-offset cursor position within `input`.
    pub cursor: usize,
    /// Available completions for the current input.
    pub completions: Vec<Box<str>>,
    /// Which completion is currently highlighted (index into `completions`).
    pub selected_completion: Option<usize>,
    /// Documentation text for the current input (e.g. command help).
    pub doc: Option<String>,
}

/// Completion popup state.
#[derive(Debug, Clone, Default)]
pub struct CompletionModel {
    pub items: Vec<CompletionItem>,
    pub selection: usize,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub kind: Option<String>,
}

// ─── ACP (Agent Client Protocol) panel ──────────────────────────────────────

/// ACP panel render state — the full chat + agent state snapshot.
///
/// The AcpPanel controller in helix-term writes this after each tick.
/// Frontends read it to render a chat panel with agent interactions,
/// tool calls, plan progress, and input.
#[derive(Debug, Clone, Default)]
pub struct AcpModel {
    /// Chat history entries in display order.
    pub entries: Vec<AcpChatEntry>,
    /// Vertical scroll offset (0 = showing newest at bottom).
    pub scroll: u16,
    /// Maximum scroll value (total content height - visible height).
    pub max_scroll: u16,
    /// Which chat entry is selected for copy/navigation (Normal mode).
    pub selected_entry: Option<usize>,

    /// Agent display name (e.g. "Claude Code", "No agent").
    pub agent_name: String,
    /// Agent version string.
    pub agent_version: String,
    /// Whether the agent is currently processing a request.
    pub agent_busy: bool,

    /// Whether the panel has keyboard focus.
    pub focused: bool,
    /// Whether the panel is in insert mode (typing) vs normal mode.
    pub insert_mode: bool,
    /// Error message to display (marquee-scrolled if too long).
    pub error: Option<String>,

    /// Current input text in the prompt.
    pub input: String,
    /// Cursor byte-offset within `input`.
    pub input_cursor: usize,

    /// Optional plan/task list displayed above the input area.
    pub plan_items: Option<Vec<AcpPlanItem>>,

    /// Number of queued messages waiting to send after current turn.
    pub queued_messages: usize,
}

/// A chat entry in the ACP panel.
#[derive(Debug, Clone)]
pub enum AcpChatEntry {
    /// User-sent message.
    UserMessage(String),
    /// Agent response text (may contain markdown).
    AgentText(String),
    /// Tool call with status tracking.
    ToolCall {
        id: String,
        name: String,
        path: Option<String>,
        status: String,
    },
    /// Status separator (e.g. "Session started", "Connected").
    Status(String),
}

/// A plan/task item displayed in the ACP panel.
#[derive(Debug, Clone)]
pub struct AcpPlanItem {
    pub content: String,
    pub status: AcpPlanStatus,
}

/// Status of a plan item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpPlanStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}
