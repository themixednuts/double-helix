use crate::compositor::{Component, Context, Event, EventResult};
use crate::ui::prompt::Movement;
use crate::ui::{completers, marquee::Marquee, Prompt, PromptEvent};
use helix_core::Position;
use helix_view::document::Mode;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::input::{KeyCode, KeyEvent, KeyModifiers};
use helix_view::theme::Modifier;
use helix_view::theme::Style;
use helix_view::Editor;
use tui::buffer::Buffer as Surface;
use tui::text::{Span, Spans, Text};
use tui::widgets::{Paragraph, Widget, Wrap};

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
    entries: Vec<ChatEntry>,
    scroll: u16,
    focused: bool,
    /// Chat input mode: false = Normal (modal), true = Insert (typing).
    chat_insert_mode: bool,
    /// Helix Prompt for the input line (used in Insert mode; Normal mode uses vim-style bindings).
    prompt: Prompt,
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
    /// Last content area dimensions and max scroll (for scroll/navigation). Set during render.
    last_content_height: u16,
    last_content_width: u16,
    last_max_scroll: u16,
}

impl Default for AcpPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpPanel {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            scroll: 0,
            focused: true,
            chat_insert_mode: false,
            prompt: Prompt::new(
                "".into(),
                None,
                completers::none,
                |_cx: &mut Context, _s: &str, _e: PromptEvent| {},
            ),
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
            last_content_height: 0,
            last_content_width: 0,
            last_max_scroll: 0,
        }
    }

    fn last_content_width(&self) -> u16 {
        self.last_content_width
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.error_marquee.touch();
        }
        if !focused {
            self.chat_insert_mode = false;
        }
    }

    /// Set or clear the panel error message (shown below status line; keeps ACP context in panel).
    pub fn set_panel_error(&mut self, msg: Option<String>) {
        self.panel_error = msg.clone();
        self.error_marquee
            .set_text(msg.map(|s| format!("ACP: {}", s)));
    }

    pub fn toggle_focus(&mut self) {
        self.set_focused(!self.focused);
    }

    /// Focus the panel and enter insert mode in the chat input.
    pub fn activate_input(&mut self) {
        self.set_focused(true);
        self.chat_insert_mode = true;
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
        self.entries.push(entry);
        self.scroll = 0;
        self.selected_entry = None;
    }

    /// Append text to the last AgentText entry, or create one.
    pub fn append_agent_text(&mut self, text: &str) {
        if let Some(ChatEntry::AgentText(ref mut existing)) = self.entries.last_mut() {
            existing.push_str(text);
        } else {
            self.entries.push(ChatEntry::AgentText(text.to_string()));
        }
        self.scroll = 0;
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
        for entry in self.entries.iter_mut().rev() {
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
            self.entries.push(ChatEntry::ToolCall {
                id: id.to_string(),
                name: n.to_string(),
                path: path.map(|s| s.to_string()),
                status: status.to_string(),
            });
            self.scroll = 0;
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

    /// Build styled text lines for rendering.
    fn build_text<'a>(&self, theme: &helix_view::Theme, width: u16) -> Text<'a> {
        let mut lines: Vec<Spans> = Vec::new();

        let user_bubble = theme.get("ui.acp.user_bubble");
        let agent_bubble = theme.get("ui.acp.agent_bubble");

        let agent_style = theme.get("ui.text.info").patch(agent_bubble);
        let user_label_style = theme
            .get("keyword")
            .add_modifier(Modifier::BOLD)
            .patch(user_bubble);
        let user_text_style = theme.get("ui.text").patch(user_bubble);
        let tool_icon_style = theme.get("ui.text.inactive").patch(agent_bubble);
        let tool_name_style = theme.get("ui.text.focus").patch(agent_bubble);
        let tool_detail_style = theme.get("ui.text.inactive").patch(agent_bubble);
        let separator_style = theme.get("ui.statusline.separator");
        let heading_style = theme.get("markup.heading.1").patch(agent_bubble);
        let code_style = theme.get("markup.raw.inline").patch(agent_bubble);
        let bold_style = agent_style.add_modifier(Modifier::BOLD);
        let italic_style = agent_style.add_modifier(Modifier::ITALIC);
        let status_dim_style = theme.get("ui.text.inactive");
        let selection_style = theme.get("ui.selection");

        for (entry_idx, entry) in self.entries.iter().enumerate() {
            let is_selected = self.selected_entry == Some(entry_idx);
            let (user_style, agent_style_use, label_style, heading_sel, code_sel, bold_sel, italic_sel) =
                if is_selected {
                    (
                        user_text_style.patch(selection_style),
                        agent_style.patch(selection_style),
                        user_label_style.patch(selection_style),
                        heading_style.patch(selection_style),
                        code_style.patch(selection_style),
                        bold_style.patch(selection_style),
                        italic_style.patch(selection_style),
                    )
                } else {
                    (
                        user_text_style,
                        agent_style,
                        user_label_style,
                        heading_style,
                        code_style,
                        bold_style,
                        italic_style,
                    )
                };
            match entry {
                ChatEntry::UserMessage(text) => {
                    // User: right-aligned (label and text), block background.
                    // "You" label is never part of selection; only the message content is.
                    let you_pad = width.saturating_sub(3).min(width);
                    lines.push(Spans::from(vec![
                        Span::styled(" ".repeat(you_pad as usize), user_text_style),
                        Span::styled("You", user_label_style),
                    ]));
                    for line in text.lines() {
                        let pad = width.saturating_sub(line.len() as u16 + 2).min(width);
                        let pad_str = " ".repeat(pad as usize);
                        lines.push(Spans::from(vec![
                            Span::styled(pad_str, user_style),
                            Span::styled(format!(" {line}"), user_style),
                        ]));
                    }
                    lines.push(Spans::default());
                }
                ChatEntry::AgentText(text) => {
                    // Agent: left-aligned, block background
                    render_markdown_lines(
                        text,
                        &mut lines,
                        agent_style_use,
                        &MarkdownLineStyles {
                            heading: heading_sel,
                            code: code_sel,
                            bold: bold_sel,
                            italic: italic_sel,
                            separator: separator_style,
                        },
                    );
                    lines.push(Spans::default());
                }
                ChatEntry::ToolCall {
                    name, path, status, ..
                } => {
                    let (icon_style, name_style, detail_style) = if is_selected {
                        (
                            tool_icon_style.patch(selection_style),
                            tool_name_style.patch(selection_style),
                            tool_detail_style.patch(selection_style),
                        )
                    } else {
                        (tool_icon_style, tool_name_style, tool_detail_style)
                    };
                    let icon = match status.as_str() {
                        "running" => "\u{25ce}",            // ◎
                        "completed" | "done" => "\u{25cf}", // ●
                        "failed" => "\u{2715}",             // ✕
                        "cancelled" => "\u{2013}",          // –
                        _ => "\u{25cb}",                    // ○
                    };
                    lines.push(Spans::from(vec![
                        Span::styled(format!(" {icon} "), icon_style),
                        Span::styled(name.clone(), name_style),
                    ]));
                    if let Some(ref p) = path {
                        lines.push(Spans::from(Span::styled(
                            format!("     {p}"),
                            detail_style,
                        )));
                    }
                }
                ChatEntry::Status(text) => {
                    let style = if is_selected {
                        status_dim_style.patch(selection_style)
                    } else {
                        status_dim_style
                    };
                    lines.push(Spans::from(Span::styled(
                        format!(" {text}"),
                        style,
                    )));
                    lines.push(Spans::default());
                }
            }
        }

        Text::from(lines)
    }

    /// Wrapped line offset of each entry's start. Used for j/k entry navigation.
    fn entry_line_offsets(&self, theme: &helix_view::Theme, width: u16) -> Vec<u16> {
        let mut offsets = vec![0];
        let user_bubble = theme.get("ui.acp.user_bubble");
        let agent_bubble = theme.get("ui.acp.agent_bubble");
        let agent_style = theme.get("ui.text.info").patch(agent_bubble);
        let user_label_style = theme
            .get("keyword")
            .add_modifier(Modifier::BOLD)
            .patch(user_bubble);
        let user_text_style = theme.get("ui.text").patch(user_bubble);
        let tool_icon_style = theme.get("ui.text.inactive").patch(agent_bubble);
        let tool_name_style = theme.get("ui.text.focus").patch(agent_bubble);
        let tool_detail_style = theme.get("ui.text.inactive").patch(agent_bubble);
        let separator_style = theme.get("ui.statusline.separator");
        let heading_style = theme.get("markup.heading.1").patch(agent_bubble);
        let code_style = theme.get("markup.raw.inline").patch(agent_bubble);
        let bold_style = agent_style.add_modifier(Modifier::BOLD);
        let italic_style = agent_style.add_modifier(Modifier::ITALIC);
        let status_dim_style = theme.get("ui.text.inactive");
        let styles = MarkdownLineStyles {
            heading: heading_style,
            code: code_style,
            bold: bold_style,
            italic: italic_style,
            separator: separator_style,
        };

        for entry in &self.entries {
            let mut lines: Vec<Spans> = Vec::new();
            match entry {
                ChatEntry::UserMessage(text) => {
                    let you_pad = width.saturating_sub(3).min(width);
                    lines.push(Spans::from(vec![
                        Span::styled(" ".repeat(you_pad as usize), user_text_style),
                        Span::styled("You", user_label_style),
                    ]));
                    for line in text.lines() {
                        let pad = width.saturating_sub(line.len() as u16 + 2).min(width);
                        lines.push(Spans::from(vec![
                            Span::styled(" ".repeat(pad as usize), user_text_style),
                            Span::styled(format!(" {line}"), user_text_style),
                        ]));
                    }
                    lines.push(Spans::default());
                }
                ChatEntry::AgentText(text) => {
                    render_markdown_lines(text, &mut lines, agent_style, &styles);
                    lines.push(Spans::default());
                }
                ChatEntry::ToolCall { name, path, status, .. } => {
                    let icon = match status.as_str() {
                        "running" => "\u{25ce}",
                        "completed" | "done" => "\u{25cf}",
                        "failed" => "\u{2715}",
                        "cancelled" => "\u{2013}",
                        _ => "\u{25cb}",
                    };
                    lines.push(Spans::from(vec![
                        Span::styled(format!(" {icon} "), tool_icon_style),
                        Span::styled(name.clone(), tool_name_style),
                    ]));
                    if let Some(ref p) = path {
                        lines.push(Spans::from(Span::styled(
                            format!("     {p}"),
                            tool_detail_style,
                        )));
                    }
                }
                ChatEntry::Status(text) => {
                    lines.push(Spans::from(Span::styled(
                        format!(" {text}"),
                        status_dim_style,
                    )));
                    lines.push(Spans::default());
                }
            }
            let entry_text = Text::from(lines);
            let (_, h) = Paragraph::new(&entry_text)
                .wrap(Wrap { trim: false })
                .required_size(width);
            offsets.push(offsets.last().copied().unwrap_or(0) + h);
        }
        offsets
    }

    /// Plain text of an entry (for copying).
    fn entry_text(&self, idx: usize) -> Option<String> {
        self.entries.get(idx).map(|e| match e {
            ChatEntry::UserMessage(s) => s.clone(),
            ChatEntry::AgentText(s) => s.clone(),
            ChatEntry::ToolCall { name, path, .. } => {
                path.as_ref()
                    .map(|p| format!("{name}\n{p}"))
                    .unwrap_or_else(|| name.clone())
            }
            ChatEntry::Status(s) => s.clone(),
        })
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

/// Render markdown-ish text into styled Spans lines.
/// Handles: headings (#), bold (**), italic (*), inline code (`), code blocks (```), horizontal rules (---).
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

// ---------------------------------------------------------------------------
// Component impl
// ---------------------------------------------------------------------------

impl Component for AcpPanel {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        if area.width < 20 || area.height < 6 {
            return;
        }

        let panel_area = area;
        let inner = Rect {
            x: panel_area.x + 1,
            y: panel_area.y,
            width: panel_area.width.saturating_sub(1),
            height: panel_area.height,
        };
        let error_rows = 1; // Always reserve row below status (like main editor)
        let bar_y = inner.y + inner.height - 1 - error_rows;

        {
            let theme = cx.editor.acp_theme();
            let bg_style = theme.get("ui.background");
            surface.clear_with(
                Rect {
                    x: panel_area.x + 1,
                    y: panel_area.y,
                    width: panel_area.width.saturating_sub(1),
                    height: panel_area.height,
                },
                bg_style,
            );
            let border_style = theme.get("ui.window");
            for y in panel_area.y..panel_area.y + panel_area.height {
                surface[(panel_area.x, y)]
                    .set_symbol(tui::symbols::line::VERTICAL)
                    .set_style(border_style);
            }
            let header_style = if self.focused {
                theme.get("ui.statusline")
            } else {
                theme.get("ui.statusline.inactive")
            };
            let header_area = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 1,
            };
            surface.set_style(header_area, header_style);
            let dot_style = if self.agent_busy {
                theme.get("warning")
            } else {
                theme.get("hint")
            };
            surface.set_stringn(inner.x + 1, inner.y, "\u{25cf}", 1, dot_style);
            surface.set_stringn(
                inner.x + 3,
                inner.y,
                &self.agent_name,
                inner.width.saturating_sub(4) as usize,
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
                let rx = inner.x + inner.width.saturating_sub(right_info.len() as u16);
                surface.set_stringn(rx, inner.y, &right_info, right_info.len(), right_style);
            }
            let bar_style = if self.focused {
                theme.get("ui.statusline")
            } else {
                theme.get("ui.statusline.inactive")
            };
            let bar_area = Rect {
                x: inner.x,
                y: bar_y,
                width: inner.width,
                height: 1,
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
            // Chat input mode: NORMAL | INSERT
            let chat_mode_label = if self.chat_insert_mode {
                "INSERT"
            } else {
                "NORMAL"
            };
            append_span(
                &mut spans,
                format!(" {chat_mode_label} "),
                get_style("ui.acp.chat_mode", "ui.statusline.mode"),
            );
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
                    inner.x,
                    bar_y,
                    &combined,
                    combined.width().min(inner.width as usize) as u16,
                );
            }
        }

        // ── Error line: below status bar (marquee if too long) ──
        if self.error_marquee.has_text() {
            let error_y = bar_y + 1;
            let error_area = Rect {
                x: inner.x + 1,
                y: error_y,
                width: inner.width.saturating_sub(2),
                height: 1,
            };
            let error_style = cx.editor.acp_theme().get("error");
            self.error_marquee.render(error_area, surface, error_style);
        }

        // ── Plan area: above input, static UI (not in chat history) ──
        let plan_rows = self
            .plan_items
            .as_ref()
            .map(|p| (p.len() + 1).min(6) as u16) // header + items, cap at 6
            .unwrap_or(0);

        // ── Input area: above status bar when active ──
        let input_rows = if self.focused { 1 } else { 0 };
        let plan_y = bar_y.saturating_sub(1 + input_rows + plan_rows);
        if plan_rows > 0 && plan_y >= inner.y {
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
            let bar_width = (inner.width as usize).saturating_sub(14).min(24);
            let filled = (done * bar_width).checked_div(total).unwrap_or(0);
            let empty = bar_width.saturating_sub(filled);
            surface.set_spans(
                inner.x + 1,
                plan_y,
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
                inner.width.saturating_sub(2),
            );
            for (i, item) in items.iter().take(5).enumerate() {
                let (icon, style) = match item.status {
                    PlanStatus::Completed => (" \u{25cf} ", plan_done_style),
                    PlanStatus::InProgress => (" \u{25ce} ", plan_progress_style),
                    PlanStatus::Failed => (" \u{2715} ", plan_failed_style),
                    PlanStatus::Pending => (" \u{25cb} ", plan_pending_style),
                };
                let y = plan_y + 1 + i as u16;
                if y < bar_y {
                    surface.set_spans(
                        inner.x + 1,
                        y,
                        &Spans::from(vec![
                            Span::styled(icon.to_string(), style),
                            Span::styled(item.content.clone(), style),
                        ]),
                        inner.width.saturating_sub(2),
                    );
                }
            }
        }

        if self.focused {
            let input_y = bar_y.saturating_sub(1);
            if input_y >= inner.y {
                let input_area = Rect {
                    x: inner.x + 1,
                    y: input_y,
                    width: inner.width.saturating_sub(2),
                    height: 1,
                };
                let placeholder_style = cx.editor.acp_theme().get("ui.text.inactive");
                self.prompt.render(input_area, surface, cx);
                if self.prompt.line().is_empty() {
                    surface.set_stringn(
                        input_area.x,
                        input_area.y,
                        "Ask anything...",
                        input_area.width as usize,
                        placeholder_style,
                    );
                }
            }
        }

        // ── Content area (chat history) ──────────────────────
        let content_area = Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: inner
                .height
                .saturating_sub(2 + error_rows + input_rows + plan_rows),
        };

        if content_area.height == 0 || content_area.width == 0 {
            return;
        }

        let theme = cx.editor.acp_theme();
        // Empty state
        if self.entries.is_empty() {
            let empty_style = theme.get("ui.text.inactive");
            let center_y = content_area.y + content_area.height / 2;

            let msg = "Space A for menu";
            let mx = content_area.x + content_area.width.saturating_sub(msg.len() as u16) / 2;
            if center_y >= content_area.y && center_y < content_area.y + content_area.height {
                surface.set_stringn(mx, center_y, msg, content_area.width as usize, empty_style);
            }

            return;
        }

        // Render chat content
        self.last_content_height = content_area.height;
        self.last_content_width = content_area.width;
        let text = self.build_text(theme, content_area.width);
        let par = Paragraph::new(&text).wrap(Wrap { trim: false });
        let (_, total_lines) = par.required_size(content_area.width);

        let max_scroll = total_lines.saturating_sub(content_area.height);
        self.last_max_scroll = max_scroll;
        let scroll = self.scroll.min(max_scroll);
        let scroll_from_top = max_scroll.saturating_sub(scroll);

        par.scroll((scroll_from_top, 0))
            .render(content_area, surface);

        // Scrollbar — matches Popup's half-block convention
        if total_lines > content_area.height {
            let win_height = content_area.height as usize;
            let len = total_lines as usize;
            let scroll_style = theme.get("ui.menu.scroll");
            let scroll_height = win_height.pow(2).div_ceil(len).min(win_height);
            let scroll_line = (win_height - scroll_height) * scroll_from_top as usize
                / std::cmp::max(1, len.saturating_sub(win_height));
            let bar_x = inner.x + inner.width - 1;

            for i in 0..win_height {
                let cell = &mut surface[(bar_x, content_area.y + i as u16)];
                if scroll_line <= i && i < scroll_line + scroll_height {
                    cell.set_symbol("▐");
                    cell.set_fg(scroll_style.fg.unwrap_or(helix_view::theme::Color::Reset));
                } else {
                    cell.set_symbol("▐");
                    cell.set_fg(scroll_style.bg.unwrap_or(helix_view::theme::Color::Reset));
                }
            }
        }
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        // When unfocused, pass all events through to the editor
        if !self.focused {
            return EventResult::Ignored(None);
        }
        self.error_marquee.touch();

        if self.chat_insert_mode {
            // Insert mode: Esc -> Normal, Enter -> send, rest to Prompt.
            match key {
                KeyEvent {
                    code: KeyCode::Esc, ..
                } => {
                    self.chat_insert_mode = false;
                    EventResult::Consumed(None)
                }
                KeyEvent {
                    code: KeyCode::Enter,
                    ..
                } => {
                    let text = self.prompt.line().clone();
                    if !text.is_empty() {
                        self.prompt.set_line(String::new(), cx.editor);
                        self.chat_insert_mode = false;
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
                    EventResult::Consumed(None)
                }
                _ => self.prompt.handle_event(event, cx),
            }
        } else {
            match key {
                // Unfocus panel (return to editor)
                KeyEvent {
                    code: KeyCode::Esc, ..
                } => {
                    self.set_focused(false);
                    EventResult::Consumed(None)
                }
                // Send message (same as Insert mode)
                KeyEvent {
                    code: KeyCode::Enter,
                    ..
                } => {
                    let text = self.prompt.line().clone();
                    if !text.is_empty() {
                        self.prompt.set_line(String::new(), cx.editor);
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
                    EventResult::Consumed(None)
                }
                // Close panel
                KeyEvent {
                    code: KeyCode::Char('q'),
                    ..
                } => {
                    let callback: crate::compositor::Callback = Box::new(|compositor, _cx| {
                        compositor.remove(ID);
                    });
                    EventResult::Consumed(Some(callback))
                }
                // Enter insert mode
                KeyEvent {
                    code: KeyCode::Char('i'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.chat_insert_mode = true;
                    EventResult::Consumed(None)
                }
                // Append (cursor at end, enter insert)
                KeyEvent {
                    code: KeyCode::Char('a'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.prompt.move_end();
                    self.chat_insert_mode = true;
                    EventResult::Consumed(None)
                }
                // Insert at start
                KeyEvent {
                    code: KeyCode::Char('I'),
                    modifiers: KeyModifiers::SHIFT,
                    ..
                } => {
                    self.prompt.move_start();
                    self.chat_insert_mode = true;
                    EventResult::Consumed(None)
                }
                // Append at end
                KeyEvent {
                    code: KeyCode::Char('A'),
                    modifiers: KeyModifiers::SHIFT,
                    ..
                } => {
                    self.prompt.move_end();
                    self.chat_insert_mode = true;
                    EventResult::Consumed(None)
                }
                // Normal mode: movement in the line
                KeyEvent {
                    code: KeyCode::Char('h'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.prompt.move_cursor(Movement::BackwardChar(1));
                    EventResult::Consumed(None)
                }
                KeyEvent {
                    code: KeyCode::Char('l'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.prompt.move_cursor(Movement::ForwardChar(1));
                    EventResult::Consumed(None)
                }
                KeyEvent {
                    code: KeyCode::Char('w'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.prompt.move_cursor(Movement::ForwardWord(1));
                    EventResult::Consumed(None)
                }
                KeyEvent {
                    code: KeyCode::Char('b'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.prompt.move_cursor(Movement::BackwardWord(1));
                    EventResult::Consumed(None)
                }
                KeyEvent {
                    code: KeyCode::Char('0'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.prompt.move_start();
                    EventResult::Consumed(None)
                }
                KeyEvent {
                    code: KeyCode::Char('$'),
                    modifiers: KeyModifiers::SHIFT,
                    ..
                } => {
                    self.prompt.move_end();
                    EventResult::Consumed(None)
                }
                // Delete char under cursor
                KeyEvent {
                    code: KeyCode::Char('x'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.prompt.delete_char_forwards(cx.editor);
                    EventResult::Consumed(None)
                }
                // Delete char before cursor
                KeyEvent {
                    code: KeyCode::Char('X'),
                    modifiers: KeyModifiers::SHIFT,
                    ..
                } => {
                    self.prompt.delete_char_backwards(cx.editor);
                    EventResult::Consumed(None)
                }
                // Prev entry: Ctrl+p / Up / k (Shift+Tab reserved for cycle_mode)
                KeyEvent {
                    code: KeyCode::Char('p'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent { code: KeyCode::Up, .. }
                | KeyEvent { code: KeyCode::Char('k'), .. } => {
                    if self.entries.is_empty() || self.last_content_height == 0 {
                        EventResult::Consumed(None)
                    } else {
                        let theme = cx.editor.acp_theme();
                        let offsets = self.entry_line_offsets(theme, self.last_content_width());
                        let max_scroll = offsets
                            .last()
                            .copied()
                            .unwrap_or(0)
                            .saturating_sub(self.last_content_height);
                        match self.selected_entry {
                            None => {
                                self.selected_entry = Some(self.entries.len().saturating_sub(1));
                                self.scroll = max_scroll
                                    .saturating_sub(
                                        offsets.get(self.selected_entry.unwrap()).copied().unwrap_or(0),
                                    )
                                    .min(max_scroll);
                            }
                            Some(0) => {
                                self.scroll = self.scroll.saturating_add(1).min(max_scroll);
                            }
                            Some(i) => {
                                self.selected_entry = Some(i - 1);
                                let entry_start = offsets.get(i - 1).copied().unwrap_or(0);
                                self.scroll = max_scroll
                                    .saturating_sub(entry_start)
                                    .min(max_scroll);
                            }
                        }
                        EventResult::Consumed(None)
                    }
                }
                // Next entry: Ctrl+n / Down / j (Tab reserved for potential future use)
                KeyEvent {
                    code: KeyCode::Char('n'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent { code: KeyCode::Down, .. }
                | KeyEvent { code: KeyCode::Char('j'), .. } => {
                    if self.entries.is_empty() || self.last_content_height == 0 {
                        EventResult::Consumed(None)
                    } else {
                        let theme = cx.editor.acp_theme();
                        let offsets = self.entry_line_offsets(theme, self.last_content_width());
                        let max_scroll = offsets
                            .last()
                            .copied()
                            .unwrap_or(0)
                            .saturating_sub(self.last_content_height);
                        match self.selected_entry {
                            None => {
                                self.selected_entry = Some(0);
                                self.scroll = max_scroll;
                            }
                            Some(i) if i + 1 >= self.entries.len() => {
                                self.scroll = self.scroll.saturating_sub(1);
                            }
                            Some(i) => {
                                self.selected_entry = Some(i + 1);
                                let entry_start = offsets.get(i + 1).copied().unwrap_or(0);
                                self.scroll = max_scroll
                                    .saturating_sub(entry_start)
                                    .min(max_scroll);
                            }
                        }
                        EventResult::Consumed(None)
                    }
                }
                // Ctrl+u / PageUp: half page up
                KeyEvent {
                    code: KeyCode::Char('u'),
                    modifiers: KeyModifiers::CONTROL,
                }
                | KeyEvent {
                    code: KeyCode::PageUp,
                    ..
                } => {
                    let half = (self.last_content_height / 2).max(1);
                    self.scroll = self.scroll.saturating_add(half).min(self.last_max_scroll);
                    self.selected_entry = None;
                    EventResult::Consumed(None)
                }
                // Ctrl+d / PageDown: half page down
                KeyEvent {
                    code: KeyCode::Char('d'),
                    modifiers: KeyModifiers::CONTROL,
                }
                | KeyEvent {
                    code: KeyCode::PageDown,
                    ..
                } => {
                    let half = (self.last_content_height / 2).max(1);
                    self.scroll = self.scroll.saturating_sub(half);
                    self.selected_entry = None;
                    EventResult::Consumed(None)
                }
                // Scroll to bottom
                KeyEvent {
                    code: KeyCode::Char('G'),
                    ..
                } => {
                    self.scroll = 0;
                    self.selected_entry = None;
                    EventResult::Consumed(None)
                }
                // Yank selected entry to clipboard
                KeyEvent {
                    code: KeyCode::Char('y'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    if let Some(idx) = self.selected_entry {
                        if let Some(text) = self.entry_text(idx) {
                            let reg = cx.editor.config().default_yank_register;
                            if let Err(e) = cx.editor.registers.write(reg, vec![text]) {
                                cx.editor.set_error(format!("Failed to yank: {e}"));
                            } else {
                                cx.editor.set_status("Yanked entry to clipboard");
                            }
                        }
                    } else {
                        cx.editor.set_status("Select an entry with j/k or Ctrl+p/n first");
                    }
                    EventResult::Consumed(None)
                }
                // Cancel agent
                KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                } => {
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
                    EventResult::Consumed(None)
                }
                // Clear queue
                KeyEvent {
                    code: KeyCode::Char('Q'),
                    ..
                } => {
                    let count = self.message_queue.len();
                    self.clear_queue();
                    if count > 0 {
                        cx.editor
                            .set_status(format!("Cleared {count} queued messages"));
                    }
                    EventResult::Consumed(None)
                }
                // Cycle thinking (Ctrl+T by default)
                key if *key == cx.editor.config().acp.cycle_thinking() => {
                    if let Some((config_id, next_value, prev_value)) =
                        self.cycle_config_option("thinking")
                    {
                        self.apply_config_option_cycle("thinking", next_value.clone());
                        let agent = cx.editor.acp_agents.iter().next().map(|(_, a)| a.clone());
                        if let Some(agent) = agent {
                            let category = "thinking".to_string();
                            cx.jobs.callback(async move {
                                let session_id = match agent.session_id().await {
                                    Some(id) => id,
                                    None => {
                                        let prev = prev_value.clone();
                                        return Ok(crate::job::Callback::EditorCompositor(
                                            Box::new(move |editor, compositor| {
                                                if let Some(panel) =
                                                    compositor.find_id::<AcpPanel>(ID)
                                                {
                                                    panel
                                                        .apply_config_option_cycle(&category, prev);
                                                }
                                                editor.set_error("No session to update thinking");
                                            }),
                                        ));
                                    }
                                };
                                match agent
                                    .set_session_config_option(
                                        session_id,
                                        config_id.clone(),
                                        next_value.clone(),
                                    )
                                    .await
                                {
                                    Ok(_) => Ok(crate::job::Callback::EditorCompositor(Box::new(
                                        |_, _| {},
                                    ))),
                                    Err(e) => Ok(crate::job::Callback::EditorCompositor(Box::new(
                                        move |editor, compositor| {
                                            if let Some(panel) = compositor.find_id::<AcpPanel>(ID)
                                            {
                                                panel.apply_config_option_cycle(
                                                    &category, prev_value,
                                                );
                                            }
                                            editor
                                                .set_error(format!("Failed to set thinking: {e}"));
                                        },
                                    ))),
                                }
                            });
                        }
                        cx.editor.set_status("Cycled thinking");
                    } else {
                        cx.editor.set_status("No thinking options from agent");
                    }
                    EventResult::Consumed(None)
                }
                // Cycle mode (S-tab by default)
                key if *key == cx.editor.config().acp.cycle_mode() => {
                    if let Some((config_id, next_value, prev_value)) =
                        self.cycle_config_option("mode")
                    {
                        self.apply_config_option_cycle("mode", next_value.clone());
                        let agent = cx.editor.acp_agents.iter().next().map(|(_, a)| a.clone());
                        if let Some(agent) = agent {
                            let category = "mode".to_string();
                            cx.jobs.callback(async move {
                                let session_id = match agent.session_id().await {
                                    Some(id) => id,
                                    None => {
                                        let prev = prev_value.clone();
                                        return Ok(crate::job::Callback::EditorCompositor(
                                            Box::new(move |editor, compositor| {
                                                if let Some(panel) =
                                                    compositor.find_id::<AcpPanel>(ID)
                                                {
                                                    panel
                                                        .apply_config_option_cycle(&category, prev);
                                                }
                                                editor.set_error("No session to update mode");
                                            }),
                                        ));
                                    }
                                };
                                match agent
                                    .set_session_config_option(
                                        session_id,
                                        config_id.clone(),
                                        next_value.clone(),
                                    )
                                    .await
                                {
                                    Ok(_) => Ok(crate::job::Callback::EditorCompositor(Box::new(
                                        |_, _| {},
                                    ))),
                                    Err(e) => Ok(crate::job::Callback::EditorCompositor(Box::new(
                                        move |editor, compositor| {
                                            if let Some(panel) = compositor.find_id::<AcpPanel>(ID)
                                            {
                                                panel.apply_config_option_cycle(
                                                    &category, prev_value,
                                                );
                                            }
                                            editor.set_error(format!("Failed to set mode: {e}"));
                                        },
                                    ))),
                                }
                            });
                        }
                        cx.editor.set_status("Cycled mode");
                    } else {
                        cx.editor.set_status("No mode options from agent");
                    }
                    EventResult::Consumed(None)
                }
                // Cycle model (Ctrl+M by default)
                key if *key == cx.editor.config().acp.cycle_model() => {
                    if let Some((config_id, next_value, prev_value)) =
                        self.cycle_config_option("model")
                    {
                        self.apply_config_option_cycle("model", next_value.clone());
                        let agent = cx.editor.acp_agents.iter().next().map(|(_, a)| a.clone());
                        if let Some(agent) = agent {
                            let category = "model".to_string();
                            cx.jobs.callback(async move {
                                let session_id = match agent.session_id().await {
                                    Some(id) => id,
                                    None => {
                                        let prev = prev_value.clone();
                                        return Ok(crate::job::Callback::EditorCompositor(
                                            Box::new(move |editor, compositor| {
                                                if let Some(panel) =
                                                    compositor.find_id::<AcpPanel>(ID)
                                                {
                                                    panel
                                                        .apply_config_option_cycle(&category, prev);
                                                }
                                                editor.set_error("No session to update model");
                                            }),
                                        ));
                                    }
                                };
                                match agent
                                    .set_session_config_option(
                                        session_id,
                                        config_id.clone(),
                                        next_value.clone(),
                                    )
                                    .await
                                {
                                    Ok(_) => Ok(crate::job::Callback::EditorCompositor(Box::new(
                                        |_, _| {},
                                    ))),
                                    Err(e) => Ok(crate::job::Callback::EditorCompositor(Box::new(
                                        move |editor, compositor| {
                                            if let Some(panel) = compositor.find_id::<AcpPanel>(ID)
                                            {
                                                panel.apply_config_option_cycle(
                                                    &category, prev_value,
                                                );
                                            }
                                            editor.set_error(format!("Failed to set model: {e}"));
                                        },
                                    ))),
                                }
                            });
                        }
                        cx.editor.set_status("Cycled model");
                    } else {
                        cx.editor.set_status("No model options from agent");
                    }
                    EventResult::Consumed(None)
                }
                _ => EventResult::Ignored(None),
            }
        }
    }

    fn cursor(&self, area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        if self.focused {
            let inner = Rect {
                x: area.x + 1,
                y: area.y,
                width: area.width.saturating_sub(1),
                height: area.height,
            };
            let error_rows = 1;
            let bar_y = inner.y + inner.height - 1 - error_rows;
            let input_y = bar_y.saturating_sub(1);
            if input_y >= inner.y {
                let input_area = Rect {
                    x: inner.x + 1,
                    y: input_y,
                    width: inner.width.saturating_sub(2),
                    height: 1,
                };
                let (pos, _) = self.prompt.cursor(input_area, ctx);
                let kind = ctx
                    .config()
                    .cursor_shape
                    .from_mode(if self.chat_insert_mode {
                        Mode::Insert
                    } else {
                        Mode::Normal
                    });
                return (pos, kind);
            }
        }
        (None, CursorKind::Hidden)
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
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
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
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
