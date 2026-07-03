use crate::component_traits;
use crate::compositor::{Component, Context, Event, EventResult, RenderContext};
use crate::ui::animation::{
    Animation, AnimationDirection, AnimationFillMode, AnimationIterationCount, AnimationSpec,
    AnimationTimingFunction,
};
use crate::widgets::{schedule_redraw_at, Marquee};
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
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tui::text::{Span, Spans};

use crate::ui::markdown::{
    fit_bubble_width, wrap_text, Doc as MarkdownDoc, MarkdownCache, MarkdownLineStyles,
};

pub const ID: &str = "assistant-panel";

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
    pending_message_g: bool,
    markdown_cache: RefCell<HashMap<helix_view::assistant::thread::EntryId, MarkdownCache>>,
    mention: MentionPopup,
    elicitation_form: Option<helix_view::assistant::elicitation::FormState>,
    auth_selected: usize,
    auth_transient: bool,
    pending_subagent_jump: Option<PendingSubagentJump>,
    /// Layer whose key help is currently shown via `editor.autoinfo`.
    shown_help: Option<AssistantLayer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssistantLayer {
    Input,
    Messages,
    Elicitation,
    Auth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssistantAction {
    FocusInput,
    FocusInputInsert,
    FocusMessages,
    InputEscape,
    InsertInputChar(char),
    SendPrompt,
    OpenConfig,
    CancelRun,
    Primary,
    ToggleFold,
    Yank,
    FollowOrJump,
    Previous,
    Next,
    First,
    FirstPending,
    Last,
    PagePrevious,
    PageNext,
    Retry,
    ToggleReviewMode,
    AcceptReview,
    AcceptAllReview,
    RejectReview,
    RejectAllReview,
    TransientPrevious,
    TransientNext,
    TransientSubmit,
    TransientPop,
    TransientActivatePrevious,
    TransientActivateNext,
    TransientBackspace,
    ToggleHelp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BindingCode {
    Char(char),
    Enter,
    Esc,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BindingKey {
    code: BindingCode,
    modifiers: KeyModifiers,
}

impl BindingKey {
    const fn new(code: BindingCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    const fn modified(code: BindingCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    fn matches(self, key: &KeyEvent) -> bool {
        let code_matches = match (self.code, key.code) {
            (BindingCode::Char(lhs), KeyCode::Char(rhs)) => lhs == rhs,
            (BindingCode::Enter, KeyCode::Enter)
            | (BindingCode::Esc, KeyCode::Esc)
            | (BindingCode::Tab, KeyCode::Tab)
            | (BindingCode::Backspace, KeyCode::Backspace)
            | (BindingCode::Up, KeyCode::Up)
            | (BindingCode::Down, KeyCode::Down)
            | (BindingCode::Left, KeyCode::Left)
            | (BindingCode::Right, KeyCode::Right)
            | (BindingCode::Home, KeyCode::Home)
            | (BindingCode::End, KeyCode::End)
            | (BindingCode::PageUp, KeyCode::PageUp)
            | (BindingCode::PageDown, KeyCode::PageDown) => true,
            _ => false,
        };
        code_matches && key.modifiers == self.modifiers
    }
}

#[derive(Debug, Clone, Copy)]
struct AssistantBinding {
    key: BindingKey,
    action: AssistantAction,
    hint: Option<(&'static str, &'static str, u8)>,
}

impl AssistantBinding {
    const fn new(
        key: BindingKey,
        action: AssistantAction,
        hint: Option<(&'static str, &'static str, u8)>,
    ) -> Self {
        Self { key, action, hint }
    }
}

const INPUT_BINDINGS: &[AssistantBinding] = &[
    AssistantBinding::new(
        BindingKey::new(BindingCode::Tab),
        AssistantAction::FocusMessages,
        Some(("tab", "messages", 240)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Enter),
        AssistantAction::SendPrompt,
        Some(("enter", "send/newline", 230)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('@')),
        AssistantAction::InsertInputChar('@'),
        Some(("@", "mention", 140)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('/')),
        AssistantAction::InsertInputChar('/'),
        Some(("/", "command", 130)),
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('j'), KeyModifiers::CONTROL),
        AssistantAction::FocusMessages,
        Some(("C-j", "messages", 180)),
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('o'), KeyModifiers::CONTROL),
        AssistantAction::OpenConfig,
        Some(("C-o", "mode/model", 170)),
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('c'), KeyModifiers::CONTROL),
        AssistantAction::CancelRun,
        Some(("C-c", "cancel", 160)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Esc),
        AssistantAction::InputEscape,
        Some(("esc", "normal", 150)),
    ),
];

const MESSAGE_BINDINGS: &[AssistantBinding] = &[
    AssistantBinding::new(
        BindingKey::new(BindingCode::Esc),
        AssistantAction::FocusInput,
        Some(("esc", "input", 210)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('i')),
        AssistantAction::FocusInputInsert,
        Some(("i", "edit", 205)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Enter),
        AssistantAction::Primary,
        Some(("enter", "open", 240)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Tab),
        AssistantAction::ToggleFold,
        Some(("tab", "fold", 230)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('y')),
        AssistantAction::Yank,
        Some(("y", "yank", 220)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('t')),
        AssistantAction::FollowOrJump,
        Some(("t", "follow/jump", 215)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('k')),
        AssistantAction::Previous,
        Some(("j/k", "select", 225)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Up),
        AssistantAction::Previous,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('j')),
        AssistantAction::Next,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Down),
        AssistantAction::Next,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('g')),
        AssistantAction::FirstPending,
        Some(("gg/G", "edge", 200)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Home),
        AssistantAction::First,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::End),
        AssistantAction::Last,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('G')),
        AssistantAction::Last,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::PageUp),
        AssistantAction::PagePrevious,
        None,
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('u'), KeyModifiers::CONTROL),
        AssistantAction::PagePrevious,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::PageDown),
        AssistantAction::PageNext,
        None,
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('d'), KeyModifiers::CONTROL),
        AssistantAction::PageNext,
        None,
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('p'), KeyModifiers::CONTROL),
        AssistantAction::Previous,
        None,
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('n'), KeyModifiers::CONTROL),
        AssistantAction::Next,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('r')),
        AssistantAction::Retry,
        Some(("r", "retry", 120)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('R')),
        AssistantAction::ToggleReviewMode,
        Some(("R", "review", 110)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('a')),
        AssistantAction::AcceptReview,
        Some(("a/x", "review file", 100)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('x')),
        AssistantAction::RejectReview,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('A')),
        AssistantAction::AcceptAllReview,
        Some(("A/X", "review all", 90)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('X')),
        AssistantAction::RejectAllReview,
        None,
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('o'), KeyModifiers::CONTROL),
        AssistantAction::OpenConfig,
        Some(("C-o", "mode/model", 80)),
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('c'), KeyModifiers::CONTROL),
        AssistantAction::CancelRun,
        Some(("C-c", "cancel", 70)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('?')),
        AssistantAction::ToggleHelp,
        Some(("?", "help", 10)),
    ),
];

const ELICITATION_BINDINGS: &[AssistantBinding] = &[
    AssistantBinding::new(
        BindingKey::new(BindingCode::Esc),
        AssistantAction::TransientPop,
        Some(("esc", "messages", 240)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Tab),
        AssistantAction::TransientNext,
        Some(("tab", "next field", 230)),
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Tab, KeyModifiers::SHIFT),
        AssistantAction::TransientPrevious,
        Some(("S-tab", "prev field", 220)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Enter),
        AssistantAction::TransientSubmit,
        Some(("enter", "submit", 235)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Backspace),
        AssistantAction::TransientBackspace,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('h')),
        AssistantAction::TransientActivatePrevious,
        Some(("h/l", "change", 180)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Left),
        AssistantAction::TransientActivatePrevious,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('l')),
        AssistantAction::TransientActivateNext,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Right),
        AssistantAction::TransientActivateNext,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char(' ')),
        AssistantAction::TransientActivateNext,
        Some(("space", "toggle", 170)),
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Char('c'), KeyModifiers::CONTROL),
        AssistantAction::CancelRun,
        Some(("C-c", "cancel request", 160)),
    ),
];

const AUTH_BINDINGS: &[AssistantBinding] = &[
    AssistantBinding::new(
        BindingKey::new(BindingCode::Esc),
        AssistantAction::TransientPop,
        Some(("esc", "messages", 240)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Tab),
        AssistantAction::TransientNext,
        Some(("tab", "next method", 230)),
    ),
    AssistantBinding::new(
        BindingKey::modified(BindingCode::Tab, KeyModifiers::SHIFT),
        AssistantAction::TransientPrevious,
        Some(("S-tab", "prev method", 220)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Enter),
        AssistantAction::TransientSubmit,
        Some(("enter", "authenticate", 235)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('k')),
        AssistantAction::TransientPrevious,
        Some(("j/k", "select", 210)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Up),
        AssistantAction::TransientPrevious,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('j')),
        AssistantAction::TransientNext,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Down),
        AssistantAction::TransientNext,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('?')),
        AssistantAction::ToggleHelp,
        Some(("?", "help", 10)),
    ),
];

#[cfg(test)]
fn assistant_escape_target(layer: AssistantLayer, input_insert: bool) -> Option<AssistantLayer> {
    match layer {
        AssistantLayer::Auth | AssistantLayer::Elicitation => Some(AssistantLayer::Messages),
        AssistantLayer::Messages => Some(AssistantLayer::Input),
        AssistantLayer::Input if input_insert => Some(AssistantLayer::Input),
        AssistantLayer::Input => None,
    }
}

#[derive(Clone, Copy)]
struct MessageNavigationState {
    selected: Option<usize>,
    scroll: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingSubagentJump {
    session_id: String,
    message_start_index: Option<usize>,
}

#[derive(Default)]
struct MentionPopup {
    active: bool,
    query: String,
    token_start: usize,
    token_end: usize,
    selected: usize,
    candidates: Vec<MentionCandidate>,
    context_keys: Vec<String>,
}

#[derive(Clone)]
struct MentionCandidate {
    label: String,
    detail: String,
    replacement: String,
    kind: MentionCandidateKind,
}

#[derive(Clone)]
enum MentionCandidateKind {
    File(PathBuf),
    Selection,
    Diagnostics,
    Diff,
    Command,
}

fn byte_index_at_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn char_index_at_byte(text: &str, byte_index: usize) -> usize {
    text[..byte_index.min(text.len())].chars().count()
}

fn mention_matches(label: &str, query: &str) -> bool {
    query.is_empty()
        || label
            .to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
}

fn relative_mention_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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
        let active_thread = model.active_thread;
        let entries_len = model.entries.len();
        self.output.set_content(model.entries);
        if !self.output.is_following_end() {
            self.output.scroll_to(model.content_scroll);
        }
        self.consume_pending_subagent_jump(editor, active_thread, entries_len);
    }

    fn consume_pending_subagent_jump(
        &mut self,
        editor: &mut Editor,
        active_thread: Option<helix_view::assistant::thread::Id>,
        entries_len: usize,
    ) {
        let Some(pending) = self.pending_subagent_jump.as_ref() else {
            return;
        };
        let Some(active_thread) = active_thread else {
            return;
        };
        let matches_active = editor
            .assistant_known_sessions()
            .into_iter()
            .any(|(session, thread)| session == pending.session_id && thread == active_thread);
        if !matches_active {
            return;
        }
        let Some(index) = pending.message_start_index else {
            self.pending_subagent_jump = None;
            return;
        };
        if index >= entries_len {
            return;
        }
        self.pending_subagent_jump = None;
        self.select_message(editor, Some(index));
        editor.set_status("Opened subagent session");
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
        model.active_thread?;
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
            pending_message_g: false,
            markdown_cache: RefCell::new(HashMap::new()),
            mention: MentionPopup::default(),
            elicitation_form: None,
            auth_selected: 0,
            auth_transient: false,
            pending_subagent_jump: None,
            shown_help: None,
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
        self.elicitation_form = None;
        self.auth_transient = false;
        self.set_focus(editor, helix_view::assistant::thread::Focus::Input);
        self.message_focus_animation.stop();
        self.set_focused(true);
    }

    pub fn focus_messages(&mut self, editor: &mut Editor) {
        self.elicitation_form = None;
        self.auth_transient = false;
        if self.input.mode() == Mode::Insert {
            self.input.exit_insert_mode();
        }
        self.set_focused(true);
        self.set_focus(editor, helix_view::assistant::thread::Focus::Messages);
    }

    fn focus_messages_without_animation(&mut self, editor: &mut Editor) {
        self.elicitation_form = None;
        self.auth_transient = false;
        if self.input.mode() == Mode::Insert {
            self.input.exit_insert_mode();
        }
        self.set_focused(true);
        self.set_focus(editor, helix_view::assistant::thread::Focus::Messages);
    }

    pub fn focus_input_region(&mut self, editor: &mut Editor) {
        self.elicitation_form = None;
        self.auth_transient = false;
        self.set_focused(true);
        self.set_focus(editor, helix_view::assistant::thread::Focus::Input);
        self.message_focus_animation.stop();
    }

    fn active_layer(&self, editor: &Editor) -> AssistantLayer {
        self.active_layer_for_model(&Self::assistant_model(editor))
    }

    fn active_layer_for_model(&self, model: &helix_view::model::AssistantModel) -> AssistantLayer {
        if self.elicitation_form.is_some() {
            AssistantLayer::Elicitation
        } else if self.auth_transient
            && matches!(
                model.auth,
                helix_view::assistant::auth::State::Required { .. }
                    | helix_view::assistant::auth::State::Failed { .. }
            )
        {
            AssistantLayer::Auth
        } else {
            match model.focus() {
                helix_view::assistant::thread::Focus::Input => AssistantLayer::Input,
                helix_view::assistant::thread::Focus::Messages => AssistantLayer::Messages,
            }
        }
    }

    fn bindings_for_layer(layer: AssistantLayer) -> &'static [AssistantBinding] {
        match layer {
            AssistantLayer::Input => INPUT_BINDINGS,
            AssistantLayer::Messages => MESSAGE_BINDINGS,
            AssistantLayer::Elicitation => ELICITATION_BINDINGS,
            AssistantLayer::Auth => AUTH_BINDINGS,
        }
    }

    fn binding_for_key(layer: AssistantLayer, key: &KeyEvent) -> Option<&'static AssistantBinding> {
        Self::bindings_for_layer(layer)
            .iter()
            .find(|binding| binding.key.matches(key))
    }

    /// Key help for a layer: (key, label) pairs, highest priority first.
    /// Single source: the layer binding tables — this feeds the info popup.
    fn layer_help_entries(
        &self,
        model: &helix_view::model::AssistantModel,
        layer: AssistantLayer,
    ) -> Vec<(&'static str, &'static str)> {
        let mut entries = Self::bindings_for_layer(layer)
            .iter()
            .filter_map(|binding| binding.hint)
            .collect::<Vec<_>>();

        if layer == AssistantLayer::Messages {
            let review_selected = model
                .selected_entry_id()
                .and_then(|id| self.output.content().iter().find(|entry| entry.id == id))
                .is_some_and(|entry| {
                    matches!(
                        &entry.kind,
                        helix_view::model::AssistantEntryKind::ReviewSummary { .. }
                    )
                });
            if !review_selected {
                entries.retain(|(key, _, _)| !matches!(*key, "a/x" | "A/X" | "R"));
            }
        }

        entries.sort_by(|a, b| b.2.cmp(&a.2));
        entries
            .into_iter()
            .map(|(key, label, _)| (key, label))
            .collect()
    }

    const fn layer_title(layer: AssistantLayer) -> &'static str {
        match layer {
            AssistantLayer::Input => "Assistant: input",
            AssistantLayer::Messages => "Assistant: messages",
            AssistantLayer::Elicitation => "Assistant: form",
            AssistantLayer::Auth => "Assistant: auth",
        }
    }

    fn layer_info(
        &self,
        model: &helix_view::model::AssistantModel,
        layer: AssistantLayer,
    ) -> helix_view::info::Info {
        let entries = self.layer_help_entries(model, layer);
        helix_view::info::Info::new(Self::layer_title(layer), &entries)
    }

    /// Reconcile the help popup with the active layer. Transient layers
    /// (form/auth) auto-show their keys (gated by `editor.auto-info`);
    /// other layers only show help while `?`-toggled, and any layer
    /// change dismisses a stale popup.
    fn sync_help(&mut self, editor: &mut Editor) {
        let layer = self.active_layer(editor);
        if let Some(shown) = self.shown_help {
            if shown != layer || !self.focused {
                self.shown_help = None;
                editor.autoinfo = None;
            }
        }
        let transient = matches!(layer, AssistantLayer::Elicitation | AssistantLayer::Auth);
        if self.focused && transient && self.shown_help.is_none() && editor.config().auto_info {
            let info = self.layer_info(&Self::assistant_model(editor), layer);
            editor.autoinfo = Some(info);
            self.shown_help = Some(layer);
        }
    }

    fn toggle_help(&mut self, editor: &mut Editor) {
        let layer = self.active_layer(editor);
        if self.shown_help == Some(layer) {
            self.shown_help = None;
            editor.autoinfo = None;
        } else {
            let info = self.layer_info(&Self::assistant_model(editor), layer);
            editor.autoinfo = Some(info);
            self.shown_help = Some(layer);
        }
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
        model: &helix_view::model::AssistantModel,
        theme: &helix_view::Theme,
    ) -> Option<(GraphicsStyle, f32)> {
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

    #[allow(
        clippy::too_many_arguments,
        reason = "message styling helper keeps independent visual attributes explicit at call sites"
    )]
    fn tool_call_message(
        &self,
        name: &str,
        status: &str,
        output: &str,
        expanded: bool,
        selected: bool,
        theme: &helix_view::Theme,
        accent: Option<(GraphicsStyle, f32)>,
    ) -> Message<'static> {
        let icon = if status == "running" {
            self.spinner.frame().to_string()
        } else {
            helix_view::model::AssistantEntry::status_icon(status).to_string()
        };
        let tone = helix_view::model::AssistantEntry::status_tone(status);
        let icon_style = Self::entry_tone_style(theme, tone);
        let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
        let muted_style = theme.get("ui.text.inactive");
        let summary = if output.is_empty() {
            status.to_string()
        } else {
            Self::collapse_preview(output, 96)
        };
        let mut lines = vec![Spans::from(vec![
            Span::styled(format!(" {icon} "), icon_style),
            Span::styled(name.to_string(), title_style),
            Span::styled(format!("  {summary}"), muted_style),
        ])];

        if expanded && !output.is_empty() {
            let plus = theme.get("diff.plus");
            let minus = theme.get("diff.minus");
            let delta = theme.get("diff.delta");
            let text = theme.get("ui.text");
            for line in output.lines() {
                let style = if line.starts_with('+') && !line.starts_with("+++") {
                    plus
                } else if line.starts_with('-') && !line.starts_with("---") {
                    minus
                } else if line.starts_with("@@") {
                    delta
                } else {
                    text
                };
                lines.push(Spans::from(Span::styled(format!("   {line}"), style)));
            }
        }

        let mut message = Message::plain(lines);
        if let Some((style, _)) = accent {
            message = message.with_selected_bar("▌", style);
        }
        if selected {
            message = Self::decorate_selected_message(
                message,
                true,
                theme,
                Self::plain_accessory_align(),
            );
        }
        message
    }

    fn review_message(
        &self,
        mode: helix_view::assistant::review::Mode,
        files: &[(
            &std::path::Path,
            helix_view::assistant::review::Status,
            &str,
        )],
        expanded: bool,
        selected: bool,
        theme: &helix_view::Theme,
        accent: Option<(GraphicsStyle, f32)>,
    ) -> Message<'static> {
        let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
        let muted_style = theme.get("ui.text.inactive");
        let pending_style = theme.get("warning");
        let diff_styles = crate::widgets::DiffStyles::from_theme(theme);
        let pending = files
            .iter()
            .filter(|(_, status, _)| status.is_pending())
            .count();
        let mut lines = vec![Spans::from(vec![
            Span::styled(" review ", title_style),
            Span::styled(mode.label().to_string(), muted_style),
            Span::styled(format!("  {} files", files.len()), muted_style),
            Span::styled(format!("  {pending} pending"), pending_style),
        ])];

        for (path, status, diff) in files {
            lines.push(Spans::from(vec![
                Span::styled("   ", muted_style),
                Span::styled(path.display().to_string(), title_style),
                Span::styled(format!("  {}", status.label()), muted_style),
            ]));
            if expanded {
                let doc = crate::widgets::DiffDocument::from_unified_diff(diff);
                let diff_lines = doc.layout_lines(
                    &diff_styles,
                    crate::widgets::DiffOptions {
                        context: 3,
                        line_numbers: false,
                    },
                    &BTreeSet::new(),
                );
                for line in diff_lines {
                    let mut spans = vec![Span::styled("     ".to_string(), muted_style)];
                    spans.extend(line.0);
                    lines.push(Spans::from(spans));
                }
            }
        }

        let mut message = Message::plain(lines);
        if let Some((style, _)) = accent {
            message = message.with_selected_bar("▌", style);
        }
        if selected {
            message = Self::decorate_selected_message(
                message,
                true,
                theme,
                Self::plain_accessory_align(),
            );
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

    fn pending_elicitation(editor: &Editor) -> Option<helix_view::assistant::thread::Elicitation> {
        Self::assistant_model(editor)
            .pending_elicitations
            .into_iter()
            .find(|item| item.status == helix_view::assistant::thread::ElicitationStatus::Pending)
    }

    fn complete_pending_elicitation(
        &mut self,
        editor: &mut Editor,
        response: helix_view::assistant::thread::ElicitationResponse,
    ) -> bool {
        let model = Self::assistant_model(editor);
        let Some(thread) = model.active_thread else {
            return false;
        };
        let Some(elicitation) = model
            .pending_elicitations
            .into_iter()
            .find(|item| item.status == helix_view::assistant::thread::ElicitationStatus::Pending)
        else {
            return false;
        };
        let effects = editor.complete_assistant_elicitation(thread, elicitation.id, response);
        Self::apply_assistant_effects(editor, effects);
        self.elicitation_form = None;
        true
    }

    fn sync_elicitation_form(
        &mut self,
        elicitation: &helix_view::assistant::thread::Elicitation,
    ) -> bool {
        if !matches!(
            elicitation.mode,
            helix_view::assistant::thread::ElicitationMode::Form { .. }
        ) {
            self.elicitation_form = None;
            return false;
        }
        self.elicitation_form = match self.elicitation_form.take() {
            Some(state) => state.sync(elicitation),
            None => helix_view::assistant::elicitation::FormState::new(elicitation),
        };
        self.elicitation_form.is_some()
    }

    fn accept_pending_elicitation(&mut self, editor: &mut Editor) -> bool {
        let Some(elicitation) = Self::pending_elicitation(editor) else {
            self.elicitation_form = None;
            return false;
        };
        let values = match &elicitation.mode {
            helix_view::assistant::thread::ElicitationMode::Form { fields, .. } => {
                self.sync_elicitation_form(&elicitation);
                let Some(form) = &self.elicitation_form else {
                    return false;
                };
                match form.submit_values(fields) {
                    Ok(values) => values,
                    Err(err) => {
                        editor.set_status(format!("Required field missing: {}", err.field));
                        return true;
                    }
                }
            }
            helix_view::assistant::thread::ElicitationMode::Url { .. } => Vec::new(),
        };
        self.complete_pending_elicitation(
            editor,
            helix_view::assistant::thread::ElicitationResponse::Accept(values),
        )
    }

    fn cancel_pending_elicitation(&mut self, editor: &mut Editor) -> bool {
        self.complete_pending_elicitation(
            editor,
            helix_view::assistant::thread::ElicitationResponse::Cancel,
        )
    }

    fn active_auth_methods(editor: &Editor) -> Option<Vec<helix_view::assistant::auth::Method>> {
        match Self::assistant_model(editor).auth {
            helix_view::assistant::auth::State::Required { methods, .. }
            | helix_view::assistant::auth::State::Failed { methods, .. } => Some(methods),
            _ => None,
        }
    }

    fn select_auth_method(&mut self, editor: &Editor, delta: isize) -> bool {
        let Some(methods) = Self::active_auth_methods(editor) else {
            return false;
        };
        if methods.is_empty() {
            return true;
        }
        let len = methods.len() as isize;
        self.auth_selected = (self.auth_selected as isize + delta).rem_euclid(len) as usize;
        true
    }

    fn accept_auth_method(&mut self, editor: &mut Editor) -> bool {
        let model = Self::assistant_model(editor);
        let Some(thread) = model.active_thread else {
            return false;
        };
        let methods = match model.auth {
            helix_view::assistant::auth::State::Required { methods, .. }
            | helix_view::assistant::auth::State::Failed { methods, .. } => methods,
            _ => return false,
        };
        let Some(method) = methods.get(self.auth_selected.min(methods.len().saturating_sub(1)))
        else {
            return false;
        };
        let effects = editor.authenticate_assistant(thread, method.id.clone());
        Self::apply_assistant_effects(editor, effects);
        true
    }

    fn yank_pending_elicitation_url(&mut self, editor: &mut Editor) -> bool {
        let Some(elicitation) = Self::pending_elicitation(editor) else {
            return false;
        };
        let helix_view::assistant::thread::ElicitationMode::Url { url, .. } = elicitation.mode
        else {
            return false;
        };
        match editor.registers.write('"', vec![url]) {
            Ok(()) => {
                editor.set_status("Assistant URL yanked");
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

    #[allow(
        clippy::too_many_arguments,
        reason = "markdown cache lookup needs entry identity plus render style context"
    )]
    fn render_markdown_cached(
        &self,
        entry: helix_view::assistant::thread::EntryId,
        text: &str,
        width: usize,
        base_style: Style,
        styles: &MarkdownLineStyles,
        theme: &helix_view::Theme,
        loader: &helix_core::syntax::Loader,
    ) -> Vec<Spans<'static>> {
        self.markdown_cache
            .borrow_mut()
            .entry(entry)
            .or_default()
            .layout(
                &MarkdownDoc::new(text),
                width,
                base_style,
                styles,
                Some(theme),
                loader,
            )
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
        self.sync_mention_context(editor, &text);
        self.set_draft(editor, text);
    }

    fn sync_mention_context(&mut self, editor: &mut Editor, text: &str) {
        let Some(thread) = Self::assistant_model(editor).active_thread else {
            return;
        };
        let items = self.context_items_for_mentions(editor, text);
        let keys = items
            .iter()
            .map(helix_view::assistant::mention::key_for_kind)
            .collect::<Vec<_>>();
        if self.mention.context_keys == keys {
            return;
        }
        self.mention.context_keys = keys;
        Self::apply(
            editor,
            helix_view::assistant::Action::SetMentionContext { thread, items },
        );
    }

    fn context_items_for_mentions(
        &self,
        editor: &Editor,
        text: &str,
    ) -> Vec<helix_view::assistant::context::Kind> {
        let mut items = Vec::new();
        let scope = editor.active_assistant_scope_or_layout();
        for token in helix_view::assistant::mention::tokens(text) {
            match token.text.as_str() {
                "selection" => {
                    if let Some(item) = editor
                        .capture_current_surface(helix_view::collab::surface::Capture::Selection)
                    {
                        items.push(item);
                    }
                }
                "diagnostics" => {
                    if let Some(item) = Self::diagnostics_context(editor) {
                        items.push(item);
                    }
                }
                "diff" | "git-diff" => {
                    if let Some(item) = Self::diff_context(editor) {
                        items.push(item);
                    }
                }
                path => {
                    items.push(helix_view::assistant::context::Kind::File(
                        helix_view::assistant::context::File {
                            path: scope.cwd.join(path),
                        },
                    ));
                }
            }
        }
        items
    }

    fn diagnostics_context(editor: &Editor) -> Option<helix_view::assistant::context::Kind> {
        let doc = editor.focused_document()?;
        let path = doc.path()?.to_path_buf();
        let items = doc
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>();
        (!items.is_empty()).then_some(helix_view::assistant::context::Kind::Diagnostics(
            helix_view::assistant::context::Diagnostics { path, items },
        ))
    }

    fn diff_context(editor: &Editor) -> Option<helix_view::assistant::context::Kind> {
        let doc = editor.focused_document()?;
        let path = doc.path()?.to_path_buf();
        let handle = doc.diff_handle()?;
        let diff = handle.load();
        if diff.is_empty() {
            return None;
        }
        let mut summary = String::new();
        for index in 0..diff.len() {
            let hunk = diff.nth_hunk(index);
            use std::fmt::Write as _;
            let _ = writeln!(
                summary,
                "hunk {}: base {}..{} -> current {}..{}",
                index + 1,
                hunk.before.start + 1,
                hunk.before.end + 1,
                hunk.after.start + 1,
                hunk.after.end + 1
            );
        }
        Some(helix_view::assistant::context::Kind::Diff(
            helix_view::assistant::context::Diff {
                path,
                summary: summary.trim_end().to_string(),
            },
        ))
    }

    fn refresh_mention_popup(&mut self, editor: &Editor) {
        let Some((input, cursor)) = self.input_text_and_cursor(editor) else {
            self.mention.active = false;
            return;
        };
        let active_slash = helix_view::assistant::mention::active_slash_query(&input, cursor);
        let active_at = helix_view::assistant::mention::active_query(&input, cursor);
        let Some(active) = active_slash.clone().or(active_at) else {
            self.mention.active = false;
            return;
        };
        let token_end = if active_slash.is_some() {
            active.end
        } else {
            helix_view::assistant::mention::active_token(&input, cursor)
                .map(|token| token.end)
                .unwrap_or(active.end)
        };
        self.mention.active = true;
        self.mention.query = active.query;
        self.mention.token_start = active.start;
        self.mention.token_end = token_end;
        self.mention.candidates = if active_slash.is_some() {
            self.command_candidates(editor, &self.mention.query)
        } else {
            self.mention_candidates(editor, &self.mention.query)
        };
        if self.mention.selected >= self.mention.candidates.len() {
            self.mention.selected = self.mention.candidates.len().saturating_sub(1);
        }
    }

    fn input_text_and_cursor(&self, editor: &Editor) -> Option<(String, usize)> {
        let doc = self.input.document(editor)?;
        let text = doc.text().to_string();
        let cursor = doc
            .selection(self.input.view_id())
            .primary()
            .cursor(doc.text().slice(..));
        Some((text.clone(), byte_index_at_char(&text, cursor)))
    }

    fn mention_candidates(&self, editor: &Editor, query: &str) -> Vec<MentionCandidate> {
        const LIMIT: usize = 40;
        let scope = editor.active_assistant_scope_or_layout();
        let root = scope.cwd;
        let mut candidates = Vec::new();
        candidates.extend(Self::fixed_mention_candidates(query));
        candidates.extend(Self::open_buffer_mention_candidates(editor, &root, query));
        candidates.extend(Self::workspace_file_mention_candidates(
            editor, &root, query,
        ));
        let mut seen = std::collections::HashSet::new();
        candidates
            .into_iter()
            .filter(|candidate| seen.insert(candidate.replacement.clone()))
            .take(LIMIT)
            .collect()
    }

    fn command_candidates(&self, editor: &Editor, query: &str) -> Vec<MentionCandidate> {
        let model = Self::assistant_model(editor);
        model
            .commands
            .iter()
            .filter(|command| mention_matches(&command.name, query))
            .take(40)
            .map(|command| MentionCandidate {
                label: format!("/{}", command.name),
                detail: command.description.clone().unwrap_or_else(|| {
                    match command.category {
                        helix_view::assistant::thread::CommandCategory::Native => "native command",
                        helix_view::assistant::thread::CommandCategory::Mcp => "mcp command",
                    }
                    .to_string()
                }),
                replacement: command.name.clone(),
                kind: MentionCandidateKind::Command,
            })
            .collect()
    }

    fn fixed_mention_candidates(query: &str) -> Vec<MentionCandidate> {
        [
            (
                "selection",
                "current selection",
                MentionCandidateKind::Selection,
            ),
            (
                "diagnostics",
                "current diagnostics",
                MentionCandidateKind::Diagnostics,
            ),
            ("diff", "current git diff", MentionCandidateKind::Diff),
        ]
        .into_iter()
        .filter(|(label, _, _)| mention_matches(label, query))
        .map(|(label, detail, kind)| MentionCandidate {
            label: format!("@{label}"),
            detail: detail.to_string(),
            replacement: label.to_string(),
            kind,
        })
        .collect()
    }

    fn open_buffer_mention_candidates(
        editor: &Editor,
        root: &Path,
        query: &str,
    ) -> Vec<MentionCandidate> {
        editor
            .documents()
            .filter_map(|doc| doc.path())
            .filter_map(|path| {
                let replacement = relative_mention_path(root, path);
                mention_matches(&replacement, query).then(|| MentionCandidate {
                    label: format!("@{replacement}"),
                    detail: "open buffer".to_string(),
                    replacement,
                    kind: MentionCandidateKind::File(path.to_path_buf()),
                })
            })
            .collect()
    }

    fn workspace_file_mention_candidates(
        editor: &Editor,
        root: &Path,
        query: &str,
    ) -> Vec<MentionCandidate> {
        let config = editor.config().file_picker.clone();
        let current_file = editor
            .focused_document()
            .and_then(|doc| doc.path().cloned());
        let Ok(matches) =
            crate::fff::search_files_available(root, query, current_file.as_deref(), &config)
        else {
            return Vec::new();
        };
        matches
            .into_iter()
            .map(|item| {
                let replacement = relative_mention_path(root, &item.path);
                MentionCandidate {
                    label: format!("@{replacement}"),
                    detail: "workspace file".to_string(),
                    replacement,
                    kind: MentionCandidateKind::File(item.path),
                }
            })
            .collect()
    }

    fn accept_selected_mention(&mut self, editor: &mut Editor) -> bool {
        if !self.mention.active || self.mention.candidates.is_empty() {
            return false;
        }
        let Some(candidate) = self.mention.candidates.get(self.mention.selected).cloned() else {
            return false;
        };
        let Some(doc_id) = self.input.doc_id() else {
            return false;
        };
        let Some(doc) = editor.component_docs.get_mut(&doc_id) else {
            return false;
        };
        let start = char_index_at_byte(&doc.text().to_string(), self.mention.token_start);
        let end = char_index_at_byte(&doc.text().to_string(), self.mention.token_end);
        let prefix = match candidate.kind {
            MentionCandidateKind::Command => "/",
            _ => "@",
        };
        let replacement = helix_core::Tendril::from(format!("{prefix}{}", candidate.replacement));
        let transaction = helix_core::Transaction::change(
            doc.text(),
            [(start, end, Some(replacement))].into_iter(),
        );
        doc.apply(&transaction, self.input.view_id());

        let item = match candidate.kind {
            MentionCandidateKind::File(path) => Some(helix_view::assistant::context::Kind::File(
                helix_view::assistant::context::File { path },
            )),
            MentionCandidateKind::Selection => {
                editor.capture_current_surface(helix_view::collab::surface::Capture::Selection)
            }
            MentionCandidateKind::Diagnostics => Self::diagnostics_context(editor),
            MentionCandidateKind::Diff => Self::diff_context(editor),
            MentionCandidateKind::Command => None,
        };
        if let (Some(thread), Some(item)) = (Self::assistant_model(editor).active_thread, item) {
            Self::apply(
                editor,
                helix_view::assistant::Action::SetMentionContext {
                    thread,
                    items: vec![item],
                },
            );
        }
        self.mention.active = false;
        self.mention.context_keys.clear();
        self.sync_draft_to_assistant(editor);
        true
    }

    fn render_mention_popup(&self, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        if !self.mention.active || self.mention.candidates.is_empty() {
            return;
        }

        let Some((cursor_x, cursor_y)) = self.last_input_cursor else {
            return;
        };

        let input_area = self.input.area();
        if input_area.width == 0 || input_area.height == 0 {
            return;
        }

        let visible_len = self.mention.candidates.len().min(6);
        let width = self
            .mention
            .candidates
            .iter()
            .take(visible_len)
            .map(|candidate| {
                UnicodeWidthStr::width(candidate.label.as_str())
                    + 2
                    + UnicodeWidthStr::width(candidate.detail.as_str())
            })
            .max()
            .unwrap_or(18)
            .clamp(18, 56) as u16;
        let width = width.min(input_area.width.max(1));
        let height = visible_len as u16;
        let x = cursor_x.min(input_area.right().saturating_sub(width));
        let y = if cursor_y >= height {
            cursor_y.saturating_sub(height)
        } else {
            input_area.bottom()
        };
        let area = Rect::new(x, y, width, height);
        let rat_area = tui::ratatui::to_ratatui_rect(area);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, rat_area, surface);

        let theme = cx.assistant_theme();
        let background = theme.get("ui.popup");
        let selected = theme.get("ui.menu.selected");
        let text = theme.get("ui.text");
        let inactive = theme.get("ui.text.inactive");
        surface.set_style(rat_area, tui::ratatui::to_ratatui_style(background));

        for (row, candidate) in self.mention.candidates.iter().take(visible_len).enumerate() {
            let y = area.y + row as u16;
            let is_selected = row == self.mention.selected;
            let row_style = if is_selected { selected } else { background };
            surface.set_stringn(
                area.x,
                y,
                " ".repeat(area.width as usize),
                area.width as usize,
                tui::ratatui::to_ratatui_style(row_style),
            );

            let label_style = if is_selected { selected } else { text };
            let label_width = UnicodeWidthStr::width(candidate.label.as_str()) as u16;
            surface.set_stringn(
                area.x,
                y,
                &candidate.label,
                area.width as usize,
                tui::ratatui::to_ratatui_style(label_style),
            );

            let detail_x = area.x + label_width.saturating_add(2);
            if detail_x < area.right() {
                surface.set_stringn(
                    detail_x,
                    y,
                    &candidate.detail,
                    area.right().saturating_sub(detail_x) as usize,
                    tui::ratatui::to_ratatui_style(if is_selected { selected } else { inactive }),
                );
            }
        }
    }

    fn sync_input_from_assistant(&mut self, editor: &mut Editor, draft: &str) {
        let Some(doc) = self.input.document_mut(editor) else {
            return;
        };

        if doc.text() == draft {
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
                    .find(|(_, p)| p.content.is::<AssistantModel>())
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
        model: &helix_view::model::AssistantModel,
        theme: &helix_view::Theme,
        loader: &helix_core::syntax::Loader,
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
        let strike_style = agent_style.add_modifier(Modifier::CROSSED_OUT);
        let link_style = theme.get("markup.link.url");
        let quote_style = theme.get("markup.quote");
        let list_style = theme.get("markup.list.unnumbered");
        let active_accent = self.current_message_accent(model, theme);
        let agent_name = model.agent_name.as_str();

        // Flex width: min 60%, max 90% of panel — but never panic on tiny sizes.
        let max_bubble = ((width as u32 * 90 / 100) as u16).min(width).max(4);
        let min_bubble = ((width as u32 * 60 / 100) as u16).max(20).min(max_bubble);

        let mut blocks = Vec::new();
        let selected = model.selected_entry_id();

        for entry in self.output.content().iter() {
            let entry_id = Some(entry.id);
            let message_accent = if entry_id == selected {
                active_accent
            } else {
                None
            };
            let selected = entry_id == selected;
            let collapsed = model.is_folded(entry.id);
            if let helix_view::model::AssistantEntryKind::ToolCall {
                name,
                status,
                output,
                ..
            } = &entry.kind
            {
                let expanded = status == "failed" || collapsed;
                blocks.push(self.tool_call_message(
                    name,
                    status,
                    output,
                    expanded,
                    selected,
                    theme,
                    message_accent,
                ));
                continue;
            }
            if let helix_view::model::AssistantEntryKind::ReviewSummary { mode, files } =
                &entry.kind
            {
                let files = files
                    .iter()
                    .map(|(path, diff, status)| (path.as_path(), *status, diff.as_str()))
                    .collect::<Vec<_>>();
                blocks.push(self.review_message(
                    *mode,
                    &files,
                    collapsed,
                    selected,
                    theme,
                    message_accent,
                ));
                continue;
            }

            let display = entry.display(agent_name);
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
                                md_lines = self.render_markdown_cached(
                                    entry.id,
                                    &display.text,
                                    inner_w,
                                    agent_style,
                                    &MarkdownLineStyles {
                                        heading: heading_style,
                                        code: code_style,
                                        bold: bold_style,
                                        italic: italic_style,
                                        strike: strike_style,
                                        link: link_style,
                                        quote: quote_style,
                                        list: list_style,
                                        separator: separator_style,
                                    },
                                    theme,
                                    loader,
                                );
                            }
                            (agent_label_style, md_lines)
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
                        message, selected, entry, theme, agent_name,
                    );
                    blocks.push(message);
                }
            }
        }

        for elicitation in model
            .pending_elicitations
            .iter()
            .filter(|item| item.status == helix_view::assistant::thread::ElicitationStatus::Pending)
        {
            blocks.push(self.elicitation_message(elicitation, theme));
        }

        if let Some(message) = self.auth_message(model, theme) {
            blocks.push(message);
        }

        for terminal in &model.terminals {
            blocks.push(Self::terminal_message(terminal, theme));
        }

        blocks
    }

    fn elicitation_message<'a>(
        &self,
        elicitation: &helix_view::assistant::thread::Elicitation,
        theme: &helix_view::Theme,
    ) -> Message<'a> {
        let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
        let muted_style = theme.get("ui.text.inactive");
        let warning_style = theme.get("warning");
        let mut lines = Vec::new();
        match &elicitation.mode {
            helix_view::assistant::thread::ElicitationMode::Form { message, fields } => {
                lines.push(Spans::from(vec![
                    Span::styled(" ? ", warning_style),
                    Span::styled(message.clone(), title_style),
                    Span::styled("  tab field  enter submit  esc cancel", muted_style),
                ]));
                let form = self
                    .elicitation_form
                    .as_ref()
                    .filter(|form| form.request_id() == elicitation.id.as_str());
                for (index, field) in fields.iter().enumerate() {
                    let required = if field.required { " *" } else { "" };
                    let marker = if form.is_some_and(|form| form.focused() == index) {
                        " > "
                    } else {
                        "   "
                    };
                    let value = match (form.and_then(|form| form.value(index)), field.field_type) {
                        (
                            Some(helix_view::assistant::elicitation::FieldValue::Text(text)),
                            helix_view::assistant::thread::ElicitationFieldType::Text
                            | helix_view::assistant::thread::ElicitationFieldType::Textarea,
                        ) => text.clone(),
                        (
                            Some(helix_view::assistant::elicitation::FieldValue::Select(selected)),
                            helix_view::assistant::thread::ElicitationFieldType::Select,
                        ) => field
                            .options
                            .get(*selected)
                            .map(|option| option.label.clone())
                            .unwrap_or_else(|| "<none>".to_string()),
                        (
                            Some(helix_view::assistant::elicitation::FieldValue::Bool(value)),
                            helix_view::assistant::thread::ElicitationFieldType::Bool,
                        ) => {
                            if *value {
                                "yes".to_string()
                            } else {
                                "no".to_string()
                            }
                        }
                        _ => match field.field_type {
                            helix_view::assistant::thread::ElicitationFieldType::Select => field
                                .options
                                .first()
                                .map(|option| option.label.clone())
                                .unwrap_or_else(|| "<none>".to_string()),
                            helix_view::assistant::thread::ElicitationFieldType::Bool => {
                                "no".to_string()
                            }
                            helix_view::assistant::thread::ElicitationFieldType::Text
                            | helix_view::assistant::thread::ElicitationFieldType::Textarea => {
                                String::new()
                            }
                        },
                    };
                    let label = field.label.as_deref().unwrap_or(&field.name);
                    lines.push(Spans::from(Span::styled(
                        format!("{marker}{label}{required}: {value}"),
                        if marker.trim().is_empty() {
                            muted_style
                        } else {
                            title_style
                        },
                    )));
                }
            }
            helix_view::assistant::thread::ElicitationMode::Url { message, url } => {
                lines.push(Spans::from(vec![
                    Span::styled(" ? ", warning_style),
                    Span::styled(message.clone(), title_style),
                    Span::styled("  y copy  esc cancel", muted_style),
                ]));
                lines.push(Spans::from(Span::styled(format!("   {url}"), muted_style)));
            }
        }
        Message::plain(lines)
    }

    fn terminal_message<'a>(
        terminal: &helix_view::model::AssistantTerminal,
        theme: &helix_view::Theme,
    ) -> Message<'a> {
        let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
        let muted_style = theme.get("ui.text.inactive");
        let status_style = match terminal.state.as_str() {
            "running" => theme.get("warning"),
            state if state.starts_with("exited:0") => theme.get("diff.plus"),
            state if state.starts_with("failed:") || state.starts_with("exited:") => {
                theme.get("error")
            }
            _ => muted_style,
        };
        let mut lines = vec![Spans::from(vec![
            Span::styled(" $ ", status_style),
            Span::styled(
                terminal
                    .title
                    .clone()
                    .unwrap_or_else(|| terminal.id.to_string()),
                title_style,
            ),
            Span::styled(format!("  {}", terminal.state), status_style),
        ])];
        for line in terminal
            .output
            .lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            lines.push(Spans::from(Span::styled(format!("   {line}"), muted_style)));
        }
        Message::plain(lines)
    }

    fn auth_message<'a>(
        &self,
        model: &helix_view::model::AssistantModel,
        theme: &helix_view::Theme,
    ) -> Option<Message<'a>> {
        let (methods, error, authenticating) = match &model.auth {
            helix_view::assistant::auth::State::Required { methods, error, .. } => {
                (methods.as_slice(), error.as_deref(), None)
            }
            helix_view::assistant::auth::State::Failed { methods, error, .. } => {
                (methods.as_slice(), Some(error.as_str()), None)
            }
            helix_view::assistant::auth::State::Authenticating { method, .. } => {
                (&[][..], None, Some(method.name.as_str()))
            }
            _ => return None,
        };
        let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
        let muted_style = theme.get("ui.text.inactive");
        let warning_style = theme.get("warning");
        let mut lines = vec![Spans::from(vec![
            Span::styled(" ! ", warning_style),
            Span::styled("Authentication required", title_style),
            Span::styled("  j/k select  enter authenticate", muted_style),
        ])];
        if let Some(name) = authenticating {
            lines.push(Spans::from(Span::styled(
                format!("   Authenticating with {name}..."),
                muted_style,
            )));
        }
        if let Some(error) = error {
            lines.push(Spans::from(Span::styled(
                format!("   {error}"),
                theme.get("error"),
            )));
        }
        for (index, method) in methods.iter().enumerate() {
            let selected = index == self.auth_selected.min(methods.len().saturating_sub(1));
            let marker = if selected { " > " } else { "   " };
            let suffix = if method.terminal.is_some() {
                " terminal"
            } else {
                ""
            };
            lines.push(Spans::from(Span::styled(
                format!("{marker}{}{suffix}", method.name),
                if selected { title_style } else { muted_style },
            )));
        }
        Some(Message::plain(lines))
    }

    /// Render chat blocks directly to the surface with proper scroll.
    fn render_content(
        &mut self,
        model: &helix_view::model::AssistantModel,
        blocks: &[Message],
        area: Rect,
        surface: &mut crate::render::CellSurface,
    ) {
        if area.height == 0 || area.width == 0 {
            self.chat_layout = MessageListState::default();
            return;
        }

        let cursor = MessageCursor::new(self.selected_index_in(model), model.content_scroll());
        self.chat_layout = message_list(surface, area, blocks, cursor.scroll(), cursor.selected());
        let mut cursor = cursor;
        cursor.clamp_selection(&self.chat_layout);
        self.output.scroll_to(cursor.scroll());
        self.output
            .set_content_height(self.chat_layout.total_height);
    }

    fn render_surface(
        &mut self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        cx: &RenderContext,
    ) {
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
        let model = cx.assistant_model(false);
        let plan = model.plan_section();
        let context_items = &model.context_items;
        let context_line = model.context_line();
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
            let theme = cx.assistant_theme();
            let bg_style = theme.get("ui.background");
            {
                let area = tui::ratatui::to_ratatui_rect(inner);
                tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
                surface.set_style(area, tui::ratatui::to_ratatui_style(bg_style));
            };

            // Left border
            let border_style = theme.get("ui.window");
            crate::widgets::vdivider(surface, border_area, border_style);

            let header_style = if self.focused {
                theme.get("ui.statusline")
            } else {
                theme.get("ui.statusline.inactive")
            };
            surface.set_style(
                tui::ratatui::to_ratatui_rect(header_area),
                tui::ratatui::to_ratatui_style(header_style),
            );
            let header = model.header();
            let agent_busy = model.agent_busy;
            // Agent status indicator: filled circle when the agent is doing
            // work (gets attention), middle-dot when idle (quiet, harmonises
            // with the chrome's separator language). The state change itself
            // is the cue — the glyph swaps, not just the colour.
            let (dot_glyph, dot_style) = if agent_busy {
                ("\u{25cf}", theme.get("warning"))
            } else {
                ("\u{00b7}", theme.get("hint"))
            };
            surface.set_stringn(
                header_area.x + 1,
                header_area.y,
                dot_glyph,
                1,
                tui::ratatui::to_ratatui_style(dot_style),
            );
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
                        surface.set_stringn(
                            rx,
                            header_area.y,
                            "  ",
                            2,
                            tui::ratatui::to_ratatui_style(header_style),
                        );
                        rx += 2;
                    }
                    let style = Self::header_item_style(theme, header_style, item.tone);
                    let width = UnicodeWidthStr::width(item.label.as_str());
                    surface.set_stringn(
                        rx,
                        header_area.y,
                        &item.label,
                        width,
                        tui::ratatui::to_ratatui_style(style),
                    );
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
                surface.set_stringn(
                    x,
                    header_area.y,
                    &item.label,
                    budget,
                    tui::ratatui::to_ratatui_style(style),
                );
                x = x.saturating_add(width.min(right_edge.saturating_sub(x)) + 1);
            }
            let bar_style = if self.focused {
                theme.get("ui.statusline")
            } else {
                theme.get("ui.statusline.inactive")
            };
            surface.set_style(
                tui::ratatui::to_ratatui_rect(bar_area),
                tui::ratatui::to_ratatui_style(bar_style),
            );
            // Status only — key help lives in the info popup (`?`), per the
            // editor-wide rule that statuslines never list shortcuts.
            let theme = cx.assistant_theme();
            let layer = self.active_layer_for_model(&model);
            let badge = match layer {
                AssistantLayer::Input => "INPUT",
                AssistantLayer::Messages => "MESSAGES",
                AssistantLayer::Elicitation => "FORM",
                AssistantLayer::Auth => "AUTH",
            };
            let badge_style = if self.focused {
                theme.get("ui.text.focus").add_modifier(Modifier::BOLD)
            } else {
                theme.get("ui.text.inactive")
            };
            surface.set_stringn(
                bar_area.x + 1,
                bar_area.y,
                badge,
                bar_area.width.saturating_sub(2) as usize,
                tui::ratatui::to_ratatui_style(badge_style.patch(bar_style)),
            );
            if layer == AssistantLayer::Messages {
                let total = self.output.content().len();
                if total > 0 {
                    let position = model
                        .selected_entry_id()
                        .and_then(|id| {
                            self.output
                                .content()
                                .iter()
                                .position(|entry| entry.id == id)
                        })
                        .map(|idx| format!("{}/{}", idx + 1, total))
                        .unwrap_or_else(|| total.to_string());
                    let width = UnicodeWidthStr::width(position.as_str()) as u16;
                    if bar_area.width > width + 2 {
                        surface.set_stringn(
                            bar_area.x + bar_area.width - width - 1,
                            bar_area.y,
                            &position,
                            width as usize,
                            tui::ratatui::to_ratatui_style(
                                theme.get("ui.text.inactive").patch(bar_style),
                            ),
                        );
                    }
                }
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
            let error_style = cx.assistant_theme().get("error");
            if let Some(when) = self.error_marquee.render(error_inset, surface, error_style) {
                schedule_redraw_at(cx.work(), when, cx.redraw.clone());
            }
        }

        // ── Plan area ──
        if plan_rows > 0 && plan_area.height > 0 {
            let theme = cx.assistant_theme();
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
            let line = Spans::from(vec![
                Span::styled(format!("{} ", section.title), tool_name_style),
                Span::styled(
                    format!(
                        "\u{2590}{}{}\u{258c} {done}/{total}",
                        "\u{2588}".repeat(filled),
                        "\u{2591}".repeat(empty)
                    ),
                    plan_progress_style,
                ),
            ]);
            let line = tui::ratatui::to_ratatui_line(&line);
            surface.set_line(plan_inset.x, plan_inset.y, &line, plan_inset.width);
            for (i, item) in section.rows.iter().take(5).enumerate() {
                let style = match item.tone {
                    helix_view::model::AssistantPlanTone::Completed => plan_done_style,
                    helix_view::model::AssistantPlanTone::InProgress => plan_progress_style,
                    helix_view::model::AssistantPlanTone::Failed => plan_failed_style,
                    helix_view::model::AssistantPlanTone::Pending => plan_pending_style,
                };
                let y = plan_inset.y + 1 + i as u16;
                if y < plan_inset.bottom() {
                    let line = Spans::from(vec![
                        Span::styled(item.icon.to_string(), style),
                        Span::styled(item.content.clone(), style),
                    ]);
                    let line = tui::ratatui::to_ratatui_line(&line);
                    surface.set_line(plan_inset.x, y, &line, plan_inset.width);
                }
            }
        }

        if model.has_running_activity() {
            schedule_redraw_at(cx.work(), self.spinner.next_redraw(), cx.redraw.clone());
        }

        if let Some((_, progress)) = self.current_message_accent(&model, cx.assistant_theme()) {
            if progress < 1.0 {
                if let Some(when) = self.message_focus_animation.sample().next_redraw {
                    schedule_redraw_at(cx.work(), when, cx.redraw.clone());
                }
            }
        }

        if context_rows > 0 && context_area.height > 0 {
            let style = cx.assistant_theme().get("ui.text.info");
            surface.set_stringn(
                context_area.x + 1,
                context_area.y,
                context_line.as_deref().unwrap_or_default(),
                context_area.width.saturating_sub(2) as usize,
                tui::ratatui::to_ratatui_style(style),
            );
        }

        if input_area_raw.height > 0 {
            let inset = Rect::new(
                input_area_raw.x + 1,
                input_area_raw.y,
                input_area_raw.width.saturating_sub(2),
                input_area_raw.height,
            );
            let theme = cx.assistant_theme();
            let text_style = theme.get("ui.text");
            let placeholder_style = theme.get("ui.text.inactive");
            let input_border_style = theme
                .try_get("ui.assistant.input.border")
                .unwrap_or_else(|| theme.get("ui.window"));

            // Draw border around input area.
            if inset.width >= 4 && inset.height >= 3 {
                let bw = inset.width as usize;
                let rounded = cx.config().acp.bubble_corners_rounded();
                let (tl, tr, bl, br) = if rounded {
                    ("╭", "╮", "╰", "╯")
                } else {
                    ("┌", "┐", "└", "┘")
                };
                let top = format!("{tl}{}{tr}", "─".repeat(bw.saturating_sub(2)));
                surface.set_stringn(
                    inset.x,
                    inset.y,
                    &top,
                    bw,
                    tui::ratatui::to_ratatui_style(input_border_style),
                );
                for row in 1..inset.height.saturating_sub(1) {
                    let y = inset.y + row;
                    surface.set_stringn(
                        inset.x,
                        y,
                        "│",
                        1,
                        tui::ratatui::to_ratatui_style(input_border_style),
                    );
                    surface.set_stringn(
                        inset.right() - 1,
                        y,
                        "│",
                        1,
                        tui::ratatui::to_ratatui_style(input_border_style),
                    );
                }
                let bot = format!("{bl}{}{br}", "─".repeat(bw.saturating_sub(2)));
                surface.set_stringn(
                    inset.x,
                    inset.y + inset.height - 1,
                    &bot,
                    bw,
                    tui::ratatui::to_ratatui_style(input_border_style),
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
                if let Some(doc) = self
                    .input
                    .doc_id()
                    .and_then(|doc_id| cx.component_document(doc_id))
                {
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
                            tui::ratatui::to_ratatui_style(text_style),
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
                    tui::ratatui::to_ratatui_style(placeholder_style),
                );
            }
        }
        self.render_mention_popup(surface, cx);

        // ── Content area (chat history) ──────────────────────

        if content_area.height == 0 || content_area.width == 0 {
            return;
        }

        self.output.set_area(content_area);

        let theme = cx.assistant_theme();
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
                surface.set_stringn(
                    mx,
                    center_y,
                    msg,
                    content_area.width as usize,
                    tui::ratatui::to_ratatui_style(empty_style),
                );
            }

            return;
        }

        // Render chat content using message list primitives.
        let corners = MessageCorners::Squared;
        let loader = cx.syntax_loader().load();
        let blocks = self.build_blocks(&model, theme, &loader, content_area.width, corners);
        self.render_content(&model, &blocks, content_area, surface);

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
        editor.assistant_entry_markdown(false, entry)
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

    fn jump_selected_subagent(&mut self, editor: &mut Editor) -> bool {
        let Some(info) = editor.selected_assistant_subagent() else {
            return false;
        };
        let can_load = editor
            .active_assistant_caps()
            .is_some_and(|caps| caps.load_session);
        let target = helix_view::assistant::tool::resolve_subagent_jump(
            &info,
            editor.assistant_known_sessions(),
            can_load,
        );
        match target {
            helix_view::assistant::tool::SubagentJumpTarget::Existing {
                thread,
                message_start_index,
                ..
            } => {
                let effects = editor.activate_assistant_thread(thread);
                Self::apply_assistant_effects(editor, effects);
                self.focus_messages(editor);
                if let Some(index) =
                    message_start_index.and_then(|index| usize::try_from(index).ok())
                {
                    self.sync_from_assistant(editor);
                    self.select_message(editor, Some(index));
                }
                editor.set_status("Opened subagent session");
                true
            }
            helix_view::assistant::tool::SubagentJumpTarget::LoadRemote {
                message_start_index,
                ..
            } => {
                let Some(backend) = editor.active_assistant_backend_id() else {
                    editor.set_status("No active assistant backend for subagent session");
                    return true;
                };
                let session_id = info.session_id;
                let scope = editor.active_assistant_scope_or_layout();
                let effects = editor.load_remote_assistant_thread(
                    backend,
                    helix_view::assistant::backend::Remote::new(session_id.clone()),
                    scope,
                    helix_view::editor::Activation::Activate,
                );
                Self::apply_assistant_effects(editor, effects);
                self.focus_messages(editor);
                self.pending_subagent_jump = Some(PendingSubagentJump {
                    session_id,
                    message_start_index: message_start_index
                        .and_then(|index| usize::try_from(index).ok()),
                });
                editor.set_status("Loading subagent session...");
                true
            }
            helix_view::assistant::tool::SubagentJumpTarget::Unsupported => {
                editor.set_status("Assistant backend cannot load subagent sessions");
                true
            }
        }
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
            MouseEventKind::ScrollUp if in_output => {
                let lines = editor.config().scroll_lines.unsigned_abs().max(1);
                let scroll = Scrollable::scroll(&self.output).saturating_sub(lines);
                Scrollable::scroll_to(&mut self.output, scroll);
                self.set_content_scroll(editor, Scrollable::scroll(&self.output));
                EventResult::Consumed(None)
            }
            MouseEventKind::ScrollDown if in_output => {
                let lines = editor.config().scroll_lines.unsigned_abs().max(1);
                let scroll = Scrollable::scroll(&self.output).saturating_add(lines);
                Scrollable::scroll_to(&mut self.output, scroll);
                self.set_content_scroll(editor, Scrollable::scroll(&self.output));
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
            self.refresh_mention_popup(cx.editor);
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

    fn open_mode_config_picker(&mut self, cx: &mut Context) -> bool {
        use crate::runtime::ui::command::{AssistantCommand, ModeConfigPickerItem, UiCommand};

        let Some((thread, mode, config)) = cx.editor.active_assistant_mode_config() else {
            cx.editor.set_status("No active assistant thread");
            return true;
        };

        let mut items = Vec::new();
        if let Some(mode) = mode {
            let selected = match mode.selected() {
                helix_view::assistant::mode::Selected::Current(id) => id,
                helix_view::assistant::mode::Selected::Pending { next, .. } => next,
            };
            items.extend(mode.items().map(|item| ModeConfigPickerItem::Mode {
                id: item.id.clone(),
                name: item.name.clone(),
                current: &item.id == selected,
            }));
        }
        for option in config.items() {
            let selected = match &option.selected {
                helix_view::assistant::config::Selected::Current(id) => id,
                helix_view::assistant::config::Selected::Pending { next, .. } => next,
            };
            items.extend(
                option
                    .values
                    .iter()
                    .map(|value| ModeConfigPickerItem::Config {
                        option: option.id.clone(),
                        value: value.id.clone(),
                        name: option.name.clone(),
                        value_label: value.label.clone(),
                        category: option.category.clone(),
                        current: &value.id == selected,
                    }),
            );
        }

        cx.ingress.ui(UiCommand::Assistant(
            AssistantCommand::PushModeConfigPicker { thread, items },
        ));
        true
    }

    fn cancel_active_run(&mut self, cx: &mut Context) {
        if let Some(effects) = cx.editor.cancel_active_assistant_thread() {
            Self::apply_assistant_effects(cx.editor, effects);
        }
        cx.editor.set_status("Canceling agent...");
    }

    fn enter_pending_elicitation(&mut self, editor: &mut Editor) -> bool {
        let Some(elicitation) = Self::pending_elicitation(editor) else {
            return false;
        };
        if self.sync_elicitation_form(&elicitation) {
            self.set_focus(editor, helix_view::assistant::thread::Focus::Messages);
            return true;
        }
        false
    }

    fn execute_message_action(&mut self, action: AssistantAction, cx: &mut Context) -> EventResult {
        let model = Self::assistant_model(cx.editor);
        match action {
            AssistantAction::FocusInput => {
                self.focus_input_region(cx.editor);
            }
            AssistantAction::FocusInputInsert => {
                self.focus_input_region(cx.editor);
                self.input.enter_insert_mode("insert_mode".into());
            }
            AssistantAction::Primary => {
                if self.enter_pending_elicitation(cx.editor) {
                    return EventResult::Consumed(None);
                }
                if Self::active_auth_methods(cx.editor).is_some() {
                    self.auth_transient = true;
                    self.set_focus(cx.editor, helix_view::assistant::thread::Focus::Messages);
                    return EventResult::Consumed(None);
                }
                if self.jump_selected_subagent(cx.editor) {
                    return EventResult::Consumed(None);
                }
                if !self.open_selected_message_details(cx.editor, Action::Replace) {
                    cx.editor.set_status("No assistant entry selected");
                }
            }
            AssistantAction::ToggleFold => {
                self.toggle_selected_message_fold(cx.editor);
            }
            AssistantAction::Yank => {
                if !self.yank_pending_elicitation_url(cx.editor) {
                    self.yank_selected_message(cx.editor);
                }
            }
            AssistantAction::FollowOrJump => {
                if !self.jump_selected_subagent(cx.editor) {
                    match cx.editor.toggle_active_assistant_follow() {
                        Ok((status, effects)) => {
                            Self::apply_assistant_effects(cx.editor, effects);
                            cx.editor.set_status(status);
                        }
                        Err(err) => cx.editor.set_status(err.to_string()),
                    }
                }
            }
            AssistantAction::Previous => {
                self.select_prev_message(cx.editor);
            }
            AssistantAction::Next => {
                self.select_next_message(cx.editor);
            }
            AssistantAction::First => {
                self.select_first_message(cx.editor);
            }
            AssistantAction::FirstPending => {
                if self.pending_message_g {
                    self.pending_message_g = false;
                    self.select_first_message(cx.editor);
                } else {
                    self.pending_message_g = true;
                }
            }
            AssistantAction::Last => {
                self.select_last_message(cx.editor);
                self.output.scroll_to_end();
                self.set_content_scroll(cx.editor, self.output.max_scroll());
            }
            AssistantAction::PagePrevious => {
                self.select_prev_message_page(cx.editor);
            }
            AssistantAction::PageNext => {
                self.select_next_message_page(cx.editor);
            }
            AssistantAction::Retry => match cx.editor.retry_active_assistant_prompt() {
                Ok(effects) => {
                    Self::apply_assistant_effects(cx.editor, effects);
                    cx.editor.set_status("Retrying assistant prompt...");
                }
                Err(err) => cx.editor.set_status(err.to_string()),
            },
            AssistantAction::ToggleReviewMode => {
                match cx.editor.toggle_active_assistant_review_mode() {
                    Ok((status, effects)) => {
                        Self::apply_assistant_effects(cx.editor, effects);
                        cx.editor.set_status(status);
                    }
                    Err(err) => cx.editor.set_status(err.to_string()),
                }
            }
            AssistantAction::AcceptReview => match cx
                .editor
                .resolve_selected_assistant_review(helix_view::assistant::review::Decision::Accept)
            {
                Ok(effects) => {
                    Self::apply_assistant_effects(cx.editor, effects);
                    cx.editor.set_status("Accepted assistant change");
                }
                Err(err) => cx.editor.set_status(err.to_string()),
            },
            AssistantAction::AcceptAllReview => {
                match cx.editor.resolve_all_active_assistant_review(
                    helix_view::assistant::review::Decision::Accept,
                ) {
                    Ok(effects) => {
                        Self::apply_assistant_effects(cx.editor, effects);
                        cx.editor.set_status("Accepted all assistant changes");
                    }
                    Err(err) => cx.editor.set_status(err.to_string()),
                }
            }
            AssistantAction::RejectReview => match cx
                .editor
                .resolve_selected_assistant_review(helix_view::assistant::review::Decision::Reject)
            {
                Ok(effects) => {
                    Self::apply_assistant_effects(cx.editor, effects);
                    cx.editor.set_status("Rejected assistant change");
                }
                Err(err) => cx.editor.set_status(err.to_string()),
            },
            AssistantAction::RejectAllReview => {
                match cx.editor.resolve_all_active_assistant_review(
                    helix_view::assistant::review::Decision::Reject,
                ) {
                    Ok(effects) => {
                        Self::apply_assistant_effects(cx.editor, effects);
                        cx.editor.set_status("Rejected all assistant changes");
                    }
                    Err(err) => cx.editor.set_status(err.to_string()),
                }
            }
            AssistantAction::OpenConfig => {
                self.open_mode_config_picker(cx);
            }
            AssistantAction::CancelRun => self.cancel_active_run(cx),
            AssistantAction::ToggleHelp => self.toggle_help(cx.editor),
            _ => {}
        }

        if !matches!(action, AssistantAction::FirstPending) {
            self.pending_message_g = false;
        }
        if let Some(index) = self.selected_message(cx.editor) {
            let viewport_height = self.output.area().height as usize;
            let mut cursor = MessageCursor::new(Some(index), model.content_scroll());
            cursor.sync(&self.chat_layout, viewport_height);
            self.set_content_scroll(cx.editor, cursor.scroll());
        }
        EventResult::Consumed(None)
    }

    fn execute_transient_action(
        &mut self,
        layer: AssistantLayer,
        action: AssistantAction,
        cx: &mut Context,
    ) -> EventResult {
        match action {
            AssistantAction::TransientPop => {
                self.elicitation_form = None;
                self.auth_transient = false;
                self.focus_messages(cx.editor);
            }
            AssistantAction::TransientSubmit if layer == AssistantLayer::Elicitation => {
                if self.accept_pending_elicitation(cx.editor) {
                    cx.editor.set_status("Submitted assistant request");
                }
            }
            AssistantAction::TransientSubmit if layer == AssistantLayer::Auth => {
                if self.accept_auth_method(cx.editor) {
                    self.auth_transient = false;
                }
            }
            AssistantAction::TransientNext if layer == AssistantLayer::Elicitation => {
                if let Some(form) = &mut self.elicitation_form {
                    form.focus_next();
                }
            }
            AssistantAction::TransientPrevious if layer == AssistantLayer::Elicitation => {
                if let Some(form) = &mut self.elicitation_form {
                    form.focus_prev();
                }
            }
            AssistantAction::TransientNext if layer == AssistantLayer::Auth => {
                self.select_auth_method(cx.editor, 1);
            }
            AssistantAction::TransientPrevious if layer == AssistantLayer::Auth => {
                self.select_auth_method(cx.editor, -1);
            }
            AssistantAction::TransientBackspace => {
                if let Some(form) = &mut self.elicitation_form {
                    form.backspace();
                }
            }
            AssistantAction::TransientActivatePrevious | AssistantAction::TransientActivateNext
                if layer == AssistantLayer::Elicitation =>
            {
                if let (Some(elicitation), Some(form)) = (
                    Self::pending_elicitation(cx.editor),
                    &mut self.elicitation_form,
                ) {
                    if let helix_view::assistant::thread::ElicitationMode::Form { fields, .. } =
                        &elicitation.mode
                    {
                        let delta = if action == AssistantAction::TransientActivatePrevious {
                            -1
                        } else {
                            1
                        };
                        form.activate_focused(fields, delta);
                    }
                }
            }
            AssistantAction::CancelRun => self.cancel_active_run(cx),
            AssistantAction::ToggleHelp => self.toggle_help(cx.editor),
            _ => {}
        }
        EventResult::Consumed(None)
    }

    fn execute_input_binding(
        &mut self,
        action: AssistantAction,
        cx: &mut Context,
    ) -> Option<EventResult> {
        match action {
            AssistantAction::FocusMessages => {
                self.focus_messages(cx.editor);
                Some(EventResult::Consumed(None))
            }
            AssistantAction::SendPrompt => {
                if self.input.mode() == Mode::Insert {
                    return None;
                }
                self.send_current_prompt(cx);
                Some(EventResult::Consumed(None))
            }
            AssistantAction::InputEscape => {
                if self.mention.active {
                    self.mention.active = false;
                    return Some(EventResult::Consumed(None));
                }
                if self.input.mode() == Mode::Insert {
                    self.input.exit_insert_mode();
                    Some(EventResult::Consumed(None))
                } else {
                    None
                }
            }
            AssistantAction::InsertInputChar(ch) => {
                if self.input.mode() != Mode::Insert {
                    self.input.enter_insert_mode("insert_mode".into());
                }
                self.insert_char_into_input(ch, cx.editor);
                self.sync_draft_to_assistant(cx.editor);
                self.refresh_mention_popup(cx.editor);
                Some(EventResult::Consumed(None))
            }
            AssistantAction::OpenConfig => {
                self.open_mode_config_picker(cx);
                Some(EventResult::Consumed(None))
            }
            AssistantAction::CancelRun => {
                self.cancel_active_run(cx);
                Some(EventResult::Consumed(None))
            }
            _ => None,
        }
    }
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
            editor.frontend_mut().focused_modal_input = self.input.input_state();
        }
        self.sync_from_assistant(editor);
        self.sync_to_model(editor);
    }

    fn render(&mut self, area: Rect, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        self.render_surface(area, surface, cx);
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let key = match event {
            Event::Key(key) => *key,
            Event::Mouse(event) => return self.handle_mouse_event(event, cx.editor),
            _ => return EventResult::Ignored(None),
        };

        // When unfocused, pass all events through to the editor.
        if !self.focused {
            if self.shown_help.take().is_some() {
                cx.editor.autoinfo = None;
            }
            return EventResult::Ignored(None);
        }

        self.error_marquee.touch();

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

        let layer = self.active_layer(cx.editor);
        if let Some(binding) = Self::binding_for_key(layer, &key) {
            match layer {
                AssistantLayer::Input => {
                    if let Some(result) = self.execute_input_binding(binding.action, cx) {
                        self.sync_help(cx.editor);
                        return result;
                    }
                }
                AssistantLayer::Messages => {
                    let result = self.execute_message_action(binding.action, cx);
                    self.sync_help(cx.editor);
                    return result;
                }
                AssistantLayer::Elicitation | AssistantLayer::Auth => {
                    let result = self.execute_transient_action(layer, binding.action, cx);
                    self.sync_help(cx.editor);
                    return result;
                }
            }
        }
        self.sync_help(cx.editor);

        if layer == AssistantLayer::Elicitation {
            if let Some(form) = &mut self.elicitation_form {
                if let KeyCode::Char(ch) = key.code {
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                        form.insert_char(ch);
                        return EventResult::Consumed(None);
                    }
                }
            }
            return EventResult::Consumed(None);
        }

        let model = Self::assistant_model(cx.editor);

        if layer == AssistantLayer::Input
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

        // ── Mode transitions ──
        // The component modal keymaps filter out Frontend commands (insert_mode,
        // command_mode, etc.), so we handle mode switching explicitly here.
        if self.input.mode() == Mode::Insert {
            if self.mention.active {
                match key {
                    KeyEvent {
                        code: KeyCode::Esc,
                        modifiers,
                        ..
                    } if modifiers.is_empty() => {
                        self.mention.active = false;
                        return EventResult::Consumed(None);
                    }
                    KeyEvent {
                        code: KeyCode::Up,
                        modifiers,
                        ..
                    } if modifiers.is_empty() => {
                        self.mention.selected = self.mention.selected.saturating_sub(1);
                        return EventResult::Consumed(None);
                    }
                    KeyEvent {
                        code: KeyCode::Char('p'),
                        modifiers,
                        ..
                    } if modifiers == KeyModifiers::CONTROL => {
                        self.mention.selected = self.mention.selected.saturating_sub(1);
                        return EventResult::Consumed(None);
                    }
                    KeyEvent {
                        code: KeyCode::Down,
                        modifiers,
                        ..
                    } if modifiers.is_empty() => {
                        if self.mention.selected + 1 < self.mention.candidates.len() {
                            self.mention.selected += 1;
                        }
                        return EventResult::Consumed(None);
                    }
                    KeyEvent {
                        code: KeyCode::Char('n'),
                        modifiers,
                        ..
                    } if modifiers == KeyModifiers::CONTROL => {
                        if self.mention.selected + 1 < self.mention.candidates.len() {
                            self.mention.selected += 1;
                        }
                        return EventResult::Consumed(None);
                    }
                    KeyEvent {
                        code: KeyCode::Enter | KeyCode::Tab,
                        modifiers,
                        ..
                    } if modifiers.is_empty() => {
                        if self.accept_selected_mention(cx.editor) {
                            return EventResult::Consumed(None);
                        }
                    }
                    _ => {}
                }
            }
            // Escape in insert mode → back to normal.
            if matches!(key.code, KeyCode::Esc) && key.modifiers.is_empty() {
                if self.cancel_pending_elicitation(cx.editor) {
                    cx.editor.set_status("Canceled assistant request");
                    return EventResult::Consumed(None);
                }
                self.input.exit_insert_mode();
                return EventResult::Consumed(None);
            }
            // Tab → insert tab character (not bound in component keymaps).
            if matches!(key.code, KeyCode::Tab) && key.modifiers.is_empty() {
                self.insert_char_into_input('\t', cx.editor);
                self.sync_draft_to_assistant(cx.editor);
                self.refresh_mention_popup(cx.editor);
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
                self.sync_draft_to_assistant(cx.editor);
                self.refresh_mention_popup(cx.editor);
                return EventResult::Consumed(None);
            }
        } else {
            // Normal mode key handling.
            if key.modifiers.is_empty() {
                match key.code {
                    // Enter → send prompt.
                    KeyCode::Enter => {
                        if self.accept_pending_elicitation(cx.editor) {
                            cx.editor.set_status("Submitted assistant request");
                            return EventResult::Consumed(None);
                        }
                        self.send_current_prompt(cx);
                        return EventResult::Consumed(None);
                    }
                    // i/a/I/A → enter insert mode with Helix-matching
                    // cursor placement. The selection transforms used to
                    // be hand-rolled here, one match arm per key — which
                    // was both verbose and prone to drift from the editor.
                    // `EditRegion::enter_insert_at` now owns the canonical
                    // semantics; this site just names the entry.
                    KeyCode::Char('i') => {
                        self.input.enter_insert_at(
                            cx.editor,
                            helix_view::edit_region::InsertEntry::AtCurrent,
                        );
                        return EventResult::Consumed(None);
                    }
                    KeyCode::Char('a') => {
                        self.input.enter_insert_at(
                            cx.editor,
                            helix_view::edit_region::InsertEntry::Append,
                        );
                        return EventResult::Consumed(None);
                    }
                    KeyCode::Char('I') => {
                        self.input.enter_insert_at(
                            cx.editor,
                            helix_view::edit_region::InsertEntry::AtLineStart,
                        );
                        return EventResult::Consumed(None);
                    }
                    KeyCode::Char('A') => {
                        self.input.enter_insert_at(
                            cx.editor,
                            helix_view::edit_region::InsertEntry::AtLineEnd,
                        );
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

    fn id(&self) -> Option<&str> {
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
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::Arc;

    fn key_event_for_binding(key: BindingKey) -> KeyEvent {
        let code = match key.code {
            BindingCode::Char(ch) => KeyCode::Char(ch),
            BindingCode::Enter => KeyCode::Enter,
            BindingCode::Esc => KeyCode::Esc,
            BindingCode::Tab => KeyCode::Tab,
            BindingCode::Backspace => KeyCode::Backspace,
            BindingCode::Up => KeyCode::Up,
            BindingCode::Down => KeyCode::Down,
            BindingCode::Left => KeyCode::Left,
            BindingCode::Right => KeyCode::Right,
            BindingCode::Home => KeyCode::Home,
            BindingCode::End => KeyCode::End,
            BindingCode::PageUp => KeyCode::PageUp,
            BindingCode::PageDown => KeyCode::PageDown,
        };
        KeyEvent {
            code,
            modifiers: key.modifiers,
        }
    }

    #[test]
    fn assistant_bindings_have_no_layer_collisions() {
        for layer in [
            AssistantLayer::Input,
            AssistantLayer::Messages,
            AssistantLayer::Elicitation,
            AssistantLayer::Auth,
        ] {
            let mut keys = HashSet::new();
            for binding in AssistantPanel::bindings_for_layer(layer) {
                assert!(
                    keys.insert(binding.key),
                    "duplicate assistant binding in {layer:?}: {:?}",
                    binding.key
                );
            }
        }
    }

    #[test]
    fn assistant_hint_bindings_dispatch_to_same_action() {
        for layer in [
            AssistantLayer::Input,
            AssistantLayer::Messages,
            AssistantLayer::Elicitation,
            AssistantLayer::Auth,
        ] {
            for binding in AssistantPanel::bindings_for_layer(layer)
                .iter()
                .filter(|binding| binding.hint.is_some())
            {
                let key = key_event_for_binding(binding.key);
                let dispatched = AssistantPanel::binding_for_key(layer, &key)
                    .expect("hinted binding must dispatch");
                assert_eq!(dispatched.action, binding.action);
            }
        }
    }

    #[test]
    fn help_toggle_bound_where_typing_cannot_shadow_it() {
        let has_toggle = |layer| {
            AssistantPanel::bindings_for_layer(layer)
                .iter()
                .any(|binding| binding.action == AssistantAction::ToggleHelp)
        };
        assert!(has_toggle(AssistantLayer::Messages));
        assert!(has_toggle(AssistantLayer::Auth));
        // `?` must stay typable in form fields and the input box.
        assert!(!has_toggle(AssistantLayer::Elicitation));
        assert!(!has_toggle(AssistantLayer::Input));
    }

    #[test]
    fn assistant_escape_pops_one_layer() {
        assert_eq!(
            assistant_escape_target(AssistantLayer::Elicitation, false),
            Some(AssistantLayer::Messages)
        );
        assert_eq!(
            assistant_escape_target(AssistantLayer::Auth, false),
            Some(AssistantLayer::Messages)
        );
        assert_eq!(
            assistant_escape_target(AssistantLayer::Messages, false),
            Some(AssistantLayer::Input)
        );
        assert_eq!(
            assistant_escape_target(AssistantLayer::Input, true),
            Some(AssistantLayer::Input)
        );
        assert_eq!(assistant_escape_target(AssistantLayer::Input, false), None);
    }

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
        let thread = editor.create_local_assistant_thread(
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
                ChatEntryKind::Thought(text) => {
                    helix_view::assistant::thread::EntryKind::Thought { text }
                }
                ChatEntryKind::ToolCall {
                    id, name, status, ..
                } => helix_view::assistant::thread::EntryKind::ToolCall(
                    helix_view::assistant::tool::Call {
                        id: helix_view::assistant::tool::Id::new(id),
                        name,
                        state: match status.as_str() {
                            "running" => helix_view::assistant::tool::State::Running,
                            "completed" | "done" => helix_view::assistant::tool::State::Completed,
                            "failed" => {
                                helix_view::assistant::tool::State::Failed { message: None }
                            }
                            "cancelled" => helix_view::assistant::tool::State::Canceled,
                            _ => helix_view::assistant::tool::State::Pending,
                        },
                        output: String::new(),
                        sandbox: None,
                        subagent: None,
                    },
                ),
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
                                    review: None,
                                };
                                files
                            ],
                        },
                    )
                }
                ChatEntryKind::ReviewSummary { files, .. } => {
                    helix_view::assistant::thread::EntryKind::ChangeSummary(
                        helix_view::assistant::change::Summary {
                            files: files
                                .into_iter()
                                .map(|(path, diff, status)| helix_view::assistant::change::File {
                                    path: path.clone(),
                                    hunks: Vec::new(),
                                    review: Some(helix_view::assistant::review::File {
                                        path,
                                        before: String::new(),
                                        after: String::new(),
                                        diff,
                                        status,
                                    }),
                                })
                                .collect(),
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
        let entry = editor
            .assistant
            .panel(false)
            .entry_id_at(index)
            .expect("entry");
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

            let model = editor.assistant_model(false);
            let loader = editor.syn_loader.load();
            let expanded_blocks = panel.build_blocks(
                &model,
                editor.assistant_theme(),
                &loader,
                30,
                MessageCorners::Squared,
            );
            let expanded_height = expanded_blocks[0].height(true);

            assert!(panel.toggle_selected_message_fold(&mut editor));

            let model = editor.assistant_model(false);
            let collapsed_blocks = panel.build_blocks(
                &model,
                editor.assistant_theme(),
                &loader,
                30,
                MessageCorners::Squared,
            );
            let collapsed_height = collapsed_blocks[0].height(true);

            assert!(expanded_height > collapsed_height);
            let entry = editor.assistant_entry_id_at(false, 0).expect("entry");
            assert!(editor.is_assistant_entry_folded(false, entry));
        });
    }
}
