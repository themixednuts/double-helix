use crate::compositor::{Component, Context, Event, EventResult};
use crate::ui::{completers, Prompt, PromptEvent};
use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::input::{KeyCode, KeyEvent, KeyModifiers};
use helix_view::theme::Modifier;
use helix_view::Editor;
use tui::buffer::Buffer as Surface;
use tui::text::{Span, Spans, Text};
use tui::widgets::{Paragraph, Widget, Wrap};

pub const ID: &str = "acp-panel";
pub const PERMISSION_ID: &str = "acp-permission";

// ---------------------------------------------------------------------------
// Config picker item
// ---------------------------------------------------------------------------

/// An item in the config/model picker.
#[derive(Clone)]
pub struct ConfigPickerItem {
    pub category: String,
    pub config_id: String,
    pub value_id: String,
    pub display: String,
    pub is_current: bool,
    /// If true, this is a session mode (use set_mode). Otherwise config option (use set_config_option).
    pub is_mode: bool,
}

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
    /// Tool call with name, status, and optional detail.
    ToolCall {
        name: String,
        status: String,
        detail: Option<String>,
    },
    /// Plan with entries showing progress.
    Plan(Vec<PlanItem>),
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
    /// Whether the input area is visible/active.
    input_active: bool,
    /// Native Helix prompt for the input line (readline keybinds, scrolling, cursor).
    prompt: Prompt,
    agent_name: String,
    agent_version: String,
    agent_busy: bool,
    /// Queued messages to send after the current turn completes.
    message_queue: Vec<String>,
    /// Config options reported by the agent (model, thinking, etc.).
    config_options: Vec<helix_acp::types::ConfigOption>,
    /// Available session modes.
    session_modes: Vec<helix_acp::types::SessionMode>,
    /// Currently active mode id.
    current_mode_id: Option<String>,
}

impl AcpPanel {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            scroll: 0,
            focused: true,
            input_active: false,
            prompt: Prompt::new(
                "".into(),
                None,
                completers::none,
                |_cx: &mut Context, _s: &str, _e: PromptEvent| {},
            ),
            agent_name: String::from("No agent"),
            agent_version: String::new(),
            agent_busy: false,
            message_queue: Vec::new(),
            config_options: Vec::new(),
            session_modes: Vec::new(),
            current_mode_id: None,
        }
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if !focused {
            self.input_active = false;
        }
    }

    pub fn toggle_focus(&mut self) {
        self.set_focused(!self.focused);
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

    /// Returns the display name of the current session mode.
    pub fn current_mode_name(&self) -> Option<&str> {
        let mode_id = self.current_mode_id.as_deref()?;
        self.session_modes
            .iter()
            .find(|m| m.id == mode_id)
            .map(|m| m.name.as_str())
    }

    pub fn config_options(&self) -> &[helix_acp::types::ConfigOption] {
        &self.config_options
    }

    pub fn session_modes(&self) -> &[helix_acp::types::SessionMode] {
        &self.session_modes
    }

    pub fn push_entry(&mut self, entry: ChatEntry) {
        self.entries.push(entry);
        self.scroll = 0;
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

    /// Update or insert a tool call entry.
    pub fn update_tool_call(&mut self, name: &str, status: &str, detail: Option<&str>) {
        for entry in self.entries.iter_mut().rev() {
            if let ChatEntry::ToolCall {
                name: ref existing_name,
                status: ref mut existing_status,
                detail: ref mut existing_detail,
            } = entry
            {
                if existing_name == name {
                    *existing_status = status.to_string();
                    if let Some(d) = detail {
                        *existing_detail = Some(d.to_string());
                    }
                    return;
                }
            }
        }
        self.entries.push(ChatEntry::ToolCall {
            name: name.to_string(),
            status: status.to_string(),
            detail: detail.map(|s| s.to_string()),
        });
        self.scroll = 0;
    }

    pub fn update_plan(&mut self, items: Vec<PlanItem>) {
        for entry in self.entries.iter_mut().rev() {
            if matches!(entry, ChatEntry::Plan(_)) {
                *entry = ChatEntry::Plan(items);
                return;
            }
        }
        self.entries.push(ChatEntry::Plan(items));
        self.scroll = 0;
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

        let agent_style = theme.get("ui.text.info");
        let user_label_style = theme
            .get("keyword")
            .add_modifier(Modifier::BOLD);
        let user_text_style = theme.get("ui.text");
        let tool_icon_style = theme.get("ui.text.inactive");
        let tool_name_style = theme.get("ui.text.focus");
        let tool_detail_style = theme.get("ui.text.inactive");
        let plan_done_style = theme.get("diff.plus");
        let plan_progress_style = theme.get("warning");
        let plan_pending_style = theme.get("ui.text.inactive");
        let plan_failed_style = theme.get("error");
        let separator_style = theme.get("ui.statusline.separator");
        let heading_style = theme.get("markup.heading.1");
        let code_style = theme.get("markup.raw.inline");
        let bold_style = agent_style.add_modifier(Modifier::BOLD);
        let italic_style = agent_style.add_modifier(Modifier::ITALIC);
        let status_dim_style = theme.get("ui.text.inactive");

        for entry in &self.entries {
            match entry {
                ChatEntry::UserMessage(text) => {
                    // User label on its own line
                    lines.push(Spans::from(Span::styled(
                        "You".to_string(),
                        user_label_style,
                    )));
                    // User message text, word-wrapped by Paragraph
                    for line in text.lines() {
                        lines.push(Spans::from(Span::styled(
                            format!("  {line}"),
                            user_text_style,
                        )));
                    }
                    lines.push(Spans::default());
                }
                ChatEntry::AgentText(text) => {
                    render_markdown_lines(
                        text,
                        &mut lines,
                        agent_style,
                        heading_style,
                        code_style,
                        bold_style,
                        italic_style,
                        separator_style,
                    );
                    lines.push(Spans::default());
                }
                ChatEntry::ToolCall {
                    name,
                    status,
                    detail,
                } => {
                    let icon = match status.as_str() {
                        "running" => "\u{25ce}", // ◎
                        "completed" | "done" => "\u{25cf}", // ●
                        "failed" => "\u{2715}", // ✕
                        "cancelled" => "\u{2013}", // –
                        _ => "\u{25cb}", // ○
                    };
                    lines.push(Spans::from(vec![
                        Span::styled(format!(" {icon} "), tool_icon_style),
                        Span::styled(name.clone(), tool_name_style),
                    ]));
                    if let Some(ref d) = detail {
                        for line in d.lines().take(3) {
                            lines.push(Spans::from(Span::styled(
                                format!("     {line}"),
                                tool_detail_style,
                            )));
                        }
                    }
                }
                ChatEntry::Plan(items) => {
                    let done = items
                        .iter()
                        .filter(|i| i.status == PlanStatus::Completed)
                        .count();
                    let total = items.len();

                    let bar_width = (width as usize).saturating_sub(14).min(24);
                    let filled = if total > 0 {
                        (done * bar_width) / total
                    } else {
                        0
                    };
                    let empty = bar_width.saturating_sub(filled);
                    lines.push(Spans::from(vec![
                        Span::styled("Plan ", tool_name_style),
                        Span::styled(
                            format!(
                                "\u{2590}{}{}\u{258c} {done}/{total}",
                                "\u{2588}".repeat(filled),
                                "\u{2591}".repeat(empty)
                            ),
                            plan_progress_style,
                        ),
                    ]));

                    for item in items {
                        let (icon, style) = match item.status {
                            PlanStatus::Completed => (" \u{25cf} ", plan_done_style),
                            PlanStatus::InProgress => (" \u{25ce} ", plan_progress_style),
                            PlanStatus::Failed => (" \u{2715} ", plan_failed_style),
                            PlanStatus::Pending => (" \u{25cb} ", plan_pending_style),
                        };
                        lines.push(Spans::from(vec![
                            Span::styled(icon.to_string(), style),
                            Span::styled(item.content.clone(), style),
                        ]));
                    }
                    lines.push(Spans::default());
                }
                ChatEntry::Status(text) => {
                    lines.push(Spans::from(Span::styled(
                        format!(" {text}"),
                        status_dim_style,
                    )));
                    lines.push(Spans::default());
                }
            }
        }

        Text::from(lines)
    }

    /// Send a prompt to the first connected agent. Returns true if sent.
    fn send_prompt(text: String, cx: &mut Context) -> bool {
        let agent = cx
            .editor
            .acp_agents
            .iter()
            .next()
            .map(|(_, a)| a.clone());

        let Some(agent) = agent else {
            cx.editor
                .set_error("No ACP agents connected. Use :acp-connect first.");
            return false;
        };

        let prompt = vec![helix_acp::ContentBlock::from(text)];
        let callback = async move {
            let session_id = match agent.session_id().await {
                Some(id) => id,
                None => {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let session = agent.new_session(cwd).await?;
                    session.session_id
                }
            };
            let response = agent.prompt(session_id, prompt).await?;

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

            let callback: crate::job::Callback =
                crate::job::Callback::EditorCompositor(Box::new(
                    move |editor: &mut Editor, compositor| {
                        editor.set_status(msg);
                        if let Some(panel) = compositor.find_id::<AcpPanel>(ID) {
                            panel.set_busy(false);
                            // Auto-send next queued message
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
                ));
            Ok(callback)
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
            Err(e) => crate::job::Callback::EditorCompositor(Box::new(
                move |editor: &mut Editor, compositor| {
                    editor.set_error(format!("ACP error: {e}"));
                    if let Some(panel) = compositor.find_id::<AcpPanel>(ID) {
                        panel.set_busy(false);
                    }
                },
            )),
        };
        crate::job::dispatch_callback(cb).await;
    });
}

// ---------------------------------------------------------------------------
// Simple markdown line renderer
// ---------------------------------------------------------------------------

/// Render markdown-ish text into styled Spans lines.
/// Handles: headings (#), bold (**), italic (*), inline code (`), code blocks (```), horizontal rules (---).
fn render_markdown_lines<'a>(
    text: &str,
    lines: &mut Vec<Spans<'a>>,
    base_style: helix_view::graphics::Style,
    heading_style: helix_view::graphics::Style,
    code_style: helix_view::graphics::Style,
    bold_style: helix_view::graphics::Style,
    italic_style: helix_view::graphics::Style,
    separator_style: helix_view::graphics::Style,
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
                    code_style,
                )));
            } else {
                lines.push(Spans::from(Span::styled(
                    "────────".to_string(),
                    code_style,
                )));
            }
            continue;
        }

        if in_code_block {
            lines.push(Spans::from(Span::styled(
                format!("  {line}"),
                code_style,
            )));
            continue;
        }

        // Horizontal rules
        let trimmed = line.trim();
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && trimmed.len() >= 3
        {
            lines.push(Spans::from(Span::styled("───".to_string(), separator_style)));
            continue;
        }

        // Headings
        if line.starts_with("# ") {
            lines.push(Spans::from(Span::styled(
                line[2..].to_string(),
                heading_style,
            )));
            continue;
        }
        if line.starts_with("## ") {
            lines.push(Spans::from(Span::styled(
                line[3..].to_string(),
                heading_style,
            )));
            continue;
        }
        if line.starts_with("### ") {
            lines.push(Spans::from(Span::styled(
                line[4..].to_string(),
                heading_style,
            )));
            continue;
        }

        // Inline formatting: parse **bold**, *italic*, `code`
        let spans = parse_inline_markdown(line, base_style, bold_style, italic_style, code_style);
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
        let bar_y = inner.y + inner.height - 1;

        {
            let theme = &cx.editor.theme;
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
            let mut left_info = String::new();
            left_info.push(' ');
            if let Some(model) = self.current_model_name() {
                left_info.push_str(model);
            }
            if let Some(mode) = self.current_mode_name() {
                if !left_info.trim().is_empty() {
                    let sep = tui::symbols::line::VERTICAL;
                    left_info.push(' ');
                    left_info.push_str(sep);
                    left_info.push(' ');
                }
                left_info.push_str(mode);
            }
            if left_info.trim().is_empty() {
                left_info = format!(" {}", self.agent_name);
            }
            surface.set_stringn(
                inner.x,
                bar_y,
                &left_info,
                (inner.width / 2) as usize,
                bar_style,
            );
            let hint = if !self.focused {
                " Space A:focus "
            } else if self.input_active {
                " Esc:exit  Enter:send "
            } else if self.agent_busy {
                " i:chat  m:config  C-c:stop  Esc:back "
            } else {
                " i:chat  m:config  q:close  Esc:back "
            };
            let hint_x = inner.x + inner.width.saturating_sub(hint.len() as u16);
            surface.set_stringn(hint_x, bar_y, hint, hint.len(), bar_style);
        }

        // ── Input area: above the statusline when active (native Prompt: scrolling, readline keybinds) ──
        if self.input_active {
            let input_y = bar_y.saturating_sub(1);
            if input_y > inner.y {
                let input_area = Rect {
                    x: inner.x + 1,
                    y: input_y,
                    width: inner.width.saturating_sub(2),
                    height: 1,
                };
                let placeholder_style = cx.editor.theme.get("ui.text.inactive");
                self.prompt.render(input_area, surface, cx);
                if self.prompt.line().is_empty() {
                    surface.set_stringn(
                        input_area.x,
                        input_area.y,
                        "Ask anything...",
                        input_area.width.saturating_sub(2) as usize,
                        placeholder_style,
                    );
                }
            }
        }

        // ── Content area ─────────────────────────────────────
        let input_rows: u16 = if self.input_active { 1 } else { 0 };
        let content_area = Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: inner.height.saturating_sub(2 + input_rows),
        };

        if content_area.height == 0 || content_area.width == 0 {
            return;
        }

        let theme = &cx.editor.theme;
        // Empty state
        if self.entries.is_empty() {
            let empty_style = theme.get("ui.text.inactive");
            let center_y = content_area.y + content_area.height / 2;

            let msg = "Press i to start chatting";
            let mx = content_area.x
                + content_area
                    .width
                    .saturating_sub(msg.len() as u16)
                    / 2;
            if center_y >= content_area.y
                && center_y < content_area.y + content_area.height
            {
                surface.set_stringn(
                    mx,
                    center_y,
                    msg,
                    content_area.width as usize,
                    empty_style,
                );
            }

            // Keybind help below
            let help_lines = [
                ("i", "enter chat"),
                ("m", "model/config"),
                ("q", "close panel"),
                ("k/j", "scroll"),
                ("C-c", "cancel agent"),
            ];
            for (i, (key, desc)) in help_lines.iter().enumerate() {
                let hy = center_y + 2 + i as u16;
                if hy < content_area.y + content_area.height {
                    let key_style = theme.get("ui.text.focus");
                    let desc_style = theme.get("ui.text.inactive");
                    let kx = content_area.x + content_area.width / 2 - 8;
                    surface.set_stringn(kx, hy, key, 4, key_style);
                    surface.set_stringn(kx + 5, hy, desc, 20, desc_style);
                }
            }

            return;
        }

        // Render chat content
        let text = self.build_text(theme, content_area.width);
        let total_lines = text.height() as u16;

        let max_scroll = total_lines.saturating_sub(content_area.height);
        let scroll = self.scroll.min(max_scroll);
        let scroll_from_top = max_scroll.saturating_sub(scroll);

        let par = Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .scroll((scroll_from_top, 0));
        par.render(content_area, surface);

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

        if self.input_active {
            // Intercept Esc/Enter so we don't pop the panel; delegate everything else to Prompt (readline keybinds).
            return match key {
                KeyEvent {
                    code: KeyCode::Esc, ..
                } => {
                    self.input_active = false;
                    EventResult::Consumed(None)
                }
                KeyEvent {
                    code: KeyCode::Enter,
                    ..
                } => {
                    let text = self.prompt.line().clone();
                    if !text.is_empty() {
                        self.prompt.set_line(String::new(), cx.editor);
                        self.input_active = false;
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
            };
        } else {
            match key {
                // Unfocus panel (return to editor)
                KeyEvent {
                    code: KeyCode::Esc, ..
                } => {
                    self.set_focused(false);
                    EventResult::Consumed(None)
                }
                // Close panel
                KeyEvent {
                    code: KeyCode::Char('q'),
                    ..
                } => {
                    let callback: crate::compositor::Callback =
                        Box::new(|compositor, _cx| {
                            compositor.remove(ID);
                        });
                    EventResult::Consumed(Some(callback))
                }
                // Enter input (Prompt handles readline keybinds)
                KeyEvent {
                    code: KeyCode::Char('i'),
                    ..
                } => {
                    self.input_active = true;
                    EventResult::Consumed(None)
                }
                // Scroll up
                KeyEvent {
                    code: KeyCode::Char('k'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Up, ..
                } => {
                    self.scroll = self.scroll.saturating_add(1);
                    EventResult::Consumed(None)
                }
                // Scroll down
                KeyEvent {
                    code: KeyCode::Char('j'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Down,
                    ..
                } => {
                    self.scroll = self.scroll.saturating_sub(1);
                    EventResult::Consumed(None)
                }
                // Page up
                KeyEvent {
                    code: KeyCode::PageUp,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('u'),
                    modifiers: KeyModifiers::CONTROL,
                } => {
                    self.scroll = self.scroll.saturating_add(20);
                    EventResult::Consumed(None)
                }
                // Page down
                KeyEvent {
                    code: KeyCode::PageDown,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('d'),
                    modifiers: KeyModifiers::CONTROL,
                } => {
                    self.scroll = self.scroll.saturating_sub(20);
                    EventResult::Consumed(None)
                }
                // Scroll to bottom
                KeyEvent {
                    code: KeyCode::Char('G'),
                    ..
                } => {
                    self.scroll = 0;
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
                // Open config/model picker
                KeyEvent {
                    code: KeyCode::Char('m'),
                    ..
                } => {
                    let mut items: Vec<ConfigPickerItem> = Vec::new();

                    // Add config options (model, thinking, etc.)
                    for opt in &self.config_options {
                        let category = opt
                            .category
                            .as_deref()
                            .unwrap_or("config")
                            .to_string();
                        for v in &opt.options {
                            items.push(ConfigPickerItem {
                                category: category.clone(),
                                config_id: opt.id.clone(),
                                value_id: v.value.clone(),
                                display: format!(
                                    "{}: {}{}",
                                    opt.name,
                                    v.name,
                                    if v.value == opt.current_value {
                                        " *"
                                    } else {
                                        ""
                                    }
                                ),
                                is_current: v.value == opt.current_value,
                                is_mode: false,
                            });
                        }
                    }

                    // Add session modes
                    for mode in &self.session_modes {
                        let is_current = self
                            .current_mode_id
                            .as_deref()
                            .map_or(false, |id| id == mode.id);
                        items.push(ConfigPickerItem {
                            category: "mode".to_string(),
                            config_id: String::new(),
                            value_id: mode.id.clone(),
                            display: format!(
                                "Mode: {}{}",
                                mode.name,
                                if is_current { " *" } else { "" }
                            ),
                            is_current,
                            is_mode: true,
                        });
                    }

                    if items.is_empty() {
                        cx.editor
                            .set_status("No config options available from agent");
                        return EventResult::Consumed(None);
                    }

                    let columns = [
                        crate::ui::PickerColumn::new(
                            "category",
                            |item: &ConfigPickerItem, _: &()| {
                                item.category.as_str().into()
                            },
                        ),
                        crate::ui::PickerColumn::new(
                            "option",
                            |item: &ConfigPickerItem, _: &()| {
                                item.display.as_str().into()
                            },
                        ),
                    ];

                    let picker = crate::ui::Picker::new(
                        columns,
                        1, // primary column: option
                        items,
                        (),
                        move |cx, item: &ConfigPickerItem, _action| {
                            for (_id, agent) in cx.editor.acp_agents.iter() {
                                let agent = agent.clone();
                                if item.is_mode {
                                    let mode_id = item.value_id.clone();
                                    tokio::spawn(async move {
                                        if let Some(session_id) =
                                            agent.session_id().await
                                        {
                                            if let Err(e) = agent
                                                .set_session_mode(
                                                    session_id, mode_id,
                                                )
                                                .await
                                            {
                                                log::error!(
                                                    "Failed to set mode: {e}"
                                                );
                                            }
                                        }
                                    });
                                } else {
                                    let config_id = item.config_id.clone();
                                    let value_id = item.value_id.clone();
                                    tokio::spawn(async move {
                                        if let Some(session_id) =
                                            agent.session_id().await
                                        {
                                            if let Err(e) = agent
                                                .set_session_config_option(
                                                    session_id, config_id,
                                                    value_id,
                                                )
                                                .await
                                            {
                                                log::error!(
                                                    "Failed to set config: {e}"
                                                );
                                            }
                                        }
                                    });
                                }
                            }
                        },
                    );

                    let callback: crate::compositor::Callback =
                        Box::new(move |compositor, _cx| {
                            compositor.push(Box::new(
                                crate::ui::overlay::overlaid(picker),
                            ));
                        });
                    EventResult::Consumed(Some(callback))
                }
                _ => EventResult::Ignored(None),
            }
        }
    }

    fn cursor(&self, area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        if self.input_active {
            let inner = Rect {
                x: area.x + 1,
                y: area.y,
                width: area.width.saturating_sub(1),
                height: area.height,
            };
            let bar_y = inner.y + inner.height - 1;
            let input_y = bar_y.saturating_sub(1);
            if input_y > inner.y {
                let input_area = Rect {
                    x: inner.x + 1,
                    y: input_y,
                    width: inner.width.saturating_sub(2),
                    height: 1,
                };
                return self.prompt.cursor(input_area, ctx);
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
        let bg = cx.editor.theme.get("ui.popup");
        for py in popup_area.y..popup_area.y + popup_area.height {
            for px in popup_area.x..popup_area.x + popup_area.width {
                surface[(px, py)].set_style(bg).set_symbol(" ");
            }
        }

        // Border top
        let border_style = cx.editor.theme.get("ui.popup");
        let top_border = format!(
            "┌{}┐",
            "─".repeat(popup_width.saturating_sub(2) as usize)
        );
        surface.set_stringn(x, y, &top_border, popup_width as usize, border_style);

        // Title
        let title_style = cx
            .editor
            .theme
            .get("ui.text")
            .add_modifier(Modifier::BOLD);
        let title_line = format!("│ {:<width$}│", self.title, width = (popup_width - 4) as usize);
        surface.set_stringn(x, y + 1, &title_line, popup_width as usize, title_style);

        let mut row = y + 2;

        // Description
        if let Some(ref desc) = self.description {
            let desc_style = cx.editor.theme.get("ui.text.inactive");
            for line in desc.lines() {
                let padded = format!("│ {:<width$}│", line, width = (popup_width - 4) as usize);
                surface.set_stringn(x, row, &padded, popup_width as usize, desc_style);
                row += 1;
            }
        }

        // Separator
        let sep = format!(
            "├{}┤",
            "─".repeat(popup_width.saturating_sub(2) as usize)
        );
        surface.set_stringn(x, row, &sep, popup_width as usize, border_style);
        row += 1;

        // Options
        for (i, opt) in self.options.iter().enumerate() {
            let is_selected = i == self.selected;
            let marker = if is_selected { ">" } else { " " };
            let style = if is_selected {
                cx.editor.theme.get("ui.menu.selected")
            } else {
                cx.editor.theme.get("ui.text")
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
        let bottom_border = format!(
            "└{}┘",
            "─".repeat(popup_width.saturating_sub(2) as usize)
        );
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
                let callback: crate::compositor::Callback =
                    Box::new(|compositor, _cx| {
                        compositor.remove(PERMISSION_ID);
                    });
                EventResult::Consumed(Some(callback))
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.send_response(PermissionResponse::Dismissed);
                let callback: crate::compositor::Callback =
                    Box::new(|compositor, _cx| {
                        compositor.remove(PERMISSION_ID);
                    });
                EventResult::Consumed(Some(callback))
            }
            // Number shortcuts: 1-9 to select
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < self.options.len() {
                    self.send_response(PermissionResponse::Selected(
                        self.options[idx].id.clone(),
                    ));
                    let callback: crate::compositor::Callback =
                        Box::new(|compositor, _cx| {
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
