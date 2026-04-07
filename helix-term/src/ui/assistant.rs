use crate::component_traits;
use crate::compositor::{Component, Context, Event, EventResult, RenderContext};
use crate::ui::animation::{
    Animation, AnimationDirection, AnimationFillMode, AnimationIterationCount, AnimationSpec,
    AnimationTimingFunction,
};
use crate::ui::marquee::{schedule_redraw_at, Marquee};
use crate::widgets::{
    message_list, Message, MessageAccessoryAlign, MessageAlign, MessageCorners, MessageCursor,
    MessageListState, MessageStyle, Spinner,
};
use helix_core::unicode::width::UnicodeWidthStr;
use helix_core::Position;
use helix_view::content_region::ContentRegion;
use helix_view::document::Mode;
use helix_view::editor::Action;
use helix_view::graphics::{CursorKind, Rect, Style as GraphicsStyle};
use helix_view::input::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use helix_view::theme::{Modifier, Style};
use helix_view::traits::{Bounded, Focusable, Identified, Modal, Scrollable};
use helix_view::Editor;
use std::time::Duration;
use tui::buffer::Buffer as Surface;
use tui::text::{Span, Spans};

pub const ID: &str = "assistant-panel";
pub const PERMISSION_ID: &str = "assistant-permission";

// ---------------------------------------------------------------------------
// Chat entries
// ---------------------------------------------------------------------------

type ChatEntry = helix_view::model::AssistantEntry;
#[cfg(test)]
type ChatEntryKind = helix_view::model::AssistantEntryKind;

// ---------------------------------------------------------------------------
// Assistant Panel
// ---------------------------------------------------------------------------

pub struct AssistantPanel {
    focused: bool,
    /// Read-only chat/output area with component-owned scroll + viewport state.
    output: ContentRegion<Vec<ChatEntry>>,
    /// Editable input area backed by a component-owned document.
    input: helix_view::edit_region::EditRegion,
    /// Last assistant/backend error message, shown below the status line.
    panel_error: Option<String>,
    /// Marquee for long error text (scroll, hold, reset, repeat; pauses after inactivity).
    error_marquee: Marquee,
    /// Last input cursor screen position (set during render, read by cursor()).
    last_input_cursor: Option<(u16, u16)>,
    /// Model panel ID, set on first sync.
    model_panel_id: Option<helix_view::model::PanelId>,
    /// Latest chat-thread layout information for future chat-entry navigation.
    chat_layout: MessageListState,
    /// Shared spinner primitive for lightweight running-status animation.
    spinner: Spinner,
    /// Selection animation that replays when message focus moves back onto a row.
    message_focus_animation: Animation,
}

#[derive(Clone, Copy)]
struct MessageNavigationState {
    selected: Option<usize>,
    scroll: usize,
}

impl Default for AssistantPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl AssistantPanel {
    fn assistant_model(editor: &Editor) -> helix_view::model::AssistantModel {
        editor.assistant_model(false)
    }

    fn assistant_model_with_focus(
        editor: &Editor,
        focused: bool,
    ) -> helix_view::model::AssistantModel {
        editor.assistant_model(focused)
    }

    fn sync_from_assistant(&mut self, editor: &mut Editor) {
        let model = Self::assistant_model(editor);
        if model.active_thread.is_none() {
            self.output.set_content(Vec::new());
            self.output.scroll_to(0);
            return;
        }

        self.sync_input_from_assistant(editor, &model.input);
        self.output.set_content(model.entries);
        self.output.scroll_to(model.content_scroll);
    }

    fn entry_id_at(
        &self,
        _editor: &Editor,
        index: usize,
    ) -> Option<helix_view::assistant::thread::EntryId> {
        self.output.content().get(index).map(|entry| entry.id)
    }

    fn apply(editor: &mut Editor, action: helix_view::assistant::Action) {
        let effects = editor.assistant_act(action);
        Self::apply_assistant_effects(editor, effects);
    }

    fn set_focus(&mut self, editor: &mut Editor, focus: helix_view::assistant::thread::Focus) {
        if Self::assistant_model(editor).active_thread.is_none() {
            return;
        }
        if let Ok(effects) = editor.set_active_assistant_focus(focus) {
            Self::apply_assistant_effects(editor, effects);
        }
        if focus == helix_view::assistant::thread::Focus::Messages {
            self.restart_message_focus_animation(editor);
        } else {
            self.message_focus_animation.stop();
        }
    }

    fn selected_index_in(&self, model: &helix_view::model::AssistantModel) -> Option<usize> {
        let selected = model.selected_entry_id()?;
        self.output
            .content()
            .iter()
            .position(|entry| entry.id == selected)
    }

    fn selected_index(&self, editor: &Editor) -> Option<usize> {
        let model = Self::assistant_model(editor);
        self.selected_index_in(&model)
    }

    fn set_selected_entry(
        &mut self,
        editor: &mut Editor,
        entry: Option<helix_view::assistant::thread::EntryId>,
        animate: bool,
    ) -> Option<helix_view::assistant::thread::EntryId> {
        let model = Self::assistant_model(editor);
        if model.active_thread.is_none() {
            return None;
        }
        let previous = model.selected_entry_id();
        if let Ok(effects) = editor.select_active_assistant_entry(entry) {
            Self::apply_assistant_effects(editor, effects);
        }

        if entry.is_some() {
            self.set_focus(editor, helix_view::assistant::thread::Focus::Messages);
            if animate && previous != entry {
                self.restart_message_focus_animation(editor);
            }
        } else {
            self.message_focus_animation.stop();
        }

        entry
    }

    fn set_content_scroll(&mut self, editor: &mut Editor, content_scroll: usize) {
        let Some(thread) = Self::assistant_model(editor).active_thread else {
            return;
        };
        Self::apply(
            editor,
            helix_view::assistant::Action::SetContentScroll {
                thread,
                content_scroll,
            },
        );
        self.output.scroll_to(content_scroll);
    }

    fn set_folded(
        &mut self,
        editor: &mut Editor,
        entry: helix_view::assistant::thread::EntryId,
        folded: bool,
    ) {
        let Some(thread) = Self::assistant_model(editor).active_thread else {
            return;
        };
        Self::apply(
            editor,
            helix_view::assistant::Action::SetFolded {
                thread,
                entry,
                folded,
            },
        );
    }

    fn navigation_state(&self, editor: &Editor) -> MessageNavigationState {
        let model = Self::assistant_model(editor);
        MessageNavigationState {
            selected: self.selected_index_in(&model),
            scroll: model.content_scroll(),
        }
    }

    fn navigation_cursor(&self, editor: &Editor) -> MessageCursor {
        let navigation = self.navigation_state(editor);
        MessageCursor::new(navigation.selected, navigation.scroll)
    }

    fn cycle_thread(&mut self, editor: &mut Editor, delta: isize) -> bool {
        let effects = match editor.cycle_active_assistant_thread(delta) {
            Ok(effects) => effects,
            Err(_) => return false,
        };
        Self::apply_assistant_effects(editor, effects);
        self.message_focus_animation.stop();
        true
    }

    fn accent_style(style: Style) -> Style {
        let mut accent = Style::default();
        if let Some(fg) = style.fg {
            accent = accent.fg(fg);
        }
        if let Some(bg) = style.bg {
            accent = accent.bg(bg);
        }
        if let Some(underline_color) = style.underline_color {
            accent = accent.underline_color(underline_color);
        }
        if let Some(underline_style) = style.underline_style {
            accent = accent.underline_style(underline_style);
        }
        accent
    }

    fn header_item_style(
        theme: &helix_view::Theme,
        default: Style,
        tone: helix_view::model::AssistantHeaderTone,
    ) -> Style {
        match tone {
            helix_view::model::AssistantHeaderTone::Default => default,
            helix_view::model::AssistantHeaderTone::Active => theme.get("ui.menu.selected"),
            helix_view::model::AssistantHeaderTone::Warning => theme.get("warning"),
        }
    }

    fn entry_tone_style(
        theme: &helix_view::Theme,
        tone: helix_view::model::AssistantEntryTone,
    ) -> Style {
        match tone {
            helix_view::model::AssistantEntryTone::Default => theme.get("ui.text"),
            helix_view::model::AssistantEntryTone::Inactive => theme.get("ui.text.inactive"),
            helix_view::model::AssistantEntryTone::Focus => theme.get("ui.text.focus"),
            helix_view::model::AssistantEntryTone::Warning => theme.get("warning"),
            helix_view::model::AssistantEntryTone::Success => theme.get("diff.plus"),
            helix_view::model::AssistantEntryTone::Error => theme.get("error"),
        }
    }

    fn bubble_message_align(side: helix_view::model::AssistantBubbleSide) -> MessageAlign {
        match side {
            helix_view::model::AssistantBubbleSide::Left => MessageAlign::Left,
            helix_view::model::AssistantBubbleSide::Right => MessageAlign::Right,
        }
    }

    fn bubble_accessory_align(
        side: helix_view::model::AssistantBubbleSide,
    ) -> MessageAccessoryAlign {
        match side {
            helix_view::model::AssistantBubbleSide::Left => MessageAccessoryAlign::Left,
            helix_view::model::AssistantBubbleSide::Right => MessageAccessoryAlign::Right,
        }
    }

    fn plain_accessory_align() -> MessageAccessoryAlign {
        MessageAccessoryAlign::Right
    }

    pub fn new() -> Self {
        Self {
            focused: true,
            output: ContentRegion::default(),
            input: helix_view::edit_region::EditRegion::default(),
            panel_error: None,
            error_marquee: Marquee::new(),
            last_input_cursor: None,
            model_panel_id: None,
            chat_layout: MessageListState::default(),
            spinner: Spinner::default(),
            message_focus_animation: Animation::new({
                let mut spec = AnimationSpec::new(Duration::from_millis(220));
                spec.timing_function = AnimationTimingFunction::EaseOut;
                spec.iteration_count = AnimationIterationCount::Count(1);
                spec.direction = AnimationDirection::Normal;
                spec.fill_mode = AnimationFillMode::Forwards;
                spec.frame_interval = Duration::from_millis(16);
                spec
            }),
        }
    }

    pub fn model_panel_id(&self) -> Option<helix_view::model::PanelId> {
        self.model_panel_id
    }

    /// Set or clear the panel error message shown below the status line.
    pub fn set_panel_error(&mut self, msg: Option<String>) {
        self.panel_error = msg.clone();
        self.error_marquee
            .set_text(msg.map(|s| format!("Assistant: {}", s)));
    }

    pub fn activate_input(&mut self, editor: &mut Editor) {
        self.set_focus(editor, helix_view::assistant::thread::Focus::Input);
        self.message_focus_animation.stop();
        self.set_focused(true);
    }

    pub fn focus_messages(&mut self, editor: &mut Editor) {
        if self.input.mode() == Mode::Insert {
            self.input.exit_insert_mode();
        }
        self.set_focused(true);
        self.set_focus(editor, helix_view::assistant::thread::Focus::Messages);
    }

    fn focus_messages_without_animation(&mut self, editor: &mut Editor) {
        if self.input.mode() == Mode::Insert {
            self.input.exit_insert_mode();
        }
        self.set_focused(true);
        self.set_focus(editor, helix_view::assistant::thread::Focus::Messages);
    }

    pub fn focus_input_region(&mut self, editor: &mut Editor) {
        self.set_focused(true);
        self.set_focus(editor, helix_view::assistant::thread::Focus::Input);
        self.message_focus_animation.stop();
    }

    fn restart_message_focus_animation(&mut self, editor: &Editor) {
        let model = Self::assistant_model(editor);
        if model.focus() == helix_view::assistant::thread::Focus::Messages
            && model.selected_entry_id().is_some()
        {
            self.message_focus_animation.restart();
        }
    }

    fn current_message_accent(
        &self,
        editor: &Editor,
        theme: &helix_view::Theme,
    ) -> Option<(GraphicsStyle, f32)> {
        let model = Self::assistant_model(editor);
        if model.focus() != helix_view::assistant::thread::Focus::Messages
            || model.selected_entry_id().is_none()
        {
            return None;
        }

        let sample = self.message_focus_animation.sample();
        let accent = theme
            .try_get("ui.accent")
            .or_else(|| theme.try_get("ui.cursor.primary"))
            .unwrap_or_else(|| theme.get("ui.menu.selected"));
        let color = accent.bg.or(accent.fg).or(accent.underline_color)?;
        Some((GraphicsStyle::default().fg(color), sample.progress))
    }

    fn action_hints(
        theme: &helix_view::Theme,
        align: MessageAccessoryAlign,
    ) -> (Vec<Spans<'static>>, MessageAccessoryAlign) {
        let key_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
        let text_style = theme.get("ui.text.inactive");
        (
            vec![Spans::from(vec![
                Span::styled(" y", key_style),
                Span::styled(" yank  ", text_style),
                Span::styled("enter", key_style),
                Span::styled(" open  ", text_style),
                Span::styled("tab", key_style),
                Span::styled(" fold  ", text_style),
                Span::styled("t", key_style),
                Span::styled(" follow", text_style),
            ])],
            align,
        )
    }

    fn expanded_details(
        &self,
        entry: &ChatEntry,
        theme: &helix_view::Theme,
        agent_name: &str,
    ) -> Vec<Spans<'static>> {
        let heading = theme.get("ui.text.info").add_modifier(Modifier::BOLD);
        let text = theme.get("ui.text.inactive");
        let details = entry.details(agent_name);
        let mut lines = vec![Spans::from(Span::styled(
            format!(" {}", details.heading),
            heading,
        ))];
        if let Some(body) = details.body {
            lines.push(Spans::from(Span::styled(body, text)));
        }
        for line in details.lines {
            lines.push(Spans::from(Span::styled(
                format!(" {}: {}", line.label, line.value),
                text,
            )));
        }
        lines
    }

    fn decorate_selected_plain_message(
        &self,
        mut message: Message<'static>,
        selected: bool,
        entry: &ChatEntry,
        theme: &helix_view::Theme,
        agent_name: &str,
    ) -> Message<'static> {
        if selected {
            message = message.with_details(self.expanded_details(entry, theme, agent_name));
            message = Self::decorate_selected_message(
                message,
                true,
                theme,
                Self::plain_accessory_align(),
            );
        }
        message
    }

    fn decorate_selected_message(
        mut message: Message<'static>,
        selected: bool,
        theme: &helix_view::Theme,
        align: MessageAccessoryAlign,
    ) -> Message<'static> {
        if selected {
            let (lines, align) = Self::action_hints(theme, align);
            message = message.with_selected_accessory(lines, align);
        }
        message
    }

    fn yank_selected_message(&mut self, editor: &mut Editor) -> bool {
        let Some(text) = self.selected_entry_ref(editor).map(ChatEntry::plain_text) else {
            return false;
        };

        match editor.registers.write('"', vec![text]) {
            Ok(()) => {
                editor.set_status("Assistant entry yanked");
                true
            }
            Err(err) => {
                editor.set_error(err.to_string());
                false
            }
        }
    }

    fn toggle_selected_message_fold(&mut self, editor: &mut Editor) -> bool {
        let model = Self::assistant_model(editor);
        let Some(entry) = model.selected_entry_id() else {
            return false;
        };
        if self.selected_entry_ref(editor).is_none() {
            return false;
        }
        self.set_folded(editor, entry, !model.is_folded(entry));
        true
    }

    fn collapse_preview(text: &str, width: usize) -> String {
        let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let max = width.max(4);
        if compact.chars().count() <= max {
            return compact;
        }

        let mut preview = String::new();
        for ch in compact.chars().take(max.saturating_sub(1)) {
            preview.push(ch);
        }
        preview.push('…');
        preview
    }

    fn apply_assistant_effects(
        editor: &mut Editor,
        effects: Vec<helix_view::assistant::effect::Effect>,
    ) {
        editor.apply_assistant_effects(effects);
    }

    fn set_draft(&mut self, editor: &mut Editor, text: String) {
        if let Some(effects) = editor.set_active_assistant_draft_if_changed(text) {
            Self::apply_assistant_effects(editor, effects);
        }
    }

    fn sync_draft_to_assistant(&mut self, editor: &mut Editor) {
        let text = self
            .input
            .document(editor)
            .map(|doc| doc.text().to_string())
            .unwrap_or_default();
        self.set_draft(editor, text);
    }

    fn sync_input_from_assistant(&mut self, editor: &mut Editor, draft: &str) {
        let Some(doc) = self.input.document_mut(editor) else {
            return;
        };

        if doc.text().to_string() == draft {
            return;
        }

        let len = doc.text().len_chars();
        let replacement = if draft.is_empty() {
            None
        } else {
            Some(helix_core::Tendril::from(draft))
        };
        let transaction =
            helix_core::Transaction::change(doc.text(), [(0, len, replacement)].into_iter());
        doc.apply(&transaction, self.input.view_id());
        doc.set_selection(
            self.input.view_id(),
            helix_core::Selection::point(doc.text().len_chars()),
        );
    }

    /// Sync panel state to the shared `Model.panels` entry. Called during render
    /// so any frontend can read the assistant panel's current state.
    fn sync_to_model(&mut self, editor: &mut Editor) {
        use helix_view::model::{AssistantModel, PanelSide, PanelSize};

        // Lazily insert a model panel on first sync, or reclaim an orphaned one.
        let panel_id = match self.model_panel_id {
            Some(id) if editor.model.panels.contains_key(id) => id,
            _ => {
                // Check for an orphaned AssistantModel panel (e.g., from a replaced component).
                let existing = editor
                    .model
                    .panels
                    .iter()
                    .find(|(_, p)| p.content.as_any().is::<AssistantModel>())
                    .map(|(id, _)| id);
                let id = existing.unwrap_or_else(|| {
                    editor.model.insert_panel(
                        "Agent",
                        Box::new(AssistantModel::default()),
                        PanelSide::Right,
                        PanelSize::Percent(35),
                    )
                });
                self.model_panel_id = Some(id);
                id
            }
        };

        let mut snapshot = Self::assistant_model_with_focus(editor, self.focused);

        let Some(model) = editor.model.panel_model_mut::<AssistantModel>(panel_id) else {
            return;
        };

        snapshot.viewport_scroll = self
            .output
            .max_scroll()
            .saturating_sub(self.output.scroll()) as u16;
        snapshot.viewport_max_scroll = self.output.max_scroll() as u16;
        snapshot.focused = self.focused;
        snapshot.insert_mode = self.focused && self.input.mode() == Mode::Insert;
        snapshot.error = self.panel_error.clone();
        snapshot.input_cursor = 0; // cursor position tracked by engine, not model

        *model = snapshot;
    }

    /// Remove this panel's entry from the shared Model. Call before dropping.
    pub fn remove_model_panel(&self, editor: &mut Editor) {
        if let Some(id) = self.model_panel_id {
            editor.model.remove_panel(id);
        }
    }

    /// Build renderable blocks from chat entries.
    fn build_blocks<'a>(
        &self,
        editor: &Editor,
        theme: &helix_view::Theme,
        width: u16,
        corners: MessageCorners,
    ) -> Vec<Message<'a>> {
        let border_style = Self::accent_style(theme.get("ui.window"));
        let agent_style = theme.get("ui.text.info");
        let user_label_style = theme.get("keyword").add_modifier(Modifier::BOLD);
        let agent_label_style = theme
            .try_get("ui.assistant.agent.label")
            .unwrap_or_else(|| theme.get("ui.text.info").add_modifier(Modifier::BOLD));
        let user_text_style = theme.get("ui.text");
        let separator_style = theme.get("ui.statusline.separator");
        let heading_style = theme.get("markup.heading.1");
        let code_style = theme.get("markup.raw.inline");
        let bold_style = agent_style.add_modifier(Modifier::BOLD);
        let italic_style = agent_style.add_modifier(Modifier::ITALIC);
        let active_accent = self.current_message_accent(editor, theme);
        let model = Self::assistant_model(editor);
        let agent_name = model.agent_name.as_str();

        // Flex width: min 60%, max 90% of panel — but never panic on tiny sizes.
        let max_bubble = ((width as u32 * 90 / 100) as u16).min(width).max(4);
        let min_bubble = ((width as u32 * 60 / 100) as u16).max(20).min(max_bubble);

        let mut blocks = Vec::new();
        let selected = model.selected_entry_id();

        for entry in self.output.content().iter() {
            let entry_id = Some(entry.id);
            let display = entry.display(agent_name);
            let message_accent = if entry_id == selected {
                active_accent
            } else {
                None
            };
            let selected = entry_id == selected;
            let collapsed = model.is_folded(entry.id);
            match &display {
                helix_view::model::AssistantEntryDisplay::Bubble(display) => {
                    let bubble_w =
                        fit_bubble_width(&display.text, min_bubble as usize, max_bubble as usize)
                            as u16;
                    let inner_w = bubble_w.saturating_sub(4) as usize;
                    let (label_style, content_lines) = match display.format {
                        helix_view::model::AssistantTextFormat::Plain => {
                            let wrapped = if collapsed {
                                vec![Self::collapse_preview(&display.text, inner_w)]
                            } else {
                                wrap_text(&display.text, inner_w)
                            };
                            let lines = wrapped
                                .iter()
                                .map(|wl| Spans::from(Span::styled(wl.clone(), user_text_style)))
                                .collect();
                            (user_label_style, lines)
                        }
                        helix_view::model::AssistantTextFormat::Markdown => {
                            let mut md_lines: Vec<Spans> = Vec::new();
                            if collapsed {
                                md_lines.push(Spans::from(Span::styled(
                                    Self::collapse_preview(&display.text, inner_w),
                                    agent_style,
                                )));
                            } else {
                                render_markdown_lines(
                                    &display.text,
                                    &mut md_lines,
                                    agent_style,
                                    &MarkdownLineStyles {
                                        heading: heading_style,
                                        code: code_style,
                                        bold: bold_style,
                                        italic: italic_style,
                                        separator: separator_style,
                                    },
                                );
                            }

                            let mut lines: Vec<Spans> = Vec::new();
                            for md_line in md_lines {
                                let line_text: String =
                                    md_line.0.iter().map(|s| s.content.as_ref()).collect();
                                if line_text.len() <= inner_w {
                                    lines.push(md_line);
                                } else {
                                    let wrapped = wrap_text(&line_text, inner_w);
                                    let style =
                                        md_line.0.first().map(|s| s.style).unwrap_or(agent_style);
                                    for wl in &wrapped {
                                        lines.push(Spans::from(Span::styled(wl.clone(), style)));
                                    }
                                }
                            }
                            (agent_label_style, lines)
                        }
                    };
                    let mut message = Message::bubble(
                        Some((display.meta.heading.clone(), label_style)),
                        content_lines,
                        bubble_w,
                        Self::bubble_message_align(display.meta.side),
                        MessageStyle {
                            border: border_style,
                            corners,
                            accent: message_accent.map(|(style, _)| style),
                            accent_progress: message_accent
                                .map(|(_, progress)| progress)
                                .unwrap_or(0.0),
                        },
                    );
                    message = Self::decorate_selected_message(
                        message,
                        selected,
                        theme,
                        Self::bubble_accessory_align(display.meta.side),
                    );
                    blocks.push(message);
                }
                helix_view::model::AssistantEntryDisplay::Plain(row) => {
                    let icon = if row.animate_leading {
                        format!(" {} ", self.spinner.frame())
                    } else {
                        row.leading.clone()
                    };
                    let lines = if row.leading.is_empty() {
                        vec![Spans::from(Span::styled(
                            row.body.clone(),
                            Self::entry_tone_style(theme, row.body_tone),
                        ))]
                    } else {
                        vec![Spans::from(vec![
                            Span::styled(icon, Self::entry_tone_style(theme, row.leading_tone)),
                            Span::styled(
                                row.body.clone(),
                                Self::entry_tone_style(theme, row.body_tone),
                            ),
                        ])]
                    };
                    let mut message = Message::plain(lines);
                    if let Some(accessory) = &row.accessory {
                        message = message.with_accessory(
                            vec![Spans::from(Span::styled(
                                accessory.clone(),
                                Self::entry_tone_style(theme, row.accessory_tone),
                            ))],
                            Self::plain_accessory_align(),
                        );
                    }
                    message = self.decorate_selected_plain_message(
                        message,
                        selected,
                        entry,
                        theme,
                        &agent_name,
                    );
                    blocks.push(message);
                }
            }
        }

        blocks
    }

    /// Render chat blocks directly to the surface with proper scroll.
    fn render_content(
        &mut self,
        editor: &Editor,
        blocks: &[Message],
        area: Rect,
        surface: &mut Surface,
    ) {
        if area.height == 0 || area.width == 0 {
            self.chat_layout = MessageListState::default();
            return;
        }

        let cursor = self.navigation_cursor(editor);
        self.chat_layout = message_list(surface, area, blocks, cursor.scroll(), cursor.selected());
        let mut cursor = cursor;
        cursor.clamp_selection(&self.chat_layout);
        self.output.scroll_to(cursor.scroll());
        self.output
            .set_content_height(self.chat_layout.total_height);
    }

    pub fn selected_message(&self, editor: &Editor) -> Option<usize> {
        self.selected_index(editor)
    }

    pub fn clear_message_selection(&mut self, editor: &mut Editor) {
        self.set_selected_entry(editor, None, false);
        self.set_focus(editor, helix_view::assistant::thread::Focus::Input);
        self.message_focus_animation.stop();
    }

    pub fn select_message(&mut self, editor: &mut Editor, index: Option<usize>) -> Option<usize> {
        let viewport_height = self.output.area().height as usize;
        let entry = index.and_then(|index| self.entry_id_at(editor, index));
        let selected = self.set_selected_entry(editor, entry, true);
        if let Some(index) = selected.and_then(|entry| {
            self.output
                .content()
                .iter()
                .position(|item| item.id == entry)
        }) {
            let cursor = MessageCursor::new(Some(index), self.navigation_state(editor).scroll);
            let scroll = self
                .chat_layout
                .scroll_to_item(index, cursor.scroll(), viewport_height);
            self.set_content_scroll(editor, scroll);
        }
        self.selected_index(editor)
    }

    pub fn select_prev_message(&mut self, editor: &mut Editor) -> Option<usize> {
        let mut cursor = self.navigation_cursor(editor);
        let selected = cursor.move_prev(&self.chat_layout, self.output.area().height as usize);
        if selected.is_some() {
            self.set_content_scroll(editor, cursor.scroll());
            self.set_selected_entry(
                editor,
                selected.and_then(|index| self.entry_id_at(editor, index)),
                true,
            );
        }
        selected
    }

    pub fn select_next_message(&mut self, editor: &mut Editor) -> Option<usize> {
        let mut cursor = self.navigation_cursor(editor);
        let selected = cursor.move_next(&self.chat_layout, self.output.area().height as usize);
        if selected.is_some() {
            self.set_content_scroll(editor, cursor.scroll());
            self.set_selected_entry(
                editor,
                selected.and_then(|index| self.entry_id_at(editor, index)),
                true,
            );
        }
        selected
    }

    pub fn select_message_at_offset(
        &mut self,
        editor: &mut Editor,
        offset: usize,
    ) -> Option<usize> {
        self.select_message(editor, self.chat_layout.item_at_offset(offset))
    }

    pub fn select_first_message(&mut self, editor: &mut Editor) -> Option<usize> {
        self.select_message(
            editor,
            if self.chat_layout.is_empty() {
                None
            } else {
                Some(0)
            },
        )
    }

    pub fn select_last_message(&mut self, editor: &mut Editor) -> Option<usize> {
        self.select_message(editor, self.chat_layout.len().checked_sub(1))
    }

    pub fn select_prev_message_page(&mut self, editor: &mut Editor) -> Option<usize> {
        let mut cursor = self.navigation_cursor(editor);
        let selected = cursor.move_prev_page(&self.chat_layout, self.output.area().height as usize);
        if selected.is_some() {
            self.set_content_scroll(editor, cursor.scroll());
            self.set_selected_entry(
                editor,
                selected.and_then(|index| self.entry_id_at(editor, index)),
                true,
            );
        }
        selected
    }

    pub fn select_next_message_page(&mut self, editor: &mut Editor) -> Option<usize> {
        let mut cursor = self.navigation_cursor(editor);
        let selected = cursor.move_next_page(&self.chat_layout, self.output.area().height as usize);
        if selected.is_some() {
            self.set_content_scroll(editor, cursor.scroll());
            self.set_selected_entry(
                editor,
                selected.and_then(|index| self.entry_id_at(editor, index)),
                true,
            );
        }
        selected
    }

    pub fn selected_entry_ref(&self, editor: &Editor) -> Option<&ChatEntry> {
        let index = self.selected_index(editor)?;
        self.output.content().get(index)
    }

    pub fn selected_message_details(&self, editor: &Editor) -> Option<String> {
        let entry = Self::assistant_model(editor).selected_entry_id()?;
        editor.assistant_entry_details(entry)
    }

    pub fn open_selected_message_details(&mut self, editor: &mut Editor, action: Action) -> bool {
        let model = Self::assistant_model(editor);
        let Some(entry) = model.selected_entry_id() else {
            log::warn!("[assistant_scratch] open requested without selected message");
            return false;
        };
        let index = self.selected_index(editor).unwrap_or_default();
        let Some(details) = self.selected_message_details(editor) else {
            log::warn!(
                "[assistant_scratch] open requested for entry={:?} without details",
                entry
            );
            return false;
        };

        log::warn!(
            "[assistant_scratch] open entry={:?} index={} action={:?} existing_doc={:?} details_len={}",
            entry,
            index,
            action,
            model.opened_doc(entry),
            details.len()
        );

        let Some(thread) = model.active_thread else {
            return false;
        };
        let Some(effects) = editor.open_assistant_entry_scratch(thread, entry, action) else {
            return false;
        };
        Self::apply_assistant_effects(editor, effects);
        self.set_focused(false);
        self.message_focus_animation.stop();
        log::warn!(
            "[assistant_ui] released panel focus after opening scratch entry={:?} index={} focused={} focus={:?}",
            entry,
            index,
            self.focused,
            model.focus()
        );
        true
    }

    fn handle_mouse_event(&mut self, event: &MouseEvent, editor: &mut Editor) -> EventResult {
        let MouseEvent {
            kind,
            column: x,
            row: y,
            ..
        } = *event;

        let output_area = self.output.area();
        let input_area = self.input.area();
        let in_output = x >= output_area.left()
            && x < output_area.right()
            && y >= output_area.top()
            && y < output_area.bottom();
        let in_input = x >= input_area.left()
            && x < input_area.right()
            && y >= input_area.top()
            && y < input_area.bottom();

        match kind {
            MouseEventKind::Down(MouseButton::Left) if in_output => {
                let row_offset = y.saturating_sub(output_area.y) as usize;
                let offset = Scrollable::scroll(&self.output) + row_offset;
                let selected = self.chat_layout.item_at_offset(offset);
                let same_selection = selected == self.selected_index(editor);
                log::warn!(
                    "[assistant_ui] output click row_offset={} offset={} selected={:?} same_selection={}",
                    row_offset,
                    offset,
                    selected,
                    same_selection
                );
                if same_selection {
                    self.focus_messages_without_animation(editor);
                } else {
                    self.focus_messages(editor);
                }
                self.select_message(editor, selected);
                EventResult::Consumed(None)
            }
            MouseEventKind::Down(MouseButton::Left) if in_input => {
                self.activate_input(editor);
                if self.input.mode() != Mode::Insert {
                    self.input.enter_insert_mode("insert_mode".into());
                }
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }

    /// Send a prompt through the assistant store/backend flow. Returns true if sent.
    fn submit_prompt(text: String, cx: &mut Context) -> bool {
        let effects = match cx.editor.submit_active_assistant_prompt(text) {
            Ok(effects) => effects,
            Err(err) => {
                cx.editor
                    .set_error(format!("{err}. Use :assistant-connect first."));
                return false;
            }
        };
        Self::apply_assistant_effects(cx.editor, effects);
        true
    }

    /// Dispatch a key through the input region's own engine + modal keymaps.
    /// Dispatch a key through the engine. Returns `true` if consumed, `false` if unbound
    /// (should bubble up to the editor).
    fn dispatch_input_key(&mut self, key: KeyEvent, cx: &mut Context) -> bool {
        let Some(result) = self.input.dispatch_key(cx.editor, key) else {
            return true;
        };
        let handled = self.handle_engine_result(result, cx);
        if handled {
            self.sync_draft_to_assistant(cx.editor);
        }
        handled
    }

    /// Handle the result of an engine dispatch in the input region.
    /// Returns `true` if consumed, `false` if the key was unbound and should bubble.
    fn handle_engine_result(
        &mut self,
        result: helix_view::engine::EngineResult,
        cx: &mut Context,
    ) -> bool {
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
            if Self::assistant_model(cx.editor).agent_busy {
                self.sync_draft_to_assistant(cx.editor);
                cx.editor
                    .set_status("Assistant is busy; wait for the current turn or cancel it.");
                return;
            }

            if !Self::submit_prompt(text, cx) {
                self.sync_draft_to_assistant(cx.editor);
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

        let result = if category == "mode" {
            cx.editor.cycle_active_assistant_mode()
        } else {
            cx.editor.cycle_active_assistant_config(category)
        };

        match result {
            Ok(effects) => {
                Self::apply_assistant_effects(cx.editor, effects);
                cx.editor.set_status(format!("Cycled {category}"));
            }
            Err(err) => cx.editor.set_status(err.to_string()),
        }
        Some(EventResult::Consumed(None))
    }
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
    let max_w = max_w.max(4);
    let min_w = min_w.min(max_w);
    let inner_max = max_w.saturating_sub(4).max(1);
    let wrapped = wrap_text(text, inner_max);
    let longest = wrapped.iter().map(|l| l.len()).max().unwrap_or(0);
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

impl Focusable for AssistantPanel {
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

impl Bounded for AssistantPanel {
    fn area(&self) -> helix_view::graphics::Rect {
        self.output.area()
    }

    fn set_area(&mut self, area: helix_view::graphics::Rect) {
        self.output.set_area(area);
    }
}

impl Scrollable for AssistantPanel {
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

impl Component for AssistantPanel {
    fn sync(&mut self, editor: &mut Editor) {
        self.output.ensure_init(editor);
        self.input.ensure_init(editor);
        if self.focused {
            editor.focused_modal_input = self.input.input_state();
        }
        self.sync_from_assistant(editor);
        self.sync_to_model(editor);
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &RenderContext) {
        log::warn!(
            "[assistant_panel] render area=({},{} {}x{})",
            area.x,
            area.y,
            area.width,
            area.height,
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
        let model = Self::assistant_model(cx.editor);
        let plan = model.plan_section();
        let context_items = &model.context_items;
        let context_line = model.context_line();
        let status_items = model.status_items();
        let plan_rows = plan
            .as_ref()
            .map(|section| (section.rows.len() + 1).min(6) as u16)
            .unwrap_or(0);
        let context_rows = if context_items.is_empty() { 0 } else { 1 };

        // Vertical layout: [header | chat | plan | context pills | input | status | error]
        let v_areas = split_vertical(
            inner,
            &[
                Size::fixed(1),         // header
                Size::Fill,             // chat content
                Size::fixed(plan_rows), // plan
                Size::fixed(context_rows),
                Size::fixed(input_rows), // input
                Size::fixed(1),          // status bar
                Size::fixed(error_rows), // error line
            ],
        );
        let header_area = v_areas[0];
        let content_area_raw = v_areas[1];
        let plan_area = v_areas[2];
        let context_area = v_areas[3];
        let input_area_raw = v_areas[4];
        let bar_area = v_areas[5];
        let error_area = v_areas[6];

        // Inset content/plan/input by 1px on each side for padding.
        let content_area = Rect::new(
            content_area_raw.x + 1,
            content_area_raw.y,
            content_area_raw.width.saturating_sub(2),
            content_area_raw.height,
        );
        {
            let theme = cx.editor.assistant_theme();
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
            let header = model.header();
            let agent_busy = model.agent_busy;
            let dot_style = if agent_busy {
                theme.get("warning")
            } else {
                theme.get("hint")
            };
            surface.set_stringn(header_area.x + 1, header_area.y, "\u{25cf}", 1, dot_style);
            let trailing_width = header
                .trailing
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    UnicodeWidthStr::width(item.label.as_str()) + usize::from(index > 0) * 2
                })
                .sum::<usize>() as u16;
            if !header.trailing.is_empty() {
                let mut rx = header_area.x + header_area.width.saturating_sub(trailing_width);
                for (index, item) in header.trailing.iter().enumerate() {
                    if index > 0 {
                        surface.set_stringn(rx, header_area.y, "  ", 2, header_style);
                        rx += 2;
                    }
                    let style = Self::header_item_style(theme, header_style, item.tone);
                    let width = UnicodeWidthStr::width(item.label.as_str());
                    surface.set_stringn(rx, header_area.y, &item.label, width, style);
                    rx += width as u16;
                }
            }
            let mut x = header_area.x + 3;
            let right_edge = if header.trailing.is_empty() {
                header_area.x + header_area.width
            } else {
                header_area.x + header_area.width - trailing_width - 1
            };
            for item in &header.leading {
                if x >= right_edge {
                    break;
                }
                let style = Self::header_item_style(theme, header_style, item.tone);
                let width = UnicodeWidthStr::width(item.label.as_str()) as u16;
                let budget = right_edge.saturating_sub(x) as usize;
                surface.set_stringn(x, header_area.y, &item.label, budget, style);
                x = x.saturating_add(width.min(right_edge.saturating_sub(x)) + 1);
            }
            let bar_style = if self.focused {
                theme.get("ui.statusline")
            } else {
                theme.get("ui.statusline.inactive")
            };
            surface.set_style(bar_area, bar_style);
            // Bottom status: mode  model (gap between, no separator)
            let use_status_colors = cx.editor.config().color_modes;
            let theme = cx.editor.assistant_theme();
            let get_style = |assistant_scope: &str, statusline_fallback: &str| -> Style {
                if use_status_colors {
                    theme
                        .try_get(assistant_scope)
                        .unwrap_or_else(|| theme.get(statusline_fallback))
                } else {
                    bar_style
                }
            };
            let mut spans: Vec<Span> = Vec::new();
            for (index, item) in status_items.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled("  ".to_string(), bar_style));
                }
                let style = match item.kind {
                    helix_view::model::AssistantStatusItemKind::Mode => {
                        get_style("ui.assistant.mode", "ui.statusline.select")
                    }
                    helix_view::model::AssistantStatusItemKind::Model => {
                        get_style("ui.assistant.model", "ui.statusline.normal")
                    }
                    helix_view::model::AssistantStatusItemKind::Follow => theme.get("ui.text.info"),
                };
                spans.push(Span::styled(
                    format!(" {} ", item.label),
                    bar_style.patch(style),
                ));
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
            let error_style = cx.editor.assistant_theme().get("error");
            if let Some(when) = self.error_marquee.render(error_inset, surface, error_style) {
                schedule_redraw_at(cx.editor.runtime().work().clone(), when, cx.ingress.clone());
            }
        }

        // ── Plan area ──
        if plan_rows > 0 && plan_area.height > 0 {
            let theme = cx.editor.assistant_theme();
            let plan_done_style = theme.get("diff.plus");
            let plan_progress_style = theme.get("warning");
            let plan_pending_style = theme.get("ui.text.inactive");
            let plan_failed_style = theme.get("error");
            let tool_name_style = theme.get("ui.text.focus");
            let section = plan.as_ref().unwrap();
            let done = section.done;
            let total = section.total;
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
                    Span::styled(format!("{} ", section.title), tool_name_style),
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
            for (i, item) in section.rows.iter().take(5).enumerate() {
                let style = match item.tone {
                    helix_view::model::AssistantPlanTone::Completed => plan_done_style,
                    helix_view::model::AssistantPlanTone::InProgress => plan_progress_style,
                    helix_view::model::AssistantPlanTone::Failed => plan_failed_style,
                    helix_view::model::AssistantPlanTone::Pending => plan_pending_style,
                };
                let y = plan_inset.y + 1 + i as u16;
                if y < plan_inset.bottom() {
                    surface.set_spans(
                        plan_inset.x,
                        y,
                        &Spans::from(vec![
                            Span::styled(item.icon.to_string(), style),
                            Span::styled(item.content.clone(), style),
                        ]),
                        plan_inset.width,
                    );
                }
            }
        }

        if model.has_running_activity() {
            schedule_redraw_at(
                cx.editor.runtime().work().clone(),
                self.spinner.next_redraw(),
                cx.ingress.clone(),
            );
        }

        if let Some((_, progress)) =
            self.current_message_accent(cx.editor, cx.editor.assistant_theme())
        {
            if progress < 1.0 {
                if let Some(when) = self.message_focus_animation.sample().next_redraw {
                    schedule_redraw_at(
                        cx.editor.runtime().work().clone(),
                        when,
                        cx.ingress.clone(),
                    );
                }
            }
        }

        if context_rows > 0 && context_area.height > 0 {
            let style = cx.editor.assistant_theme().get("ui.text.info");
            surface.set_stringn(
                context_area.x + 1,
                context_area.y,
                context_line.as_deref().unwrap_or_default(),
                context_area.width.saturating_sub(2) as usize,
                style,
            );
        }

        if input_area_raw.height > 0 {
            let inset = Rect::new(
                input_area_raw.x + 1,
                input_area_raw.y,
                input_area_raw.width.saturating_sub(2),
                input_area_raw.height,
            );
            let theme = cx.editor.assistant_theme();
            let text_style = theme.get("ui.text");
            let placeholder_style = theme.get("ui.text.inactive");
            let input_border_style = theme
                .try_get("ui.assistant.input.border")
                .unwrap_or_else(|| theme.get("ui.window"));

            // Draw border around input area.
            if inset.width >= 4 && inset.height >= 3 {
                let bw = inset.width as usize;
                let rounded = cx.editor.config().acp.bubble_corners_rounded();
                let (tl, tr, bl, br) = if rounded {
                    ("╭", "╮", "╰", "╯")
                } else {
                    ("┌", "┐", "└", "┘")
                };
                let top = format!("{tl}{}{tr}", "─".repeat(bw.saturating_sub(2)));
                surface.set_stringn(inset.x, inset.y, &top, bw, input_border_style);
                for row in 1..inset.height.saturating_sub(1) {
                    let y = inset.y + row;
                    surface.set_stringn(inset.x, y, "│", 1, input_border_style);
                    surface.set_stringn(inset.right() - 1, y, "│", 1, input_border_style);
                }
                let bot = format!("{bl}{}{br}", "─".repeat(bw.saturating_sub(2)));
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
            self.input.set_area(input_area);

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

        let theme = cx.editor.assistant_theme();
        // Empty state
        if self.output.content().is_empty() {
            let empty_style = theme.get("ui.text.inactive");
            let center_y = content_area.y + content_area.height / 2;

            let msg = "Space A for menu";
            let mx = content_area.x
                + content_area
                    .width
                    .saturating_sub(UnicodeWidthStr::width(msg) as u16)
                    / 2;
            if center_y >= content_area.y && center_y < content_area.y + content_area.height {
                surface.set_stringn(mx, center_y, msg, content_area.width as usize, empty_style);
            }

            return;
        }

        // Render chat content using message list primitives.
        let corners = MessageCorners::Squared;
        let blocks = self.build_blocks(cx.editor, theme, content_area.width, corners);
        self.render_content(cx.editor, &blocks, content_area, surface);

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
        let key = match event {
            Event::Key(key) => *key,
            Event::Mouse(event) => return self.handle_mouse_event(event, cx.editor),
            _ => return EventResult::Ignored(None),
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
            if let Some(effects) = cx.editor.cancel_active_assistant_thread() {
                Self::apply_assistant_effects(cx.editor, effects);
            }
            cx.editor.set_status("Cancelling agent...");
            return EventResult::Consumed(None);
        }

        // Cycle keys work in any mode.
        if let Some(result) = self.handle_cycle_key(&key, cx) {
            return result;
        }

        if matches!(
            (key.code, key.modifiers),
            (KeyCode::PageUp, KeyModifiers::CONTROL)
                | (KeyCode::Left, KeyModifiers::ALT)
                | (KeyCode::Char('h'), KeyModifiers::ALT)
        ) && self.cycle_thread(cx.editor, -1)
        {
            return EventResult::Consumed(None);
        }

        if matches!(
            (key.code, key.modifiers),
            (KeyCode::PageDown, KeyModifiers::CONTROL)
                | (KeyCode::Right, KeyModifiers::ALT)
                | (KeyCode::Char('l'), KeyModifiers::ALT)
        ) && self.cycle_thread(cx.editor, 1)
        {
            return EventResult::Consumed(None);
        }

        let model = Self::assistant_model(cx.editor);

        if model.focus() == helix_view::assistant::thread::Focus::Input
            && matches!(key.code, KeyCode::Char(' '))
            && key.modifiers.is_empty()
        {
            log::warn!(
                "[assistant_ui] bubbling plain space focus_target={:?} input_mode={:?}",
                model.focus(),
                self.input.mode()
            );
            return EventResult::Ignored(None);
        }

        if model.focus() == helix_view::assistant::thread::Focus::Messages {
            let viewport_height = self.output.area().height as usize;
            let handled = match (key.code, key.modifiers) {
                (KeyCode::Esc, KeyModifiers::NONE) => {
                    self.focus_input_region(cx.editor);
                    true
                }
                (KeyCode::Char('i'), KeyModifiers::NONE) => {
                    self.focus_input_region(cx.editor);
                    self.input.enter_insert_mode("insert_mode".into());
                    true
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    log::warn!(
                        "[assistant_ui] enter open selected={:?} focus={:?}",
                        model.selected_entry_id(),
                        model.focus()
                    );
                    if !self.open_selected_message_details(cx.editor, Action::Replace) {
                        cx.editor.set_status("No assistant entry selected");
                    }
                    true
                }
                (KeyCode::Tab, KeyModifiers::NONE) => {
                    self.toggle_selected_message_fold(cx.editor);
                    true
                }
                (KeyCode::Char('y'), KeyModifiers::NONE) => {
                    self.yank_selected_message(cx.editor);
                    true
                }
                (KeyCode::Char('t'), KeyModifiers::NONE) => {
                    if let Ok((status, effects)) = cx.editor.toggle_active_assistant_follow() {
                        Self::apply_assistant_effects(cx.editor, effects);
                        cx.editor.set_status(status);
                    }
                    true
                }
                (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    self.select_prev_message(cx.editor);
                    true
                }
                (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    self.select_next_message(cx.editor);
                    true
                }
                (KeyCode::Home, KeyModifiers::NONE) => {
                    self.select_first_message(cx.editor);
                    true
                }
                (KeyCode::End, KeyModifiers::NONE) => {
                    self.select_last_message(cx.editor);
                    true
                }
                (KeyCode::PageUp, KeyModifiers::NONE)
                | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    self.select_prev_message_page(cx.editor);
                    true
                }
                (KeyCode::PageDown, KeyModifiers::NONE)
                | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                    self.select_next_message_page(cx.editor);
                    true
                }
                (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                    self.select_prev_message(cx.editor);
                    true
                }
                (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                    self.select_next_message(cx.editor);
                    true
                }
                _ => false,
            };

            if handled {
                if let Some(index) = self.selected_message(cx.editor) {
                    let mut cursor = MessageCursor::new(Some(index), model.content_scroll());
                    cursor.sync(&self.chat_layout, viewport_height);
                    self.set_content_scroll(cx.editor, cursor.scroll());
                }
                return EventResult::Consumed(None);
            }
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
            if matches!(key.code, KeyCode::Tab) && key.modifiers.contains(KeyModifiers::SHIFT) {
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
                                        let pos = helix_core::line_ending::line_end_char_index(
                                            &text, line,
                                        );
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
        if self.dispatch_input_key(key, cx) {
            log::warn!(
                "[assistant_event] key={} mode={:?} → Consumed (engine handled)",
                key.key_sequence_format(),
                self.input.mode()
            );
            EventResult::Consumed(None)
        } else {
            log::warn!(
                "[assistant_event] key={} mode={:?} → Ignored (unbound, bubbling)",
                key.key_sequence_format(),
                self.input.mode()
            );
            EventResult::Ignored(None)
        }
    }

    fn cursor(&self, _area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        if self.focused
            && Self::assistant_model(ctx).focus() == helix_view::assistant::thread::Focus::Input
        {
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

/// A popup that shows when an assistant backend requests permission.
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
        let bg = cx.editor.assistant_theme().get("ui.popup");
        for py in popup_area.y..popup_area.y + popup_area.height {
            for px in popup_area.x..popup_area.x + popup_area.width {
                surface[(px, py)].set_style(bg).set_symbol(" ");
            }
        }

        // Border top
        let border_style = cx.editor.assistant_theme().get("ui.popup");
        let top_border = format!("┌{}┐", "─".repeat(popup_width.saturating_sub(2) as usize));
        surface.set_stringn(x, y, &top_border, popup_width as usize, border_style);

        // Title
        let title_style = cx
            .editor
            .assistant_theme()
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
            let desc_style = cx.editor.assistant_theme().get("ui.text.inactive");
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
                cx.editor.assistant_theme().get("ui.menu.selected")
            } else {
                cx.editor.assistant_theme().get("ui.text")
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
                EventResult::Consumed(Some(crate::compositor::PostAction::RemoveById(
                    PERMISSION_ID,
                )))
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.send_response(PermissionResponse::Dismissed);
                EventResult::Consumed(Some(crate::compositor::PostAction::RemoveById(
                    PERMISSION_ID,
                )))
            }
            // Number shortcuts: 1-9 to select
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < self.options.len() {
                    self.send_response(PermissionResponse::Selected(self.options[idx].id.clone()));
                    EventResult::Consumed(Some(crate::compositor::PostAction::RemoveById(
                        PERMISSION_ID,
                    )))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::Handlers;
    use arc_swap::ArcSwap;
    use helix_core::Rope;
    use helix_loader::runtime_dirs;
    use helix_view::editor::Config;
    use helix_view::theme;
    use helix_view::Document;
    use std::path::Path;
    use std::sync::Arc;

    fn test_editor() -> Editor {
        let theme_loader = theme::Loader::new(runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        Editor::new(
            Rect::new(0, 0, 120, 40),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(config, |cfg: &Config| cfg)),
            helix_runtime::test::runtime(),
            Handlers::dummy(),
        )
    }

    fn seed_editor_with_file(editor: &mut Editor) -> helix_view::DocumentId {
        let mut doc = Document::from(
            Rope::from("fn main() {}\n"),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        );
        doc.set_path(Some(Path::new("main.rs")));
        let _ = doc.set_language_by_language_id("rust", &editor.syn_loader.load());
        editor.new_file_from_document(Action::VerticalSplit, doc)
    }

    fn with_test_runtime<T>(f: impl FnOnce() -> T) -> T {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        f()
    }

    fn seed_assistant(
        editor: &mut Editor,
        entries: Vec<ChatEntry>,
    ) -> helix_view::assistant::thread::Id {
        let thread = editor.assistant.create(
            helix_view::assistant::thread::Origin::Local,
            helix_view::assistant::thread::Scope::new(std::path::PathBuf::from(".")),
        );
        for entry in entries {
            let kind = match entry.kind {
                ChatEntryKind::UserMessage(text) => {
                    helix_view::assistant::thread::EntryKind::UserPrompt { text }
                }
                ChatEntryKind::AgentText(text) => {
                    helix_view::assistant::thread::EntryKind::AssistantText { text }
                }
                ChatEntryKind::ToolCall { id, name, status } => {
                    helix_view::assistant::thread::EntryKind::ToolCall(
                        helix_view::assistant::tool::Call {
                            id: helix_view::assistant::tool::Id::new(id),
                            name,
                            state: match status.as_str() {
                                "running" => helix_view::assistant::tool::State::Running,
                                "completed" | "done" => {
                                    helix_view::assistant::tool::State::Completed
                                }
                                "failed" => {
                                    helix_view::assistant::tool::State::Failed { message: None }
                                }
                                "cancelled" => helix_view::assistant::tool::State::Canceled,
                                _ => helix_view::assistant::tool::State::Pending,
                            },
                        },
                    )
                }
                ChatEntryKind::Status(text) => {
                    helix_view::assistant::thread::EntryKind::Status { text }
                }
                ChatEntryKind::ChangeSummary { files } => {
                    helix_view::assistant::thread::EntryKind::ChangeSummary(
                        helix_view::assistant::change::Summary {
                            files: vec![
                                helix_view::assistant::change::File {
                                    path: std::path::PathBuf::from("change.txt"),
                                    hunks: Vec::new(),
                                };
                                files
                            ],
                        },
                    )
                }
            };
            let effects = editor
                .assistant
                .apply(helix_view::assistant::event::Event::Thread {
                    thread,
                    event: helix_view::assistant::thread::Event::Content(
                        helix_view::assistant::thread::Content::Append(
                            helix_view::assistant::thread::NewEntry {
                                turn: None,
                                kind,
                                locations: Vec::new(),
                            },
                        ),
                    ),
                });
            AssistantPanel::apply_assistant_effects(editor, effects);
        }
        thread
    }

    fn seed_default_thread(editor: &mut Editor) -> helix_view::assistant::thread::Id {
        seed_assistant(
            editor,
            vec![
                ChatEntry {
                    id: helix_view::assistant::thread::EntryId::new(
                        std::num::NonZeroU64::new(1).unwrap(),
                    ),
                    locations: 0,
                    kind: ChatEntryKind::AgentText(
                        "Echo: \"hello\"\n\nLorem ipsum dolor sit amet, consectetur adipiscing elit."
                            .into(),
                    ),
                },
                ChatEntry {
                    id: helix_view::assistant::thread::EntryId::new(
                        std::num::NonZeroU64::new(2).unwrap(),
                    ),
                    locations: 0,
                    kind: ChatEntryKind::UserMessage("hi there from user".into()),
                },
            ],
        )
    }

    fn select_thread_entry(editor: &mut Editor, index: usize) {
        let entry = editor.assistant_entry_id_at(index).expect("entry");
        let effects = editor
            .set_active_assistant_focus(helix_view::assistant::thread::Focus::Messages)
            .expect("focus active messages");
        editor.apply_assistant_effects(effects);
        let effects = editor
            .select_active_assistant_entry(Some(entry))
            .expect("select active entry");
        editor.apply_assistant_effects(effects);
    }

    #[test]
    fn opening_message_details_creates_markdown_scratch_doc() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_editor_with_file(&mut editor);
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync_from_assistant(&mut editor);

            select_thread_entry(&mut editor, 0);
            assert!(panel.open_selected_message_details(&mut editor, Action::Replace));

            let (_view_id, doc) = focused!(editor);
            assert_eq!(doc.path(), None);
            assert!(doc.is_persistent_scratch());
            assert_eq!(doc.language_name(), Some("markdown"));
            assert!(doc
                .text()
                .slice(..)
                .to_string()
                .starts_with("# Agent Message\n"));
        });
    }

    #[test]
    fn reopening_same_message_reuses_existing_doc() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            let base = seed_editor_with_file(&mut editor);
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync_from_assistant(&mut editor);

            select_thread_entry(&mut editor, 0);
            assert!(panel.open_selected_message_details(&mut editor, Action::Replace));
            let first = focused!(editor).1.id();

            editor.switch(base, Action::Replace);
            assert!(panel.open_selected_message_details(&mut editor, Action::Replace));
            let reopened = focused!(editor).1.id();

            assert_eq!(reopened, first);
        });
    }

    #[test]
    fn opening_different_messages_creates_distinct_docs() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            let base = seed_editor_with_file(&mut editor);
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync_from_assistant(&mut editor);

            select_thread_entry(&mut editor, 0);
            assert!(panel.open_selected_message_details(&mut editor, Action::Replace));
            let first = focused!(editor).1.id();

            editor.switch(base, Action::Replace);
            assert!(editor.documents.contains_key(&first));
            select_thread_entry(&mut editor, 1);
            assert!(panel.open_selected_message_details(&mut editor, Action::Replace));
            let second = focused!(editor).1.id();

            assert_ne!(first, second);
            assert!(editor.documents.contains_key(&first));
            assert!(editor.documents.contains_key(&second));

            let first_label = editor.buffer_label(editor.documents.get(&first).unwrap());
            let second_label = editor.buffer_label(editor.documents.get(&second).unwrap());
            assert_ne!(first_label, second_label);
        });
    }

    #[test]
    fn opening_message_details_releases_panel_focus() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_editor_with_file(&mut editor);
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync_from_assistant(&mut editor);
            panel.focus_messages(&mut editor);
            select_thread_entry(&mut editor, 0);

            assert!(panel.open_selected_message_details(&mut editor, Action::Replace));
            assert!(!helix_view::traits::Focusable::is_focused(&panel));
        });
    }

    #[test]
    fn fold_toggle_collapses_agent_message_to_single_preview_line() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_assistant(
                &mut editor,
                vec![ChatEntry {
                    id: helix_view::assistant::thread::EntryId::new(
                        std::num::NonZeroU64::new(1).unwrap(),
                    ),
                    locations: 0,
                    kind: ChatEntryKind::AgentText(
                        "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua."
                            .into(),
                    ),
                }],
            );
            let mut panel = AssistantPanel::new();
            panel.sync_from_assistant(&mut editor);
            select_thread_entry(&mut editor, 0);

            let expanded_blocks = panel.build_blocks(
                &editor,
                editor.assistant_theme(),
                30,
                MessageCorners::Squared,
            );
            let expanded_height = expanded_blocks[0].height(true);

            assert!(panel.toggle_selected_message_fold(&mut editor));

            let collapsed_blocks = panel.build_blocks(
                &editor,
                editor.assistant_theme(),
                30,
                MessageCorners::Squared,
            );
            let collapsed_height = collapsed_blocks[0].height(true);

            assert!(expanded_height > collapsed_height);
            let entry = editor.assistant_entry_id_at(0).expect("entry");
            assert!(editor.assistant_entry_is_folded(entry));
        });
    }
}
