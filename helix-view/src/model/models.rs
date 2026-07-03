//! Component model structs — frontend-agnostic state for picker, prompt, completion.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::assistant::{context, thread};

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

// ─── Assistant panel ─────────────────────────────────────────────────────────

/// Assistant panel render state — the full chat + agent state snapshot.
///
/// The editor/view layer owns and publishes this derived snapshot.
/// Frontends read it to render a chat panel with agent interactions,
/// tool calls, plan progress, and input without reconstructing domain state.
#[derive(Debug, Clone, Default)]
pub struct AssistantModel {
    /// Open assistant session tabs.
    pub tabs: Vec<AssistantTab>,
    /// Current assistant history entries for the active scope.
    pub history: Vec<AssistantHistoryEntry>,
    /// Active thread id.
    pub active_thread: Option<thread::Id>,
    /// Chat history entries in display order.
    pub entries: Vec<AssistantEntry>,
    /// Panel viewport scroll offset (0 = showing newest at bottom).
    pub viewport_scroll: u16,
    /// Maximum panel viewport scroll value (total content height - visible height).
    pub viewport_max_scroll: u16,
    /// Which chat entry is selected for copy/navigation (Normal mode).
    pub selected_entry: Option<thread::EntryId>,
    /// Active thread focus target.
    pub focus: Option<crate::assistant::thread::Focus>,
    /// Folded chat entries for the active thread.
    pub folded_entries: Vec<thread::EntryId>,
    /// Opened document mappings for active thread entries.
    pub opened_docs: HashMap<thread::EntryId, crate::DocumentId>,
    /// Durable active thread content scroll position.
    pub content_scroll: usize,

    /// Active assistant mode label.
    pub mode_name: Option<String>,
    /// Active assistant model label.
    pub model_label: Option<String>,
    /// Active assistant follow status.
    pub follow: Option<AssistantFollow>,
    /// Active assistant write/review mode.
    pub review_mode: crate::assistant::review::Mode,

    /// Agent display name (e.g. "Claude Code", "No agent").
    pub agent_name: String,
    /// Agent version string.
    pub agent_version: String,
    /// Whether the agent is currently processing a request.
    pub agent_busy: bool,
    /// Current active run status label, if any.
    pub agent_status: Option<String>,

    /// Whether the panel has keyboard focus.
    pub focused: bool,
    /// Whether the panel is in insert mode (typing) vs normal mode.
    pub insert_mode: bool,
    /// Error message to display (marquee-scrolled if too long).
    pub error: Option<String>,

    /// Current input text in the prompt.
    pub input: String,
    /// Context items attached to the active thread.
    pub context_items: Vec<AssistantContextItem>,
    /// Cursor byte-offset within `input`.
    pub input_cursor: usize,

    /// Optional plan/task list displayed above the input area.
    pub plan_items: Option<Vec<AssistantPlanItem>>,

    /// Number of queued messages waiting to send after current turn.
    pub queued_messages: usize,

    /// Terminals associated with the active thread.
    pub terminals: Vec<AssistantTerminal>,
    /// Authentication state for the active agent.
    pub auth: crate::assistant::auth::State,
    /// Active thread usage counters/context window data.
    pub usage: crate::assistant::thread::Usage,
    /// Agent commands available for the active thread.
    pub commands: Vec<crate::assistant::thread::Command>,
    /// Pending or recently completed elicitations for the active thread.
    pub pending_elicitations: Vec<crate::assistant::thread::Elicitation>,
    /// ACP capabilities advertised by the active agent.
    pub caps: Option<helix_acp::AgentCaps>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantHeaderTone {
    Default,
    Active,
    Warning,
}

#[derive(Debug, Clone)]
pub struct AssistantHeaderItem {
    pub label: String,
    pub tone: AssistantHeaderTone,
}

#[derive(Debug, Clone, Default)]
pub struct AssistantHeaderModel {
    pub leading: Vec<AssistantHeaderItem>,
    pub trailing: Vec<AssistantHeaderItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantPlanTone {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct AssistantPlanRow {
    pub icon: &'static str,
    pub tone: AssistantPlanTone,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct AssistantPlanSection {
    pub title: &'static str,
    pub done: usize,
    pub total: usize,
    pub rows: Vec<AssistantPlanRow>,
}

impl AssistantModel {
    #[must_use]
    pub fn focus(&self) -> thread::Focus {
        self.focus.unwrap_or_default()
    }

    #[must_use]
    pub fn selected_entry_id(&self) -> Option<thread::EntryId> {
        self.selected_entry
    }

    #[must_use]
    pub fn content_scroll(&self) -> usize {
        self.content_scroll
    }

    #[must_use]
    pub fn is_folded(&self, entry: thread::EntryId) -> bool {
        self.folded_entries.contains(&entry)
    }

    #[must_use]
    pub fn opened_doc(&self, entry: thread::EntryId) -> Option<crate::DocumentId> {
        self.opened_docs.get(&entry).copied()
    }

    #[must_use]
    pub fn plan_items(&self) -> Option<&[AssistantPlanItem]> {
        self.plan_items.as_deref()
    }

    #[must_use]
    pub fn follow_label(&self) -> Option<&'static str> {
        self.follow.map(AssistantFollow::label)
    }

    #[must_use]
    pub fn status_items(&self) -> Vec<AssistantStatusItem> {
        let mut items = Vec::new();
        if let Some(mode_name) = &self.mode_name {
            items.push(AssistantStatusItem {
                kind: AssistantStatusItemKind::Mode,
                label: mode_name.clone(),
            });
        }
        if let Some(model_label) = &self.model_label {
            items.push(AssistantStatusItem {
                kind: AssistantStatusItemKind::Model,
                label: model_label.clone(),
            });
        }
        if let Some(follow_label) = self.follow_label() {
            items.push(AssistantStatusItem {
                kind: AssistantStatusItemKind::Follow,
                // Editorial format: dot-prefixed separator, lowercase, no
                // colon — reads as quiet metadata, not as a config dump.
                label: format!("· follow {follow_label}"),
            });
        }
        items.push(AssistantStatusItem {
            kind: AssistantStatusItemKind::Review,
            label: format!("· {}", self.review_mode.label()),
        });
        items
    }

    #[must_use]
    pub fn has_running_activity(&self) -> bool {
        self.entries.iter().any(|entry| {
            matches!(
                &entry.kind,
                AssistantEntryKind::ToolCall { status, .. } if status == "running"
            )
        }) || self.plan_items().is_some_and(|items| {
            items
                .iter()
                .any(|item| item.status == AssistantPlanStatus::InProgress)
        })
    }

    #[must_use]
    pub fn context_line(&self) -> Option<String> {
        if self.context_items.is_empty() {
            return None;
        }

        Some(
            self.context_items
                .iter()
                .map(|item| format!("[{}]", item.label))
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    #[must_use]
    pub fn header(&self) -> AssistantHeaderModel {
        let mut leading = Vec::new();
        if self.tabs.is_empty() {
            leading.push(AssistantHeaderItem {
                label: self.agent_name.clone(),
                tone: AssistantHeaderTone::Default,
            });
        } else {
            for tab in self
                .tabs
                .iter()
                .filter(|tab| Some(tab.id) == self.active_thread)
                .take(1)
            {
                leading.push(AssistantHeaderItem {
                    label: tab.label(),
                    tone: AssistantHeaderTone::Active,
                });
            }
            if leading.is_empty() {
                leading.push(AssistantHeaderItem {
                    label: self.agent_name.clone(),
                    tone: AssistantHeaderTone::Default,
                });
            }
        }

        let mut trailing = Vec::new();
        trailing.push(AssistantHeaderItem {
            label: match self.focus() {
                thread::Focus::Input => " INPUT ".to_string(),
                thread::Focus::Messages => " MESSAGES ".to_string(),
            },
            tone: AssistantHeaderTone::Active,
        });
        for item in self.status_items().into_iter().filter(|item| {
            matches!(
                item.kind,
                AssistantStatusItemKind::Mode
                    | AssistantStatusItemKind::Model
                    | AssistantStatusItemKind::Review
            )
        }) {
            trailing.push(AssistantHeaderItem {
                label: item.label,
                tone: AssistantHeaderTone::Default,
            });
        }
        if let Some(status) = &self.agent_status {
            trailing.push(AssistantHeaderItem {
                label: status.clone(),
                tone: AssistantHeaderTone::Warning,
            });
        } else if self.agent_busy {
            trailing.push(AssistantHeaderItem {
                label: "working".to_string(),
                tone: AssistantHeaderTone::Warning,
            });
        }
        let total_tokens = self
            .usage
            .total_input_tokens
            .saturating_add(self.usage.total_output_tokens);
        let last_tokens = self
            .usage
            .input_tokens
            .saturating_add(self.usage.output_tokens);
        if total_tokens > 0 || last_tokens > 0 {
            trailing.push(AssistantHeaderItem {
                label: format!(
                    "{} tok · last {}",
                    compact_count(total_tokens),
                    compact_count(last_tokens)
                ),
                tone: AssistantHeaderTone::Default,
            });
        }

        AssistantHeaderModel { leading, trailing }
    }

    #[must_use]
    pub fn plan_section(&self) -> Option<AssistantPlanSection> {
        let items = self.plan_items()?;
        if items.is_empty() {
            return None;
        }

        Some(AssistantPlanSection {
            title: "Plan",
            done: items
                .iter()
                .filter(|item| item.status == AssistantPlanStatus::Completed)
                .count(),
            total: items.len(),
            rows: items
                .iter()
                .map(|item| AssistantPlanRow {
                    icon: item.status.icon(),
                    tone: item.status.tone(),
                    content: item.content.clone(),
                })
                .collect(),
        })
    }
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

#[derive(Debug, Clone)]
pub struct AssistantContextItem {
    pub id: context::Id,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantStatusItemKind {
    Mode,
    Model,
    Follow,
    Review,
}

#[derive(Debug, Clone)]
pub struct AssistantStatusItem {
    pub kind: AssistantStatusItemKind,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct AssistantTab {
    pub id: thread::Id,
    pub title: String,
    pub run: thread::Run,
    pub unread: bool,
    pub follow: AssistantFollow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantFollow {
    Off,
    On,
    Paused,
}

impl AssistantFollow {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Paused => "paused",
        }
    }

    #[must_use]
    pub const fn tab_marker(self) -> &'static str {
        match self {
            Self::Off => "",
            Self::On => "@",
            Self::Paused => "|",
        }
    }
}

impl AssistantTab {
    #[must_use]
    pub fn label(&self) -> String {
        let run = match &self.run {
            thread::Run::Running | thread::Run::Waiting => "~",
            thread::Run::Failed { .. } => "!",
            thread::Run::Idle => "",
        };
        let unread = if self.unread { "*" } else { "" };
        let follow = self.follow.tab_marker();
        format!(" {run}{unread}{}{follow} ", self.title)
    }
}

#[derive(Debug, Clone)]
pub struct AssistantHistoryEntry {
    pub id: thread::Id,
    pub title: Option<String>,
    pub unread: bool,
    pub run: thread::Run,
}

#[derive(Debug, Clone)]
pub struct AssistantTerminal {
    pub id: String,
    pub title: Option<String>,
    pub state: String,
    pub output: String,
}

/// A chat entry in the assistant panel.
#[derive(Debug, Clone)]
pub struct AssistantEntry {
    pub id: thread::EntryId,
    pub locations: usize,
    pub kind: AssistantEntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantEntryRole {
    User,
    Agent,
    Tool,
    Status,
    Change,
}

#[derive(Debug, Clone)]
pub struct AssistantEntryDetailLine {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct AssistantEntryDetails {
    pub heading: String,
    pub body: Option<String>,
    pub lines: Vec<AssistantEntryDetailLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantEntryTone {
    Default,
    Inactive,
    Focus,
    Warning,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct AssistantEntryRow {
    pub leading: String,
    pub leading_tone: AssistantEntryTone,
    pub animate_leading: bool,
    pub body: String,
    pub body_tone: AssistantEntryTone,
    pub accessory: Option<String>,
    pub accessory_tone: AssistantEntryTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantBubbleSide {
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub struct AssistantBubbleMeta {
    pub heading: String,
    pub side: AssistantBubbleSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantTextFormat {
    Plain,
    Markdown,
}

#[derive(Debug, Clone)]
pub struct AssistantBubbleDisplay {
    pub meta: AssistantBubbleMeta,
    pub format: AssistantTextFormat,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum AssistantEntryDisplay {
    Bubble(AssistantBubbleDisplay),
    Plain(AssistantEntryRow),
}

#[derive(Debug, Clone)]
pub enum AssistantEntryKind {
    /// User-sent message.
    UserMessage(String),
    /// Agent response text (may contain markdown).
    AgentText(String),
    /// Agent thought text, folded by default.
    Thought(String),
    /// Tool call with status tracking.
    ToolCall {
        id: String,
        name: String,
        status: String,
        output: String,
        subagent: Option<String>,
    },
    /// Status separator (e.g. "Session started", "Connected").
    Status(String),
    ChangeSummary {
        files: usize,
    },
    ReviewSummary {
        mode: crate::assistant::review::Mode,
        files: Vec<(std::path::PathBuf, String, crate::assistant::review::Status)>,
    },
}

impl AssistantEntry {
    #[must_use]
    pub const fn is_foldable(&self) -> bool {
        matches!(
            self.kind,
            AssistantEntryKind::UserMessage(_)
                | AssistantEntryKind::AgentText(_)
                | AssistantEntryKind::Thought(_)
                | AssistantEntryKind::ReviewSummary { .. }
        )
    }

    #[must_use]
    pub const fn role(&self) -> AssistantEntryRole {
        match self.kind {
            AssistantEntryKind::UserMessage(_) => AssistantEntryRole::User,
            AssistantEntryKind::AgentText(_) | AssistantEntryKind::Thought(_) => {
                AssistantEntryRole::Agent
            }
            AssistantEntryKind::ToolCall { .. } => AssistantEntryRole::Tool,
            AssistantEntryKind::Status(_) => AssistantEntryRole::Status,
            AssistantEntryKind::ChangeSummary { .. } | AssistantEntryKind::ReviewSummary { .. } => {
                AssistantEntryRole::Change
            }
        }
    }

    #[must_use]
    pub fn plain_text(&self) -> String {
        match &self.kind {
            AssistantEntryKind::UserMessage(message)
            | AssistantEntryKind::AgentText(message)
            | AssistantEntryKind::Thought(message)
            | AssistantEntryKind::Status(message) => message.clone(),
            AssistantEntryKind::ToolCall {
                id,
                name,
                status,
                output,
                subagent,
            } => {
                let mut lines = vec![
                    format!("id: {id}"),
                    format!("name: {name}"),
                    format!("status: {status}"),
                ];
                if let Some(session) = subagent {
                    lines.push(format!("subagent: {session}"));
                }
                if self.locations > 0 {
                    lines.push(format!("locations: {}", self.locations));
                }
                if !output.is_empty() {
                    lines.push(String::new());
                    lines.push(output.clone());
                }
                lines.join(helix_core::NATIVE_LINE_ENDING.as_str())
            }
            AssistantEntryKind::ChangeSummary { files } => format!("{files} changed files"),
            AssistantEntryKind::ReviewSummary { files, .. } => files
                .iter()
                .map(|(path, diff, status)| {
                    format!("{} ({})\n{}", path.display(), status.label(), diff)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    #[must_use]
    pub fn details(&self, agent_name: &str) -> AssistantEntryDetails {
        match &self.kind {
            AssistantEntryKind::UserMessage(message) => AssistantEntryDetails {
                heading: "user".to_string(),
                body: Some(message.clone()),
                lines: Vec::new(),
            },
            AssistantEntryKind::AgentText(message) => AssistantEntryDetails {
                heading: agent_name.to_string(),
                body: Some(message.clone()),
                lines: Vec::new(),
            },
            AssistantEntryKind::Thought(message) => AssistantEntryDetails {
                heading: "thinking".to_string(),
                body: Some(message.clone()),
                lines: Vec::new(),
            },
            AssistantEntryKind::ToolCall {
                id,
                name,
                status,
                output,
                subagent,
            } => {
                let mut lines = vec![
                    AssistantEntryDetailLine {
                        label: "id".to_string(),
                        value: id.clone(),
                    },
                    AssistantEntryDetailLine {
                        label: "status".to_string(),
                        value: status.clone(),
                    },
                ];
                if let Some(session) = subagent {
                    lines.push(AssistantEntryDetailLine {
                        label: "subagent".to_string(),
                        value: session.clone(),
                    });
                }
                if self.locations > 0 {
                    lines.push(AssistantEntryDetailLine {
                        label: "locations".to_string(),
                        value: self.locations.to_string(),
                    });
                }
                AssistantEntryDetails {
                    heading: format!("tool {name}"),
                    body: (!output.is_empty()).then(|| output.clone()),
                    lines,
                }
            }
            AssistantEntryKind::Status(message) => AssistantEntryDetails {
                heading: "status".to_string(),
                body: Some(message.clone()),
                lines: Vec::new(),
            },
            AssistantEntryKind::ChangeSummary { files } => AssistantEntryDetails {
                heading: "changes".to_string(),
                body: Some(format!("{files} changed files")),
                lines: Vec::new(),
            },
            AssistantEntryKind::ReviewSummary { mode, files } => AssistantEntryDetails {
                heading: format!("{} review", mode.label()),
                body: Some(
                    files
                        .iter()
                        .map(|(path, diff, status)| {
                            format!(
                                "## {} ({})\n\n```diff\n{}```",
                                path.display(),
                                status.label(),
                                diff
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                ),
                lines: Vec::new(),
            },
        }
    }

    #[must_use]
    pub fn details_markdown(&self, agent_name: &str) -> String {
        let details = self.details(agent_name);
        let heading = match self.role() {
            AssistantEntryRole::User => "User Message",
            AssistantEntryRole::Agent => "Agent Message",
            AssistantEntryRole::Tool => "Tool Call",
            AssistantEntryRole::Status => "Status",
            AssistantEntryRole::Change => "Change Summary",
        };

        let mut text = format!("# {heading}\n\n");
        if let Some(body) = details.body {
            text.push_str(&body);
            text.push('\n');
        }
        for line in details.lines {
            text.push_str(&format!("- {}: {}\n", line.label, line.value));
        }
        text
    }

    #[must_use]
    pub fn plain_row(&self) -> Option<AssistantEntryRow> {
        match &self.kind {
            AssistantEntryKind::ToolCall {
                name,
                status,
                output,
                subagent,
                ..
            } => Some(AssistantEntryRow {
                leading: format!(" {} ", Self::status_icon(status)),
                leading_tone: Self::status_tone(status),
                animate_leading: status == "running",
                body: if output.is_empty() {
                    name.clone()
                } else {
                    format!("{name} - {}", Self::summary(output, 72))
                },
                body_tone: AssistantEntryTone::Focus,
                accessory: Some(if subagent.is_some() {
                    format!(" ↳ subagent  {status}")
                } else {
                    format!(" {status}")
                }),
                accessory_tone: Self::status_tone(status),
            }),
            AssistantEntryKind::Status(text) => Some(AssistantEntryRow {
                leading: String::new(),
                leading_tone: AssistantEntryTone::Inactive,
                animate_leading: false,
                body: format!(" {text}"),
                body_tone: AssistantEntryTone::Inactive,
                accessory: None,
                accessory_tone: AssistantEntryTone::Default,
            }),
            AssistantEntryKind::ChangeSummary { files } => Some(AssistantEntryRow {
                leading: String::new(),
                leading_tone: AssistantEntryTone::Inactive,
                animate_leading: false,
                body: format!(" changes: {files} files"),
                body_tone: AssistantEntryTone::Inactive,
                accessory: None,
                accessory_tone: AssistantEntryTone::Default,
            }),
            AssistantEntryKind::ReviewSummary { files, .. } => {
                let pending = files
                    .iter()
                    .filter(|(_, _, status)| status.is_pending())
                    .count();
                Some(AssistantEntryRow {
                    leading: String::new(),
                    leading_tone: AssistantEntryTone::Inactive,
                    animate_leading: false,
                    body: if files.len() == 1 {
                        format!(" review: {} ({})", files[0].0.display(), files[0].2.label())
                    } else {
                        format!(" review: {} files ({pending} pending)", files.len())
                    },
                    body_tone: if pending > 0 {
                        AssistantEntryTone::Warning
                    } else {
                        AssistantEntryTone::Inactive
                    },
                    accessory: Some(" a accept  x reject ".to_string()).filter(|_| pending > 0),
                    accessory_tone: AssistantEntryTone::Inactive,
                })
            }
            AssistantEntryKind::UserMessage(_)
            | AssistantEntryKind::AgentText(_)
            | AssistantEntryKind::Thought(_) => None,
        }
    }

    #[must_use]
    pub fn bubble_meta(&self, agent_name: &str) -> Option<AssistantBubbleMeta> {
        match self.role() {
            AssistantEntryRole::User => Some(AssistantBubbleMeta {
                heading: " you".to_string(),
                side: AssistantBubbleSide::Right,
            }),
            AssistantEntryRole::Agent => Some(AssistantBubbleMeta {
                heading: format!(" {agent_name}"),
                side: AssistantBubbleSide::Left,
            }),
            AssistantEntryRole::Tool | AssistantEntryRole::Status | AssistantEntryRole::Change => {
                None
            }
        }
    }

    #[must_use]
    pub fn display(&self, agent_name: &str) -> AssistantEntryDisplay {
        match &self.kind {
            AssistantEntryKind::UserMessage(text) => {
                AssistantEntryDisplay::Bubble(AssistantBubbleDisplay {
                    meta: self.bubble_meta(agent_name).expect("user bubble metadata"),
                    format: AssistantTextFormat::Plain,
                    text: text.clone(),
                })
            }
            AssistantEntryKind::AgentText(text) => {
                AssistantEntryDisplay::Bubble(AssistantBubbleDisplay {
                    meta: self.bubble_meta(agent_name).expect("agent bubble metadata"),
                    format: AssistantTextFormat::Markdown,
                    text: text.clone(),
                })
            }
            AssistantEntryKind::Thought(text) => AssistantEntryDisplay::Plain(AssistantEntryRow {
                leading: " … ".to_string(),
                leading_tone: AssistantEntryTone::Inactive,
                animate_leading: false,
                body: format!("thinking... {}", Self::summary(text, 96)),
                body_tone: AssistantEntryTone::Inactive,
                accessory: None,
                accessory_tone: AssistantEntryTone::Default,
            }),
            AssistantEntryKind::ToolCall { .. }
            | AssistantEntryKind::Status(_)
            | AssistantEntryKind::ChangeSummary { .. }
            | AssistantEntryKind::ReviewSummary { .. } => {
                AssistantEntryDisplay::Plain(self.plain_row().expect("plain row display"))
            }
        }
    }

    #[must_use]
    pub fn status_tone(status: &str) -> AssistantEntryTone {
        match status {
            "running" => AssistantEntryTone::Warning,
            "completed" | "done" => AssistantEntryTone::Success,
            "failed" => AssistantEntryTone::Error,
            "cancelled" => AssistantEntryTone::Inactive,
            _ => AssistantEntryTone::Inactive,
        }
    }

    #[must_use]
    pub fn status_icon(status: &str) -> &'static str {
        match status {
            "completed" | "done" => "●",
            "failed" => "✕",
            "cancelled" => "–",
            _ => "○",
        }
    }

    fn summary(text: &str, max: usize) -> String {
        let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if compact.chars().count() <= max {
            return compact;
        }
        compact
            .chars()
            .take(max.saturating_sub(1))
            .chain(std::iter::once('…'))
            .collect()
    }
}

/// A plan/task item displayed in the assistant panel.
#[derive(Debug, Clone)]
pub struct AssistantPlanItem {
    pub content: String,
    pub status: AssistantPlanStatus,
}

/// Status of a plan item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantPlanStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl AssistantPlanStatus {
    #[must_use]
    pub const fn tone(self) -> AssistantPlanTone {
        match self {
            Self::Pending => AssistantPlanTone::Pending,
            Self::InProgress => AssistantPlanTone::InProgress,
            Self::Completed => AssistantPlanTone::Completed,
            Self::Failed => AssistantPlanTone::Failed,
        }
    }

    #[must_use]
    pub const fn icon(self) -> &'static str {
        match self {
            Self::Pending => " ○ ",
            Self::InProgress => " ◎ ",
            Self::Completed => " ● ",
            Self::Failed => " ✕ ",
        }
    }
}
