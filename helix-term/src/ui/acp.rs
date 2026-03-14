use crate::component_traits;
use crate::compositor::{Component, Context, Event, EventResult, RenderContext};
use crate::ui::marquee::{schedule_redraw_at, Marquee};
use helix_core::Position;
use helix_view::content_region::ContentRegion;
use helix_view::document::Mode;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::input::{KeyCode, KeyEvent, KeyModifiers};
use helix_view::theme::Modifier;
use helix_view::theme::Style;
use helix_view::traits::{Bounded, Focusable, Identified, Modal, Scrollable};
use helix_view::Editor;
use tui::buffer::Buffer as Surface;
use tui::text::{Span, Spans};
use crate::widgets::{chat_bubble, BubbleAlign, BubbleStyle};

pub const ID: &str = "acp-panel";
pub const PERMISSION_ID: &str = "acp-permission";

// ---------------------------------------------------------------------------
// Chat entries
// ---------------------------------------------------------------------------

/// An entry in the ACP chat log.
#[derive(Clone)]
pub enum ChatEntry {
    /// User prompt text.
    UserMessage(String),
    /// Agent text chunk (accumulated streaming).
    AgentText(String),
    /// Tool call: name (e.g. read_file), optional path on newline, status only (no output).
    ToolCall {
        id: String,
        name: String,
        path: Option<String>,
        status: String,
    },
    /// A status/separator line.
    Status(String),
}

#[derive(Clone)]
pub struct PlanItem {
    pub content: String,
    pub status: PlanStatus,
}

#[derive(Clone, Copy, PartialEq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

// ---------------------------------------------------------------------------
// Chat block types for rendering
// ---------------------------------------------------------------------------

/// A renderable block in the chat output area.
struct ChatBlock<'a> {
    kind: ChatBlockKind<'a>,
}

enum ChatBlockKind<'a> {
    /// A bordered bubble with optional label, styled content, and background fill.
    Bubble {
        label: Option<(String, Style)>,
        lines: Vec<Spans<'a>>,
        bubble_width: u16,
        align: BubbleAlign,
        style: BubbleStyle,
    },
    /// Plain styled lines (tool calls, status messages).
    Plain(Vec<Spans<'a>>),
}

impl<'a> ChatBlock<'a> {
    /// Total rows this block occupies (excluding trailing spacing).
    fn height(&self) -> u16 {
        match &self.kind {
            ChatBlockKind::Bubble { label, lines, .. } => {
                let label_h = if label.is_some() { 1 } else { 0 };
                let bubble_h = lines.len() as u16 + 2; // top + content + bottom border
                label_h + bubble_h
            }
            ChatBlockKind::Plain(lines) => lines.len() as u16,
        }
    }
}

// ---------------------------------------------------------------------------
// Session history entry
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SessionRecord {
    pub session_id: String,
    pub agent_name: String,
    pub started: std::time::Instant,
    pub message_count: usize,
}

// ---------------------------------------------------------------------------
// ACP Panel
// ---------------------------------------------------------------------------

pub struct AcpPanel {
    focused: bool,
    /// Read-only chat/output area with component-owned scroll + viewport state.
    output: ContentRegion<Vec<ChatEntry>>,
    /// Editable input area backed by a component-owned document.
    input: helix_view::edit_region::EditRegion,
    agent_name: String,
    agent_version: String,
    agent_busy: bool,
    /// Last ACP/agent error message, shown below the status line (keeps agent context in panel).
    panel_error: Option<String>,
    /// Marquee for long error text (scroll, hold, reset, repeat; pauses after inactivity).
    error_marquee: Marquee,
    /// Queued messages to send after the current turn completes.
    message_queue: Vec<String>,
    /// Config options reported by the agent (model, thinking, etc.).
    config_options: Vec<helix_acp::types::ConfigOption>,
    /// Available session modes.
    session_modes: Vec<helix_acp::types::SessionMode>,
    /// Currently active mode id.
    current_mode_id: Option<String>,
    /// Plan/tasks shown above input (static UI, not in chat history).
    plan_items: Option<Vec<PlanItem>>,
    /// Selected chat entry index (for j/k navigation and copy). None = no selection.
    selected_entry: Option<usize>,
    /// Last input cursor screen position (set during render, read by cursor()).
    last_input_cursor: Option<(u16, u16)>,
    /// Model panel ID, set on first sync.
    model_panel_id: Option<helix_view::model::PanelId>,
}

impl Default for AcpPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpPanel {
    pub fn new() -> Self {
        Self {
            focused: true,
            output: ContentRegion::default(),
            input: helix_view::edit_region::EditRegion::default(),
            agent_name: String::from("No agent"),
            agent_version: String::new(),
            agent_busy: false,
            panel_error: None,
            error_marquee: Marquee::new(),
            message_queue: Vec::new(),
            config_options: Vec::new(),
            session_modes: Vec::new(),
            current_mode_id: None,
            plan_items: None,
            selected_entry: None,
            last_input_cursor: None,
            model_panel_id: None,
        }
    }

    pub fn model_panel_id(&self) -> Option<helix_view::model::PanelId> {
        self.model_panel_id
    }

    /// Set or clear the panel error message (shown below status line; keeps ACP context in panel).
    pub fn set_panel_error(&mut self, msg: Option<String>) {
        self.panel_error = msg.clone();
        self.error_marquee
            .set_text(msg.map(|s| format!("ACP: {}", s)));
    }

    pub fn activate_input(&mut self) {
        self.set_focused(true);
    }

    pub fn set_agent_name(&mut self, name: String) {
        self.agent_name = name;
    }

    pub fn set_agent_version(&mut self, version: String) {
        self.agent_version = version;
    }

    pub fn set_busy(&mut self, busy: bool) {
        self.agent_busy = busy;
    }

    pub fn is_busy(&self) -> bool {
        self.agent_busy
    }

    pub fn set_config_options(&mut self, opts: Vec<helix_acp::types::ConfigOption>) {
        self.config_options = opts;
    }

    pub fn set_session_modes(&mut self, modes: Vec<helix_acp::types::SessionMode>) {
        self.session_modes = modes;
    }

    pub fn set_current_mode_id(&mut self, mode_id: String) {
        self.current_mode_id = Some(mode_id);
    }

    /// Returns the display name of the current model (from config_options with category "model").
    pub fn current_model_name(&self) -> Option<&str> {
        for opt in &self.config_options {
            if opt.category.as_deref() == Some("model") {
                // Find the option value matching current_value
                for v in &opt.options {
                    if v.value == opt.current_value {
                        return Some(&v.name);
                    }
                }
                // Fallback to the raw current_value
                if !opt.current_value.is_empty() {
                    return Some(&opt.current_value);
                }
            }
        }
        None
    }

    /// Display string for status bar: show model display name (e.g. "Sonnet", "Default (recommended)").
    pub fn status_model_display(&self) -> Option<String> {
        self.status_config_name("model")
    }

    /// Display string for a config option by category (e.g. "model", "thinking").
    /// Prefers the option's value (id) over display name when available.
    pub fn status_config_display(&self, category: &str) -> Option<String> {
        for opt in &self.config_options {
            if opt.category.as_deref() == Some(category) {
                for v in &opt.options {
                    if v.value == opt.current_value || v.name == opt.current_value {
                        return Some(v.value.clone());
                    }
                }
                if !opt.current_value.is_empty() {
                    return Some(opt.current_value.clone());
                }
            }
        }
        None
    }

    /// Display name for a config option by category (e.g. "mode").
    /// Used when session_modes is empty but config_options has the category (e.g. claude-acp).
    pub fn status_config_name(&self, category: &str) -> Option<String> {
        for opt in &self.config_options {
            if opt.category.as_deref() == Some(category) {
                for v in &opt.options {
                    if v.value == opt.current_value || v.name == opt.current_value {
                        return Some(v.name.clone());
                    }
                }
                if !opt.current_value.is_empty() {
                    return Some(opt.current_value.clone());
                }
            }
        }
        None
    }

    /// Look up mode display name by id in config_options (for agents like claude-acp that use
    /// config_options category "mode" instead of session_modes).
    fn mode_name_for_id(&self, mode_id: &str) -> Option<String> {
        for opt in &self.config_options {
            if opt.category.as_deref() == Some("mode") {
                for v in &opt.options {
                    if v.value == mode_id {
                        return Some(v.name.clone());
                    }
                }
            }
        }
        None
    }

    /// Returns the display name of the current session mode.
    /// Uses session_modes when available (e.g. Codex), else config_options with category "mode"
    /// (e.g. claude-acp which sends mode in config_options, not session_modes).
    pub fn current_mode_name(&self) -> Option<String> {
        if let Some(mode_id) = &self.current_mode_id {
            if let Some(mode) = self.session_modes.iter().find(|m| m.id == *mode_id) {
                return Some(mode.name.clone());
            }
            if let Some(name) = self.mode_name_for_id(mode_id) {
                return Some(name);
            }
        }
        self.status_config_name("mode")
    }

    pub fn config_options(&self) -> &[helix_acp::types::ConfigOption] {
        &self.config_options
    }

    /// Cycle to the next value for a config option by category.
    /// Returns (config_id, next_value_id, prev_value_id) or None if category not found or no options.
    pub fn cycle_config_option(&self, category: &str) -> Option<(String, String, String)> {
        for opt in &self.config_options {
            if opt.category.as_deref() != Some(category) || opt.options.is_empty() {
                continue;
            }
            let prev_value = opt.current_value.clone();
            let current_idx = opt
                .options
                .iter()
                .position(|v| v.value == opt.current_value)
                .unwrap_or(0);
            let next_idx = (current_idx + 1) % opt.options.len();
            let next_value = opt.options[next_idx].value.clone();
            return Some((opt.id.clone(), next_value, prev_value));
        }
        None
    }

    /// Optimistically update config_options so the UI reflects the new value immediately.
    /// The agent will send config_option_update to confirm; this avoids waiting for the round-trip.
    pub fn apply_config_option_cycle(&mut self, category: &str, next_value: String) {
        for opt in &mut self.config_options {
            if opt.category.as_deref() == Some(category) {
                opt.current_value = next_value.clone();
                if category == "mode" {
                    self.current_mode_id = Some(next_value);
                }
                break;
            }
        }
    }

    pub fn session_modes(&self) -> &[helix_acp::types::SessionMode] {
        &self.session_modes
    }

    pub fn push_entry(&mut self, entry: ChatEntry) {
        if matches!(entry, ChatEntry::UserMessage(_)) {
            self.plan_items = None;
        }
        self.output.content_mut().push(entry);
        self.output.scroll_to_end();
        self.selected_entry = None;
    }

    /// Append text to the last AgentText entry, or create one.
    pub fn append_agent_text(&mut self, text: &str) {
        if let Some(ChatEntry::AgentText(ref mut existing)) = self.output.content_mut().last_mut() {
            existing.push_str(text);
        } else {
            self.output
                .content_mut()
                .push(ChatEntry::AgentText(text.to_string()));
        }
        self.output.scroll_to_end();
    }

    /// Add or update a tool call by id. Format: name (e.g. read_file), path on newline, status only.
    /// When name is None, only updates existing (by id).
    pub fn update_tool_call(
        &mut self,
        id: &str,
        name: Option<&str>,
        path: Option<&str>,
        status: &str,
    ) {
        for entry in self.output.content_mut().iter_mut().rev() {
            if let ChatEntry::ToolCall {
                id: ref existing_id,
                status: ref mut existing_status,
                path: ref mut existing_path,
                ..
            } = entry
            {
                if existing_id == id {
                    *existing_status = status.to_string();
                    if let Some(p) = path {
                        *existing_path = Some(p.to_string());
                    }
                    return;
                }
            }
        }
        if let Some(n) = name {
            self.output.content_mut().push(ChatEntry::ToolCall {
                id: id.to_string(),
                name: n.to_string(),
                path: path.map(|s| s.to_string()),
                status: status.to_string(),
            });
            self.output.scroll_to_end();
        }
    }

    /// Set plan items (shown in dedicated area above input, not in chat).
    pub fn update_plan(&mut self, items: Vec<PlanItem>) {
        self.plan_items = Some(items);
    }

    /// Clear the plan area so it disappears from the UI.
    pub fn clear_plan(&mut self) {
        self.plan_items = None;
    }

    /// Enqueue a follow-up message to send after the current turn finishes.
    pub fn enqueue_message(&mut self, msg: String) {
        self.message_queue.push(msg);
    }

    /// Take the next queued message, if any.
    pub fn dequeue_message(&mut self) -> Option<String> {
        if self.message_queue.is_empty() {
            None
        } else {
            Some(self.message_queue.remove(0))
        }
    }

    pub fn queue_len(&self) -> usize {
        self.message_queue.len()
    }

    pub fn clear_queue(&mut self) {
        self.message_queue.clear();
    }

    /// Sync panel state to the shared `Model.panels` entry. Called during render
    /// so any frontend can read the ACP panel's current state.
    fn sync_to_model(&mut self, editor: &mut Editor) {
        use helix_view::model::{
            AcpChatEntry as UiEntry, AcpModel, AcpPlanItem as UiPlanItem,
            AcpPlanStatus as UiPlanStatus, PanelSide, PanelSize,
        };

        // Lazily insert a model panel on first sync, or reclaim an orphaned one.
        let panel_id = match self.model_panel_id {
            Some(id) if editor.model.panels.contains_key(id) => id,
            _ => {
                // Check for an orphaned AcpModel panel (e.g., from a replaced component).
                let existing = editor
                    .model
                    .panels
                    .iter()
                    .find(|(_, p)| p.content.as_any().is::<AcpModel>())
                    .map(|(id, _)| id);
                let id = existing.unwrap_or_else(|| {
                    editor.model.insert_panel(
                        "Agent",
                        Box::new(AcpModel::default()),
                        PanelSide::Right,
                        PanelSize::Percent(35),
                    )
                });
                self.model_panel_id = Some(id);
                id
            }
        };

        let input_text = self
            .input
            .document(editor)
            .map(|doc| doc.text().to_string())
            .unwrap_or_default();

        let Some(model) = editor.model.panel_model_mut::<AcpModel>(panel_id) else {
            return;
        };

        // Map chat entries.
        model.entries = self
            .output
            .content()
            .iter()
            .map(|e| match e {
                ChatEntry::UserMessage(s) => UiEntry::UserMessage(s.clone()),
                ChatEntry::AgentText(s) => UiEntry::AgentText(s.clone()),
                ChatEntry::ToolCall {
                    id,
                    name,
                    path,
                    status,
                } => UiEntry::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    path: path.clone(),
                    status: status.clone(),
                },
                ChatEntry::Status(s) => UiEntry::Status(s.clone()),
            })
            .collect();

        model.scroll = self
            .output
            .max_scroll()
            .saturating_sub(self.output.scroll()) as u16;
        model.max_scroll = self.output.max_scroll() as u16;
        model.selected_entry = self.selected_entry;
        model.agent_name.clone_from(&self.agent_name);
        model.agent_version.clone_from(&self.agent_version);
        model.agent_busy = self.agent_busy;
        model.focused = self.focused;
        model.insert_mode = self.focused;
        model.error = self.panel_error.clone();
        model.input = input_text;
        model.input_cursor = 0; // cursor position tracked by engine, not model
        model.queued_messages = self.message_queue.len();

        // Map plan items.
        model.plan_items = self.plan_items.as_ref().map(|items| {
            items
                .iter()
                .map(|p| UiPlanItem {
                    content: p.content.clone(),
                    status: match p.status {
                        PlanStatus::Pending => UiPlanStatus::Pending,
                        PlanStatus::InProgress => UiPlanStatus::InProgress,
                        PlanStatus::Completed => UiPlanStatus::Completed,
                        PlanStatus::Failed => UiPlanStatus::Failed,
                    },
                })
                .collect()
        });
    }

    /// Remove this panel's entry from the shared Model. Call before dropping.
    pub fn remove_model_panel(&self, editor: &mut Editor) {
        if let Some(id) = self.model_panel_id {
            editor.model.remove_panel(id);
        }
    }

    /// Build renderable blocks from chat entries.
    fn build_blocks<'a>(&self, theme: &helix_view::Theme, width: u16) -> Vec<ChatBlock<'a>> {
        let border_style = theme.get("ui.window");
        let agent_style = theme.get("ui.text.info");
        let user_label_style = theme.get("keyword").add_modifier(Modifier::BOLD);
        let agent_label_style = theme
            .try_get("ui.acp.agent.label")
            .unwrap_or_else(|| theme.get("ui.text.info").add_modifier(Modifier::BOLD));
        let user_text_style = theme.get("ui.text");
        let tool_icon_style = theme.get("ui.text.inactive");
        let tool_name_style = theme.get("ui.text.focus");
        let tool_detail_style = theme.get("ui.text.inactive");
        let separator_style = theme.get("ui.statusline.separator");
        let heading_style = theme.get("markup.heading.1");
        let code_style = theme.get("markup.raw.inline");
        let bold_style = agent_style.add_modifier(Modifier::BOLD);
        let italic_style = agent_style.add_modifier(Modifier::ITALIC);
        let status_dim_style = theme.get("ui.text.inactive");

        // Bubble background colors (distinguishable, theme-configurable).
        let user_bubble_bg = theme
            .try_get("ui.acp.user_bubble")
            .unwrap_or_else(|| Style::default());
        let agent_bubble_bg = theme
            .try_get("ui.acp.agent_bubble")
            .unwrap_or_else(|| Style::default());

        // Flex width: min 60%, max 90% of panel.
        let min_bubble = ((width as u32 * 60 / 100) as u16).max(20);
        let max_bubble = ((width as u32 * 90 / 100) as u16).min(width);

        let mut blocks = Vec::new();

        for entry in self.output.content().iter() {
            match entry {
                ChatEntry::UserMessage(text) => {
                    let bubble_w =
                        fit_bubble_width(text, min_bubble as usize, max_bubble as usize) as u16;
                    let inner_w = bubble_w.saturating_sub(4) as usize;
                    let wrapped = wrap_text(text, inner_w);
                    let content_lines: Vec<Spans> = wrapped
                        .iter()
                        .map(|wl| Spans::from(Span::styled(wl.clone(), user_text_style.patch(user_bubble_bg))))
                        .collect();
                    blocks.push(ChatBlock {
                        kind: ChatBlockKind::Bubble {
                            label: Some((" You".to_string(), user_label_style)),
                            lines: content_lines,
                            bubble_width: bubble_w,
                            align: BubbleAlign::Right,
                            style: BubbleStyle {
                                border: border_style.patch(user_bubble_bg),
                                background: user_bubble_bg,
                            },
                        },
                    });
                }
                ChatEntry::AgentText(text) => {
                    let bubble_w =
                        fit_bubble_width(text, min_bubble as usize, max_bubble as usize) as u16;
                    let inner_w = bubble_w.saturating_sub(4) as usize;

                    // Render markdown then re-wrap into bubble-sized lines.
                    let mut md_lines: Vec<Spans> = Vec::new();
                    render_markdown_lines(
                        text,
                        &mut md_lines,
                        agent_style.patch(agent_bubble_bg),
                        &MarkdownLineStyles {
                            heading: heading_style.patch(agent_bubble_bg),
                            code: code_style.patch(agent_bubble_bg),
                            bold: bold_style.patch(agent_bubble_bg),
                            italic: italic_style.patch(agent_bubble_bg),
                            separator: separator_style.patch(agent_bubble_bg),
                        },
                    );

                    // Re-wrap markdown lines to fit inside the bubble.
                    let mut content_lines: Vec<Spans> = Vec::new();
                    for md_line in md_lines {
                        let line_text: String =
                            md_line.0.iter().map(|s| s.content.as_ref()).collect();
                        if line_text.len() <= inner_w {
                            content_lines.push(md_line);
                        } else {
                            let wrapped = wrap_text(&line_text, inner_w);
                            let style = md_line
                                .0
                                .first()
                                .map(|s| s.style)
                                .unwrap_or(agent_style.patch(agent_bubble_bg));
                            for wl in &wrapped {
                                content_lines.push(Spans::from(Span::styled(wl.clone(), style)));
                            }
                        }
                    }

                    blocks.push(ChatBlock {
                        kind: ChatBlockKind::Bubble {
                            label: Some((
                                format!(" {}", self.agent_name),
                                agent_label_style,
                            )),
                            lines: content_lines,
                            bubble_width: bubble_w,
                            align: BubbleAlign::Left,
                            style: BubbleStyle {
                                border: border_style.patch(agent_bubble_bg),
                                background: agent_bubble_bg,
                            },
                        },
                    });
                }
                ChatEntry::ToolCall {
                    name, path, status, ..
                } => {
                    let icon = match status.as_str() {
                        "running" => "\u{25ce}",            // ◎
                        "completed" | "done" => "\u{25cf}", // ●
                        "failed" => "\u{2715}",             // ✕
                        "cancelled" => "\u{2013}",          // –
                        _ => "\u{25cb}",                    // ○
                    };
                    let mut lines = vec![Spans::from(vec![
                        Span::styled(format!(" {icon} "), tool_icon_style),
                        Span::styled(name.clone(), tool_name_style),
                    ])];
                    if let Some(ref p) = path {
                        lines.push(Spans::from(Span::styled(
                            format!("     {p}"),
                            tool_detail_style,
                        )));
                    }
                    blocks.push(ChatBlock {
                        kind: ChatBlockKind::Plain(lines),
                    });
                }
                ChatEntry::Status(text) => {
                    blocks.push(ChatBlock {
                        kind: ChatBlockKind::Plain(vec![
                            Spans::from(Span::styled(format!(" {text}"), status_dim_style)),
                        ]),
                    });
                }
            }
        }

        blocks
    }

    /// Render chat blocks directly to the surface with proper scroll.
    fn render_content(
        &mut self,
        blocks: &[ChatBlock],
        area: Rect,
        surface: &mut Surface,
    ) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        // Compute total height (each block's height + 1 spacing line after).
        let total_height: usize = blocks.iter().map(|b| b.height() as usize + 1).sum();
        self.output.set_content_height(total_height);

        let scroll = Scrollable::scroll(&self.output);
        let mut y_offset: isize = area.y as isize - scroll as isize;

        for block in blocks {
            let block_h = block.height();
            let block_total = block_h as usize + 1; // +1 for spacing

            // Skip blocks entirely above the viewport.
            if y_offset + block_total as isize <= area.y as isize {
                y_offset += block_total as isize;
                continue;
            }

            // Stop if we're past the bottom of the viewport.
            if y_offset >= (area.y + area.height) as isize {
                break;
            }

            match &block.kind {
                ChatBlockKind::Bubble {
                    label,
                    lines,
                    bubble_width,
                    align,
                    style,
                } => {
                    let mut cur_y = y_offset;

                    // Label line.
                    if let Some((text, label_style)) = label {
                        if cur_y >= area.y as isize && cur_y < (area.y + area.height) as isize {
                            let lx = match align {
                                BubbleAlign::Right => {
                                    area.x + area.width.saturating_sub(text.len() as u16)
                                }
                                BubbleAlign::Left => area.x,
                            };
                            surface.set_stringn(
                                lx,
                                cur_y as u16,
                                text,
                                area.width as usize,
                                *label_style,
                            );
                        }
                        cur_y += 1;
                    }

                    // Bubble (only render the visible portion).
                    if cur_y < (area.y + area.height) as isize
                        && cur_y + lines.len() as isize + 2 > area.y as isize
                    {
                        // Clip to visible area.
                        let bubble_y = cur_y.max(area.y as isize) as u16;
                        let bubble_bottom = ((cur_y + lines.len() as isize + 2) as u16)
                            .min(area.y + area.height);
                        let visible_h = bubble_bottom.saturating_sub(bubble_y);

                        if visible_h > 0 {
                            // We need to handle partial scroll: if cur_y < area.y,
                            // skip the top rows of the bubble.
                            let skip_top = (area.y as isize - cur_y).max(0) as usize;

                            // Build the set of lines to render, accounting for skip.
                            let bubble_area =
                                Rect::new(area.x, bubble_y, area.width, visible_h);

                            // Render using the chat_bubble widget.
                            // If skip_top > 0, we need to adjust what we render.
                            if skip_top == 0 {
                                chat_bubble(
                                    surface,
                                    bubble_area,
                                    lines,
                                    *bubble_width,
                                    *align,
                                    *style,
                                );
                            } else {
                                // Partial render: skip_top rows clipped from top.
                                // Skip 1 = top border hidden, rest = content lines.
                                let content_skip = skip_top.saturating_sub(1);
                                let remaining_lines: Vec<_> =
                                    lines.iter().skip(content_skip).cloned().collect();
                                if skip_top <= 1 {
                                    // Top border partially visible or just hidden.
                                    chat_bubble(
                                        surface,
                                        bubble_area,
                                        &remaining_lines,
                                        *bubble_width,
                                        *align,
                                        *style,
                                    );
                                } else {
                                    // Deep scroll: render remaining content + bottom border.
                                    // Use chat_bubble which always draws top border,
                                    // so it's slightly off. Accept the visual trade-off
                                    // for scroll — it's only visible during fast scroll.
                                    chat_bubble(
                                        surface,
                                        bubble_area,
                                        &remaining_lines,
                                        *bubble_width,
                                        *align,
                                        *style,
                                    );
                                }
                            }
                        }
                    }
                }
                ChatBlockKind::Plain(lines) => {
                    for (i, line) in lines.iter().enumerate() {
                        let ly = y_offset + i as isize;
                        if ly < area.y as isize {
                            continue;
                        }
                        if ly >= (area.y + area.height) as isize {
                            break;
                        }
                        let mut x = area.x;
                        for span in &line.0 {
                            let remaining = area.right().saturating_sub(x) as usize;
                            if remaining == 0 {
                                break;
                            }
                            let text: &str = &span.content;
                            let width = text.len().min(remaining);
                            surface.set_stringn(x, ly as u16, text, width, span.style);
                            x += width as u16;
                        }
                    }
                }
            }

            y_offset += block_total as isize;
        }
    }

    /// Send a prompt to the first connected agent. Returns true if sent.
    fn send_prompt(text: String, cx: &mut Context) -> bool {
        let agent = cx.editor.acp_agents.iter().next().map(|(_, a)| a.clone());

        let Some(agent) = agent else {
            cx.editor
                .set_error("No ACP agents connected. Use :acp-connect first.");
            return false;
        };

        let prompt = vec![helix_acp::ContentBlock::from(text)];
        let callback = async move {
            let result = async {
                let session_id = match agent.session_id().await {
                    Some(id) => id,
                    None => {
                        let cwd = std::env::current_dir().unwrap_or_default();
                        let session = agent.new_session(cwd).await?;
                        session.session_id
                    }
                };
                agent.prompt(session_id, prompt).await
            }
            .await;

            let cb: crate::job::Callback = match result {
                Ok(response) => {
                    let msg = format!(
                        "Done ({})",
                        match response.stop_reason {
                            helix_acp::types::StopReason::EndTurn => "completed",
                            helix_acp::types::StopReason::Cancelled => "cancelled",
                            helix_acp::types::StopReason::MaxTokens => "max tokens",
                            helix_acp::types::StopReason::MaxTurnRequests => "max turns",
                            helix_acp::types::StopReason::Refusal => "refused",
                        }
                    );
                    crate::job::Callback::EditorCompositor(Box::new(
                        move |editor: &mut Editor, compositor| {
                            editor.set_status(msg);
                            if let Some(panel) = compositor.find_id::<AcpPanel>(ID) {
                                panel.set_busy(false);
                                panel.set_panel_error(None);
                                if let Some(next_msg) = panel.dequeue_message() {
                                    let agent =
                                        editor.acp_agents.iter().next().map(|(_, a)| a.clone());
                                    if let Some(agent) = agent {
                                        panel.push_entry(ChatEntry::UserMessage(next_msg.clone()));
                                        panel.set_busy(true);
                                        dispatch_queued_prompt(agent, next_msg);
                                    }
                                }
                            }
                        },
                    ))
                }
                Err(e) => {
                    let err_msg = format!("{e}");
                    crate::job::Callback::EditorCompositor(Box::new(
                        move |_editor: &mut Editor, compositor| {
                            if let Some(panel) = compositor.find_id::<AcpPanel>(ID) {
                                panel.set_panel_error(Some(err_msg));
                                panel.set_busy(false);
                            }
                        },
                    ))
                }
            };
            Ok(cb)
        };

        cx.jobs.callback(callback);
        true
    }

    /// Dispatch a key through the input region's own engine + modal keymaps.
    /// Dispatch a key through the engine. Returns `true` if consumed, `false` if unbound
    /// (should bubble up to the editor).
    fn dispatch_input_key(&mut self, key: KeyEvent, cx: &mut Context) -> bool {
        let Some(result) = self.input.dispatch_key(cx.editor, key) else {
            return true;
        };
        self.handle_engine_result(result, cx)
    }

    /// Handle the result of an engine dispatch in the input region.
    /// Returns `true` if consumed, `false` if the key was unbound and should bubble.
    fn handle_engine_result(&mut self, result: helix_view::engine::EngineResult, cx: &mut Context) -> bool {
        use helix_view::engine::EngineResult;
        match result {
            EngineResult::Executed | EngineResult::Pending => true,
            EngineResult::Unbound => false,
            EngineResult::InsertChar(ch) => {
                self.insert_char_into_input(ch, cx.editor);
                true
            }
            EngineResult::CancelledInsert(keys) => {
                for ev in keys.iter() {
                    if let Some(ch) = ev.char() {
                        self.insert_char_into_input(ch, cx.editor);
                    }
                }
                true
            }
            EngineResult::ReplayInsert { keys, .. } => {
                // For dot-repeat in the input region, just replay the keys as chars.
                for ev in keys.iter() {
                    if let Some(ch) = ev.char() {
                        self.insert_char_into_input(ch, cx.editor);
                    }
                }
                true
            }
        }
    }

    /// Insert a single character into the input region's component document.
    fn insert_char_into_input(&self, ch: char, editor: &mut Editor) {
        let Some(doc_id) = self.input.doc_id() else {
            return;
        };
        let doc = editor
            .component_docs
            .get_mut(&doc_id)
            .expect("input doc missing");
        let text = doc.text();
        let selection = doc.selection(self.input.view_id()).clone();
        let cursors = selection.cursors(text.slice(..));
        let mut t = helix_core::Tendril::new();
        t.push(ch);
        let transaction = helix_core::Transaction::insert(text, &cursors, t);
        doc.apply(&transaction, self.input.view_id());
    }

    fn send_current_prompt(&mut self, cx: &mut Context) {
        let text = match self.input.take_text(cx.editor) {
            Some(t) => t,
            None => return,
        };
        if !text.is_empty() {
            self.panel_error = None;
            if self.agent_busy {
                self.push_entry(ChatEntry::Status(format!(
                    "Queued: {}",
                    text.chars().take(40).collect::<String>()
                )));
                self.enqueue_message(text);
            } else {
                self.push_entry(ChatEntry::UserMessage(text.clone()));
                if Self::send_prompt(text, cx) {
                    self.agent_busy = true;
                }
            }
        }
    }

    /// Handle cycle key bindings (thinking, mode, model). Returns `Some` if consumed.
    fn handle_cycle_key(&mut self, key: &KeyEvent, cx: &mut Context) -> Option<EventResult> {
        let config = cx.editor.config();
        let category = if *key == config.acp.cycle_thinking() {
            "thinking"
        } else if *key == config.acp.cycle_mode() {
            "mode"
        } else if *key == config.acp.cycle_model() {
            "model"
        } else {
            return None;
        };

        let Some((config_id, next_value, prev_value)) = self.cycle_config_option(category) else {
            cx.editor
                .set_status(format!("No {category} options from agent"));
            return Some(EventResult::Consumed(None));
        };

        self.apply_config_option_cycle(category, next_value.clone());
        let agent = cx.editor.acp_agents.iter().next().map(|(_, a)| a.clone());
        if let Some(agent) = agent {
            let cat = category.to_string();
            cx.jobs.callback(async move {
                let session_id = match agent.session_id().await {
                    Some(id) => id,
                    None => {
                        let prev = prev_value.clone();
                        let cat2 = cat.clone();
                        return Ok(crate::job::Callback::EditorCompositor(Box::new(
                            move |editor, compositor| {
                                if let Some(panel) = compositor.find_id::<AcpPanel>(ID) {
                                    panel.apply_config_option_cycle(&cat2, prev);
                                }
                                editor.set_error(format!("No session to update {cat}"));
                            },
                        )));
                    }
                };
                match agent
                    .set_session_config_option(session_id, config_id.clone(), next_value.clone())
                    .await
                {
                    Ok(_) => Ok(crate::job::Callback::EditorCompositor(Box::new(|_, _| {}))),
                    Err(e) => {
                        let cat2 = cat.clone();
                        Ok(crate::job::Callback::EditorCompositor(Box::new(
                            move |editor, compositor| {
                                if let Some(panel) = compositor.find_id::<AcpPanel>(ID) {
                                    panel.apply_config_option_cycle(&cat2, prev_value);
                                }
                                editor.set_error(format!("Failed to set {cat}: {e}"));
                            },
                        )))
                    }
                }
            });
        }
        cx.editor.set_status(format!("Cycled {category}"));
        Some(EventResult::Consumed(None))
    }
}

/// Spawn an async task that sends a queued prompt and handles the completion.
/// On completion, dispatches a callback to mark the panel idle and dequeue the next message.
pub fn dispatch_queued_prompt(agent: std::sync::Arc<helix_acp::AcpAgent>, text: String) {
    tokio::spawn(async move {
        let session_id = match agent.session_id().await {
            Some(id) => id,
            None => return,
        };
        let prompt = vec![helix_acp::ContentBlock::from(text)];
        let result = agent.prompt(session_id, prompt).await;
        let cb = match result {
            Ok(response) => {
                let done_msg = format!(
                    "Done ({})",
                    match response.stop_reason {
                        helix_acp::types::StopReason::EndTurn => "completed",
                        helix_acp::types::StopReason::Cancelled => "cancelled",
                        helix_acp::types::StopReason::MaxTokens => "max tokens",
                        helix_acp::types::StopReason::MaxTurnRequests => "max turns",
                        helix_acp::types::StopReason::Refusal => "refused",
                    }
                );
                crate::job::Callback::EditorCompositor(Box::new(
                    move |editor: &mut Editor, compositor| {
                        editor.set_status(done_msg);
                        if let Some(panel) = compositor.find_id::<AcpPanel>(ID) {
                            panel.set_busy(false);
                            if let Some(next_msg) = panel.dequeue_message() {
                                let agent = editor.acp_agents.iter().next().map(|(_, a)| a.clone());
                                if let Some(agent) = agent {
                                    panel.push_entry(ChatEntry::UserMessage(next_msg.clone()));
                                    panel.set_busy(true);
                                    dispatch_queued_prompt(agent, next_msg);
                                }
                            }
                        }
                    },
                ))
            }
            Err(e) => {
                let err_msg = format!("{e}");
                crate::job::Callback::EditorCompositor(Box::new(
                    move |_editor: &mut Editor, compositor| {
                        if let Some(panel) = compositor.find_id::<AcpPanel>(ID) {
                            panel.set_panel_error(Some(err_msg));
                            panel.set_busy(false);
                        }
                    },
                ))
            }
        };
        crate::job::dispatch_callback(cb).await;
    });
}

// ---------------------------------------------------------------------------
// Simple markdown line renderer
// ---------------------------------------------------------------------------

struct MarkdownLineStyles {
    heading: helix_view::graphics::Style,
    code: helix_view::graphics::Style,
    bold: helix_view::graphics::Style,
    italic: helix_view::graphics::Style,
    separator: helix_view::graphics::Style,
}

/// Compute the ideal bubble width for `text`: fit to the longest wrapped
/// line, then clamp to [min_w, max_w].
fn fit_bubble_width(text: &str, min_w: usize, max_w: usize) -> usize {
    let inner_max = max_w.saturating_sub(4);
    let wrapped = wrap_text(text, inner_max);
    let longest = wrapped.iter().map(|l| l.len()).max().unwrap_or(0);
    // bubble = longest line + 4 (border + padding)
    (longest + 4).clamp(min_w, max_w)
}

/// Render markdown-ish text into styled Spans lines.
/// Handles: headings (#), bold (**), italic (*), inline code (`), code blocks (```), horizontal rules (---).
/// Word-wrap text to fit within `max_width` columns.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut result = Vec::new();
    if max_width == 0 {
        return result;
    }
    for line in text.lines() {
        if line.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0;
        for word in line.split_whitespace() {
            let word_width = word.len();
            if current_width > 0 && current_width + 1 + word_width > max_width {
                result.push(current);
                current = String::new();
                current_width = 0;
            }
            if current_width > 0 {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
        }
        if !current.is_empty() || line.ends_with(' ') {
            result.push(current);
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

fn render_markdown_lines<'a>(
    text: &str,
    lines: &mut Vec<Spans<'a>>,
    base_style: helix_view::graphics::Style,
    styles: &MarkdownLineStyles,
) {
    let mut in_code_block = false;

    for line in text.lines() {
        // Code block toggle
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            if in_code_block {
                // Start of code block — just show a dim line
                lines.push(Spans::from(Span::styled(
                    "────────".to_string(),
                    styles.code,
                )));
            } else {
                lines.push(Spans::from(Span::styled(
                    "────────".to_string(),
                    styles.code,
                )));
            }
            continue;
        }

        if in_code_block {
            lines.push(Spans::from(Span::styled(format!("  {line}"), styles.code)));
            continue;
        }

        // Horizontal rules
        let trimmed = line.trim();
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed
                .chars()
                .all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && trimmed.len() >= 3
        {
            lines.push(Spans::from(Span::styled(
                "───".to_string(),
                styles.separator,
            )));
            continue;
        }

        // Headings
        if let Some(stripped) = line.strip_prefix("# ") {
            lines.push(Spans::from(Span::styled(
                stripped.to_string(),
                styles.heading,
            )));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("## ") {
            lines.push(Spans::from(Span::styled(
                stripped.to_string(),
                styles.heading,
            )));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("### ") {
            lines.push(Spans::from(Span::styled(
                stripped.to_string(),
                styles.heading,
            )));
            continue;
        }

        // Inline formatting: parse **bold**, *italic*, `code`
        let spans =
            parse_inline_markdown(line, base_style, styles.bold, styles.italic, styles.code);
        lines.push(Spans::from(spans));
    }
}

/// Parse a single line for inline markdown: **bold**, *italic*, `code`.
fn parse_inline_markdown(
    line: &str,
    base: helix_view::graphics::Style,
    bold: helix_view::graphics::Style,
    italic: helix_view::graphics::Style,
    code: helix_view::graphics::Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Inline code
        if chars[i] == '`' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base));
            }
            i += 1;
            let mut code_text = String::new();
            while i < len && chars[i] != '`' {
                code_text.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1; // skip closing `
            }
            spans.push(Span::styled(code_text, code));
            continue;
        }

        // Bold: **text**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base));
            }
            i += 2;
            let mut bold_text = String::new();
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '*') {
                bold_text.push(chars[i]);
                i += 1;
            }
            if i + 1 < len {
                i += 2; // skip closing **
            }
            spans.push(Span::styled(bold_text, bold));
            continue;
        }

        // Italic: *text*
        if chars[i] == '*' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base));
            }
            i += 1;
            let mut italic_text = String::new();
            while i < len && chars[i] != '*' {
                italic_text.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1; // skip closing *
            }
            spans.push(Span::styled(italic_text, italic));
            continue;
        }

        current.push(chars[i]);
        i += 1;
    }

    if !current.is_empty() {
        spans.push(Span::styled(current, base));
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }

    spans
}

impl Focusable for AcpPanel {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.error_marquee.touch();
        }
    }
}

impl Bounded for AcpPanel {
    fn area(&self) -> helix_view::graphics::Rect {
        self.output.area()
    }

    fn set_area(&mut self, area: helix_view::graphics::Rect) {
        self.output.set_area(area);
    }
}

impl Scrollable for AcpPanel {
    fn scroll(&self) -> usize {
        Scrollable::scroll(&self.output)
    }

    fn scroll_to(&mut self, offset: usize) {
        Scrollable::scroll_to(&mut self.output, offset);
    }

    fn content_height(&self) -> usize {
        self.output.content_height()
    }
    // max_scroll() uses self.area().height automatically via Bounded
}

// ---------------------------------------------------------------------------
// Component impl
// ---------------------------------------------------------------------------

impl Component for AcpPanel {
    fn sync(&mut self, editor: &mut Editor) {
        self.output.ensure_init(editor);
        self.input.ensure_init(editor);
        if self.focused {
            editor.focused_modal_input = self.input.input_state();
        }
        self.sync_to_model(editor);
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &RenderContext) {
        log::warn!(
            "[acp_panel] render area=({},{} {}x{})",
            area.x, area.y, area.width, area.height,
        );
        if area.width < 20 || area.height < 6 {
            return;
        }

        use helix_view::layout::{split_horizontal, split_vertical, Size};

        // Layout: [1px border | content]
        let h_areas = split_horizontal(area, &[Size::fixed(1), Size::Fill]);
        let border_area = h_areas[0];
        let inner = h_areas[1];

        let error_rows = 1u16;
        let input_rows = 5u16; // 1 top border + 3 content + 1 bottom border
        let plan_rows = self
            .plan_items
            .as_ref()
            .map(|p| (p.len() + 1).min(6) as u16)
            .unwrap_or(0);

        // Vertical layout: [header(1) | chat(fill) | plan | input | statusbar(1) | error(1)]
        let v_areas = split_vertical(
            inner,
            &[
                Size::fixed(1),          // header
                Size::Fill,              // chat content
                Size::fixed(plan_rows),  // plan
                Size::fixed(input_rows), // input
                Size::fixed(1),          // status bar
                Size::fixed(error_rows), // error line
            ],
        );
        let header_area = v_areas[0];
        let content_area_raw = v_areas[1];
        let plan_area = v_areas[2];
        let input_area_raw = v_areas[3];
        let bar_area = v_areas[4];
        let error_area = v_areas[5];

        // Inset content/plan/input by 1px on each side for padding.
        let content_area = Rect::new(
            content_area_raw.x + 1,
            content_area_raw.y,
            content_area_raw.width.saturating_sub(2),
            content_area_raw.height,
        );
        {
            let theme = cx.editor.acp_theme();
            let bg_style = theme.get("ui.background");
            surface.clear_with(inner, bg_style);

            // Left border
            let border_style = theme.get("ui.window");
            crate::widgets::vdivider(surface, border_area, border_style);

            let header_style = if self.focused {
                theme.get("ui.statusline")
            } else {
                theme.get("ui.statusline.inactive")
            };
            surface.set_style(header_area, header_style);
            let dot_style = if self.agent_busy {
                theme.get("warning")
            } else {
                theme.get("hint")
            };
            surface.set_stringn(header_area.x + 1, header_area.y, "\u{25cf}", 1, dot_style);
            surface.set_stringn(
                header_area.x + 3,
                header_area.y,
                &self.agent_name,
                header_area.width.saturating_sub(4) as usize,
                header_style,
            );
            let right_info = if self.agent_busy {
                if !self.message_queue.is_empty() {
                    format!("{} queued ", self.message_queue.len())
                } else {
                    "working ".to_string()
                }
            } else if !self.agent_version.is_empty() {
                format!("v{} ", self.agent_version)
            } else {
                String::new()
            };
            if !right_info.is_empty() {
                let right_style = if self.agent_busy {
                    theme.get("warning")
                } else {
                    theme.get("ui.statusline.inactive")
                };
                let rx = header_area.x + header_area.width.saturating_sub(right_info.len() as u16);
                surface.set_stringn(
                    rx,
                    header_area.y,
                    &right_info,
                    right_info.len(),
                    right_style,
                );
            }
            let bar_style = if self.focused {
                theme.get("ui.statusline")
            } else {
                theme.get("ui.statusline.inactive")
            };
            surface.set_style(bar_area, bar_style);
            // Bottom status: mode  model (gap between, no separator)
            let use_status_colors = cx.editor.config().color_modes;
            let theme = cx.editor.acp_theme();
            let get_style = |acp_scope: &str, statusline_fallback: &str| -> Style {
                if use_status_colors {
                    theme
                        .try_get(acp_scope)
                        .unwrap_or_else(|| theme.get(statusline_fallback))
                } else {
                    bar_style
                }
            };
            let mut spans: Vec<Span> = Vec::new();
            let append_span = |spans: &mut Vec<Span>, content: String, style: Style| {
                spans.push(Span::styled(content, bar_style.patch(style)));
            };
            let mode_name = self.current_mode_name();
            let model_name = self.status_model_display();
            if let Some(ref v) = mode_name {
                append_span(
                    &mut spans,
                    format!(" {v} "),
                    get_style("ui.acp.mode", "ui.statusline.select"),
                );
            }
            if mode_name.is_some() && model_name.is_some() {
                append_span(&mut spans, "  ".to_string(), bar_style); // gap
            }
            if let Some(ref v) = model_name {
                append_span(
                    &mut spans,
                    format!(" {v} "),
                    get_style("ui.acp.model", "ui.statusline.normal"),
                );
            }
            if !spans.is_empty() {
                let combined = Spans::from(spans);
                surface.set_spans(
                    bar_area.x,
                    bar_area.y,
                    &combined,
                    combined.width().min(bar_area.width as usize) as u16,
                );
            }
        }

        // ── Error line ──
        if self.error_marquee.has_text() {
            let error_inset = Rect::new(
                error_area.x + 1,
                error_area.y,
                error_area.width.saturating_sub(2),
                error_area.height,
            );
            let error_style = cx.editor.acp_theme().get("error");
            if let Some(when) = self.error_marquee.render(error_inset, surface, error_style) {
                schedule_redraw_at(when);
            }
        }

        // ── Plan area ──
        if plan_rows > 0 && plan_area.height > 0 {
            let theme = cx.editor.acp_theme();
            let plan_done_style = theme.get("diff.plus");
            let plan_progress_style = theme.get("warning");
            let plan_pending_style = theme.get("ui.text.inactive");
            let plan_failed_style = theme.get("error");
            let tool_name_style = theme.get("ui.text.focus");
            let items = self.plan_items.as_ref().unwrap();
            let done = items
                .iter()
                .filter(|i| i.status == PlanStatus::Completed)
                .count();
            let total = items.len();
            let plan_inset = Rect::new(
                plan_area.x + 1,
                plan_area.y,
                plan_area.width.saturating_sub(2),
                plan_area.height,
            );
            let bar_width = (plan_inset.width as usize).saturating_sub(14).min(24);
            let filled = (done * bar_width).checked_div(total).unwrap_or(0);
            let empty = bar_width.saturating_sub(filled);
            surface.set_spans(
                plan_inset.x,
                plan_inset.y,
                &Spans::from(vec![
                    Span::styled("Plan ", tool_name_style),
                    Span::styled(
                        format!(
                            "\u{2590}{}{}\u{258c} {done}/{total}",
                            "\u{2588}".repeat(filled),
                            "\u{2591}".repeat(empty)
                        ),
                        plan_progress_style,
                    ),
                ]),
                plan_inset.width,
            );
            for (i, item) in items.iter().take(5).enumerate() {
                let (icon, style) = match item.status {
                    PlanStatus::Completed => (" \u{25cf} ", plan_done_style),
                    PlanStatus::InProgress => (" \u{25ce} ", plan_progress_style),
                    PlanStatus::Failed => (" \u{2715} ", plan_failed_style),
                    PlanStatus::Pending => (" \u{25cb} ", plan_pending_style),
                };
                let y = plan_inset.y + 1 + i as u16;
                if y < plan_inset.bottom() {
                    surface.set_spans(
                        plan_inset.x,
                        y,
                        &Spans::from(vec![
                            Span::styled(icon.to_string(), style),
                            Span::styled(item.content.clone(), style),
                        ]),
                        plan_inset.width,
                    );
                }
            }
        }

        if input_area_raw.height > 0 {
            let inset = Rect::new(
                input_area_raw.x + 1,
                input_area_raw.y,
                input_area_raw.width.saturating_sub(2),
                input_area_raw.height,
            );
            let theme = cx.editor.acp_theme();
            let text_style = theme.get("ui.text");
            let placeholder_style = theme.get("ui.text.inactive");
            let input_border_style = theme
                .try_get("ui.acp.input.border")
                .unwrap_or_else(|| theme.get("ui.window"));

            // Draw rounded border around input area.
            if inset.width >= 4 && inset.height >= 3 {
                let bw = inset.width as usize;
                // Top border: ╭───╮
                let top = format!("╭{}╮", "─".repeat(bw.saturating_sub(2)));
                surface.set_stringn(inset.x, inset.y, &top, bw, input_border_style);
                // Side borders for inner rows.
                for row in 1..inset.height.saturating_sub(1) {
                    let y = inset.y + row;
                    surface.set_stringn(inset.x, y, "│", 1, input_border_style);
                    surface.set_stringn(inset.right() - 1, y, "│", 1, input_border_style);
                }
                // Bottom border: ╰───╯
                let bot = format!("╰{}╯", "─".repeat(bw.saturating_sub(2)));
                surface.set_stringn(
                    inset.x,
                    inset.y + inset.height - 1,
                    &bot,
                    bw,
                    input_border_style,
                );
            }

            // Inner text area inside the border (1px border + 1px padding each side).
            let input_area = Rect::new(
                inset.x + 2,
                inset.y + 1,
                inset.width.saturating_sub(4),
                inset.height.saturating_sub(2),
            );

            // Render component document content in the input area.
            let view_id = self.input.id();
            let is_empty = if input_area.width > 0 && input_area.height > 0 {
                if let Some(doc) = self.input.document(cx.editor) {
                    let text = doc.text();
                    let height = input_area.height as usize;
                    let total_lines = text.len_lines();

                    // Find cursor line to scroll around it.
                    let cursor_line = doc
                        .selections()
                        .get(&view_id)
                        .map(|sel| {
                            let cursor = sel.primary().cursor(text.slice(..));
                            text.char_to_line(cursor)
                        })
                        .unwrap_or(0);

                    // Scroll so cursor is visible.
                    let scroll = if cursor_line >= height {
                        cursor_line + 1 - height
                    } else {
                        0
                    };

                    // Render visible lines.
                    for row in 0..height {
                        let line_idx = scroll + row;
                        if line_idx >= total_lines {
                            break;
                        }
                        let line = text.line(line_idx);
                        let s: String = line
                            .chars()
                            .take_while(|c| *c != '\n')
                            .take(input_area.width as usize)
                            .collect();
                        surface.set_stringn(
                            input_area.x,
                            input_area.y + row as u16,
                            &s,
                            input_area.width as usize,
                            text_style,
                        );
                    }

                    // Cursor position relative to scroll.
                    if let Some(sel) = doc.selections().get(&view_id) {
                        let cursor = sel.primary().cursor(text.slice(..));
                        let line_start = text.line_to_char(cursor_line);
                        let col = cursor - line_start;
                        let screen_row = cursor_line.saturating_sub(scroll);
                        let cx_pos = input_area.x + col as u16;
                        let cy_pos = input_area.y + screen_row as u16;
                        if cy_pos < input_area.bottom() && cx_pos < input_area.right() {
                            self.last_input_cursor = Some((cx_pos, cy_pos));
                        }
                    }
                    // Empty = no content besides trailing line ending(s)
                    !text.chars().any(|c| c != '\n' && c != '\r')
                } else {
                    true
                }
            } else {
                true
            };

            if is_empty && input_area.width > 0 && input_area.height > 0 {
                surface.set_stringn(
                    input_area.x,
                    input_area.y,
                    "Ask anything...",
                    input_area.width as usize,
                    placeholder_style,
                );
            }
        }

        // ── Content area (chat history) ──────────────────────

        if content_area.height == 0 || content_area.width == 0 {
            return;
        }

        self.output.set_area(content_area);

        let theme = cx.editor.acp_theme();
        // Empty state
        if self.output.content().is_empty() {
            let empty_style = theme.get("ui.text.inactive");
            let center_y = content_area.y + content_area.height / 2;

            let msg = "Space A for menu";
            let mx = content_area.x + content_area.width.saturating_sub(msg.len() as u16) / 2;
            if center_y >= content_area.y && center_y < content_area.y + content_area.height {
                surface.set_stringn(mx, center_y, msg, content_area.width as usize, empty_style);
            }

            return;
        }

        // Render chat content using block-based rendering with chat_bubble widget.
        let blocks = self.build_blocks(theme, content_area.width);
        self.render_content(&blocks, content_area, surface);

        // Scrollbar
        let total_height = self.output.content_height();
        let scroll_from_top = Scrollable::scroll(&self.output);
        if total_height > content_area.height as usize {
            let scroll_style = theme.get("ui.menu.scroll");
            crate::widgets::Scrollbar::new(
                total_height,
                scroll_from_top,
                content_area.height as usize,
            )
            .thumb_style(
                Style::default().fg(scroll_style.fg.unwrap_or(helix_view::theme::Color::Reset)),
            )
            .track(
                "▐",
                Style::default().fg(scroll_style.bg.unwrap_or(helix_view::theme::Color::Reset)),
            )
            .render(
                Rect::new(
                    content_area_raw.right() - 1,
                    content_area.y,
                    1,
                    content_area.height,
                ),
                surface,
            );
        }
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        // When unfocused, pass all events through to the editor.
        if !self.focused {
            return EventResult::Ignored(None);
        }

        self.error_marquee.touch();

        // Ctrl+c → cancel agent (any mode).
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) {
            for (_id, agent) in cx.editor.acp_agents.iter() {
                let agent = agent.clone();
                tokio::spawn(async move {
                    if let Some(session_id) = agent.session_id().await {
                        agent.cancel(session_id);
                    }
                });
            }
            self.clear_queue();
            cx.editor.set_status("Cancelling agent...");
            return EventResult::Consumed(None);
        }

        // Cycle keys work in any mode.
        if let Some(result) = self.handle_cycle_key(&key, cx) {
            return result;
        }

        // ── Mode transitions ──
        // The component modal keymaps filter out Frontend commands (insert_mode,
        // command_mode, etc.), so we handle mode switching explicitly here.
        if self.input.mode() == Mode::Insert {
            // Escape in insert mode → back to normal.
            if matches!(key.code, KeyCode::Esc) && key.modifiers.is_empty() {
                self.input.exit_insert_mode();
                return EventResult::Consumed(None);
            }
            // Tab → insert tab character (not bound in component keymaps).
            if matches!(key.code, KeyCode::Tab) && key.modifiers.is_empty() {
                self.insert_char_into_input('\t', cx.editor);
                return EventResult::Consumed(None);
            }
            // Shift+Tab → dedent (remove leading whitespace on current line).
            if matches!(key.code, KeyCode::Tab)
                && key.modifiers.contains(KeyModifiers::SHIFT)
            {
                if let Some(doc_id) = self.input.doc_id() {
                    if let Some(doc) = cx.editor.component_docs.get_mut(&doc_id) {
                        let text = doc.text();
                        let sel = doc.selection(self.input.view_id()).clone();
                        let cursor = sel.primary().cursor(text.slice(..));
                        let line = text.char_to_line(cursor);
                        let line_start = text.line_to_char(line);
                        // Remove up to 4 leading spaces or 1 tab.
                        let line_text = text.line(line);
                        let mut remove = 0usize;
                        for ch in line_text.chars() {
                            if ch == '\t' && remove == 0 {
                                remove = 1;
                                break;
                            } else if ch == ' ' && remove < 4 {
                                remove += 1;
                            } else {
                                break;
                            }
                        }
                        if remove > 0 {
                            let transaction = helix_core::Transaction::change(
                                text,
                                [(line_start, line_start + remove, None)].into_iter(),
                            );
                            doc.apply(&transaction, self.input.view_id());
                        }
                    }
                }
                return EventResult::Consumed(None);
            }
        } else {
            // Normal mode key handling.
            if key.modifiers.is_empty() {
                match key.code {
                    // Enter → send prompt.
                    KeyCode::Enter => {
                        self.send_current_prompt(cx);
                        return EventResult::Consumed(None);
                    }
                    // i/a/I/A → enter insert mode.
                    KeyCode::Char('i') => {
                        self.input.enter_insert_mode("insert_mode".into());
                        return EventResult::Consumed(None);
                    }
                    KeyCode::Char('a') => {
                        self.input.enter_insert_mode("append_mode".into());
                        // Move cursor forward by one (append behavior).
                        if let Some(doc_id) = self.input.doc_id() {
                            if let Some(doc) = cx.editor.component_docs.get_mut(&doc_id) {
                                let text = doc.text().slice(..);
                                let selection = doc
                                    .selection(self.input.view_id())
                                    .clone()
                                    .transform(|range| {
                                        let pos = helix_core::graphemes::next_grapheme_boundary(
                                            text,
                                            range.cursor(text),
                                        );
                                        helix_core::Range::new(pos, pos)
                                    });
                                doc.set_selection(self.input.view_id(), selection);
                            }
                        }
                        return EventResult::Consumed(None);
                    }
                    KeyCode::Char('I') => {
                        self.input.enter_insert_mode("insert_mode".into());
                        // Move cursor to start of line.
                        if let Some(doc_id) = self.input.doc_id() {
                            if let Some(doc) = cx.editor.component_docs.get_mut(&doc_id) {
                                let text = doc.text().slice(..);
                                let selection = doc
                                    .selection(self.input.view_id())
                                    .clone()
                                    .transform(|range| {
                                        let line = text.char_to_line(range.cursor(text));
                                        let pos = text.line_to_char(line);
                                        helix_core::Range::new(pos, pos)
                                    });
                                doc.set_selection(self.input.view_id(), selection);
                            }
                        }
                        return EventResult::Consumed(None);
                    }
                    KeyCode::Char('A') => {
                        self.input.enter_insert_mode("append_mode".into());
                        // Move cursor to end of line.
                        if let Some(doc_id) = self.input.doc_id() {
                            if let Some(doc) = cx.editor.component_docs.get_mut(&doc_id) {
                                let text = doc.text().slice(..);
                                let selection = doc
                                    .selection(self.input.view_id())
                                    .clone()
                                    .transform(|range| {
                                        let line = text.char_to_line(range.cursor(text));
                                        let pos = helix_core::line_ending::line_end_char_index(&text, line);
                                        helix_core::Range::new(pos, pos)
                                    });
                                doc.set_selection(self.input.view_id(), selection);
                            }
                        }
                        return EventResult::Consumed(None);
                    }
                    _ => {}
                }
            }
        }

        // Dispatch through the region's own engine + keymaps.
        // If the engine says the key is unbound, let it bubble up to the editor
        // (e.g. `:` for command mode, or any other user-bound Frontend command).
        if self.dispatch_input_key(*key, cx) {
            log::warn!("[acp_event] key={} mode={:?} → Consumed (engine handled)", key.key_sequence_format(), self.input.mode());
            EventResult::Consumed(None)
        } else {
            log::warn!("[acp_event] key={} mode={:?} → Ignored (unbound, bubbling)", key.key_sequence_format(), self.input.mode());
            EventResult::Ignored(None)
        }
    }

    fn cursor(&self, _area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        if self.focused {
            if let Some((cx, cy)) = self.last_input_cursor {
                let pos = Position::new(cy as usize, cx as usize);
                let kind = ctx.config().cursor_shape.from_mode(self.input.mode());
                return (Some(pos), kind);
            }
        }
        (None, CursorKind::Hidden)
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }

    fn layout_role(&self) -> crate::compositor::LayoutRole {
        crate::compositor::LayoutRole::Docked
    }

    fn panel_id(&self) -> Option<helix_view::model::PanelId> {
        self.model_panel_id
    }

    component_traits!(focusable, scrollable);
}

// ---------------------------------------------------------------------------
// Permission popup
// ---------------------------------------------------------------------------

/// A popup that shows when an ACP agent requests permission.
pub struct PermissionPopup {
    title: String,
    description: Option<String>,
    options: Vec<PermissionChoice>,
    selected: usize,
    /// Channel to send the response back to the handler.
    response_tx: Option<tokio::sync::oneshot::Sender<PermissionResponse>>,
}

pub struct PermissionChoice {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
}

pub enum PermissionResponse {
    Selected(String),
    Dismissed,
}

impl PermissionPopup {
    pub fn new(
        title: String,
        description: Option<String>,
        options: Vec<PermissionChoice>,
        response_tx: tokio::sync::oneshot::Sender<PermissionResponse>,
    ) -> Self {
        Self {
            title,
            description,
            options,
            selected: 0,
            response_tx: Some(response_tx),
        }
    }

    fn send_response(&mut self, response: PermissionResponse) {
        if let Some(tx) = self.response_tx.take() {
            let _ = tx.send(response);
        }
    }
}

impl Component for PermissionPopup {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &RenderContext) {
        let popup_width = 50u16.min(area.width.saturating_sub(4));
        let option_lines = self.options.len() as u16;
        let desc_lines = self
            .description
            .as_ref()
            .map(|d| d.lines().count() as u16)
            .unwrap_or(0);
        let popup_height = (4 + option_lines + desc_lines).min(area.height.saturating_sub(4));

        let x = (area.width.saturating_sub(popup_width)) / 2;
        let y = (area.height.saturating_sub(popup_height)) / 2;

        let popup_area = Rect {
            x,
            y,
            width: popup_width,
            height: popup_height,
        };

        // Background
        let bg = cx.editor.acp_theme().get("ui.popup");
        for py in popup_area.y..popup_area.y + popup_area.height {
            for px in popup_area.x..popup_area.x + popup_area.width {
                surface[(px, py)].set_style(bg).set_symbol(" ");
            }
        }

        // Border top
        let border_style = cx.editor.acp_theme().get("ui.popup");
        let top_border = format!("┌{}┐", "─".repeat(popup_width.saturating_sub(2) as usize));
        surface.set_stringn(x, y, &top_border, popup_width as usize, border_style);

        // Title
        let title_style = cx
            .editor
            .acp_theme()
            .get("ui.text")
            .add_modifier(Modifier::BOLD);
        let title_line = format!(
            "│ {:<width$}│",
            self.title,
            width = (popup_width - 4) as usize
        );
        surface.set_stringn(x, y + 1, &title_line, popup_width as usize, title_style);

        let mut row = y + 2;

        // Description
        if let Some(ref desc) = self.description {
            let desc_style = cx.editor.acp_theme().get("ui.text.inactive");
            for line in desc.lines() {
                let padded = format!("│ {:<width$}│", line, width = (popup_width - 4) as usize);
                surface.set_stringn(x, row, &padded, popup_width as usize, desc_style);
                row += 1;
            }
        }

        // Separator
        let sep = format!("├{}┤", "─".repeat(popup_width.saturating_sub(2) as usize));
        surface.set_stringn(x, row, &sep, popup_width as usize, border_style);
        row += 1;

        // Options
        for (i, opt) in self.options.iter().enumerate() {
            let is_selected = i == self.selected;
            let marker = if is_selected { ">" } else { " " };
            let style = if is_selected {
                cx.editor.acp_theme().get("ui.menu.selected")
            } else {
                cx.editor.acp_theme().get("ui.text")
            };
            let opt_line = format!(
                "│{marker} {:<width$}│",
                opt.title,
                width = (popup_width - 5) as usize
            );
            surface.set_stringn(x, row, &opt_line, popup_width as usize, style);
            row += 1;
        }

        // Border bottom
        let bottom_border = format!("└{}┘", "─".repeat(popup_width.saturating_sub(2) as usize));
        if row < popup_area.y + popup_area.height {
            surface.set_stringn(x, row, &bottom_border, popup_width as usize, border_style);
        }
    }

    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                EventResult::Consumed(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.options.len() {
                    self.selected += 1;
                }
                EventResult::Consumed(None)
            }
            KeyCode::Enter => {
                if let Some(opt) = self.options.get(self.selected) {
                    self.send_response(PermissionResponse::Selected(opt.id.clone()));
                }
                let callback: crate::compositor::Callback = Box::new(|compositor, _cx| {
                    compositor.remove(PERMISSION_ID);
                });
                EventResult::Consumed(Some(callback))
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.send_response(PermissionResponse::Dismissed);
                let callback: crate::compositor::Callback = Box::new(|compositor, _cx| {
                    compositor.remove(PERMISSION_ID);
                });
                EventResult::Consumed(Some(callback))
            }
            // Number shortcuts: 1-9 to select
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < self.options.len() {
                    self.send_response(PermissionResponse::Selected(self.options[idx].id.clone()));
                    let callback: crate::compositor::Callback = Box::new(|compositor, _cx| {
                        compositor.remove(PERMISSION_ID);
                    });
                    EventResult::Consumed(Some(callback))
                } else {
                    EventResult::Consumed(None)
                }
            }
            _ => EventResult::Consumed(None),
        }
    }

    fn id(&self) -> Option<&'static str> {
        Some(PERMISSION_ID)
    }
}
