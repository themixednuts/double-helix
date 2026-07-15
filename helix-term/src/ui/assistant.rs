use crate::component_traits;
use crate::compositor::{Component, Context, Event, EventResult, PostAction, RenderContext};
use crate::ui::animation::{
    Animation, AnimationDirection, AnimationFillMode, AnimationIterationCount, AnimationSpec,
    AnimationTimingFunction,
};
use crate::widgets::{Marquee, MarqueeFrame};
use crate::widgets::{Message, MessageCorners, MessageCursor, MessageListState, Spinner};
use helix_core::unicode::width::{UnicodeWidthChar, UnicodeWidthStr};
use helix_core::Position;
use helix_view::content_region::ContentRegion;
use helix_view::document::Mode;
use helix_view::editor::Action;
use helix_view::graphics::{CursorKind, Rect, Style as GraphicsStyle};
use helix_view::input::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use helix_view::theme::{Modifier, Style};
use helix_view::traits::{Bounded, Focusable, Identified, Modal, Scrollable};
use helix_view::Editor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ERROR_MARQUEE_FRAME: helix_runtime::FrameSource =
    helix_runtime::FrameSource::new("assistant.error-marquee");
const ACTIVITY_SPINNER_FRAME: helix_runtime::FrameSource =
    helix_runtime::FrameSource::new("assistant.activity-spinner");
const MESSAGE_FOCUS_FRAME: helix_runtime::FrameSource =
    helix_runtime::FrameSource::new("assistant.message-focus");
use tui::text::{Span, Spans};

use crate::ui::markdown::MarkdownLineStyles;

mod markdown_service;
use markdown_service::{
    MarkdownService, RenderStyle as MarkdownRenderStyle, RequestKey as MarkdownRequestKey,
};
mod presentation_service;
use presentation_service::{
    PresentationService, RequestKey as PresentationRequestKey, Snapshot as PresentationSnapshot,
    Source as PresentationSource,
};

pub const ID: &str = "assistant-panel";

// ---------------------------------------------------------------------------
// Chat entries
// ---------------------------------------------------------------------------

type ChatEntry = helix_view::model::AssistantEntry;
#[cfg(test)]
type ChatEntryKind = helix_view::model::AssistantEntryKind;

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputVisualLine {
    text: String,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct InputViewportLayout {
    visible_lines: Arc<[String]>,
    cursor_row: usize,
    cursor_column: usize,
    placeholder: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    inspected_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputLayoutKey {
    document_version: i32,
    cursor: usize,
    width: usize,
    height: usize,
}

#[derive(Debug, Default)]
struct InputLayoutCache {
    key: Option<InputLayoutKey>,
    layout: Option<Arc<InputViewportLayout>>,
}

impl InputLayoutCache {
    fn layout(
        &mut self,
        key: InputLayoutKey,
        text: helix_core::RopeSlice<'_>,
    ) -> Arc<InputViewportLayout> {
        if self.key == Some(key) {
            if let Some(layout) = &self.layout {
                return Arc::clone(layout);
            }
        }
        let layout = Arc::new(layout_input_viewport(
            text, key.cursor, key.width, key.height,
        ));
        self.key = Some(key);
        self.layout = Some(Arc::clone(&layout));
        layout
    }
}

fn layout_input_viewport(
    text: helix_core::RopeSlice<'_>,
    cursor: usize,
    width: usize,
    height: usize,
) -> InputViewportLayout {
    let width = width.max(1);
    let height = height.max(1);
    let cursor = cursor.min(text.len_chars());
    let cursor_line = text.char_to_line(cursor);
    let mut inspected_lines = 1;
    let current_line_start = text.line_to_char(cursor_line);
    let mut visible = std::collections::VecDeque::with_capacity(height);
    let mut current_after = Vec::with_capacity(height);
    let mut found_cursor = false;
    let mut cursor_visual_start = current_line_start;
    for_each_input_rope_row(
        text.line(cursor_line),
        current_line_start,
        width,
        Some(cursor),
        |row, is_last| {
            if !found_cursor
                && ((cursor >= row.start && cursor < row.end) || (is_last && cursor == row.end))
            {
                found_cursor = true;
                cursor_visual_start = row.start;
                if visible.len() == height {
                    visible.pop_front();
                }
                visible.push_back(row);
                return true;
            }
            if found_cursor {
                if current_after.len() == height {
                    return false;
                }
                current_after.push(row);
            } else {
                if visible.len() == height {
                    visible.pop_front();
                }
                visible.push_back(row);
            }
            true
        },
    );
    debug_assert!(found_cursor, "cursor must belong to an input visual row");

    let mut previous_line = cursor_line;
    while visible.len() < height && previous_line > 0 {
        previous_line -= 1;
        inspected_lines += 1;
        let line_start = text.line_to_char(previous_line);
        let remaining = height - visible.len();
        let mut tail = std::collections::VecDeque::with_capacity(remaining);
        for_each_input_rope_row(
            text.line(previous_line),
            line_start,
            width,
            None,
            |row, _| {
                if tail.len() == remaining {
                    tail.pop_front();
                }
                tail.push_back(row);
                true
            },
        );
        while let Some(row) = tail.pop_back() {
            visible.push_front(row);
        }
    }

    if previous_line == 0 && visible.len() < height {
        for row in current_after {
            if visible.len() == height {
                break;
            }
            visible.push_back(row);
        }
        let mut next_line = cursor_line + 1;
        while visible.len() < height && next_line < text.len_lines() {
            inspected_lines += 1;
            let line_start = text.line_to_char(next_line);
            for_each_input_rope_row(text.line(next_line), line_start, width, None, |row, _| {
                if visible.len() == height {
                    return false;
                }
                visible.push_back(row);
                true
            });
            next_line += 1;
        }
    }

    let cursor_row = visible
        .iter()
        .position(|row| row.start == cursor_visual_start)
        .unwrap_or_default();
    let cursor_start = visible
        .get(cursor_row)
        .map_or(cursor, |row| row.start.min(cursor));
    let cursor_column = text
        .slice(cursor_start..cursor)
        .chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum::<usize>()
        .min(width.saturating_sub(1));

    InputViewportLayout {
        visible_lines: visible
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .into(),
        cursor_row,
        cursor_column,
        placeholder: text.len_chars() == text.len_lines().saturating_sub(1),
        inspected_lines,
    }
}

fn for_each_input_rope_row(
    line: helix_core::RopeSlice<'_>,
    line_start: usize,
    width: usize,
    trailing_cursor: Option<usize>,
    mut visit: impl FnMut(InputVisualLine, bool) -> bool,
) {
    let mut content_len = line.len_chars();
    if content_len > 0 && line.char(content_len - 1) == '\n' {
        content_len -= 1;
        if content_len > 0 && line.char(content_len - 1) == '\r' {
            content_len -= 1;
        }
    }
    if content_len == 0 {
        visit(
            InputVisualLine {
                text: String::new(),
                start: line_start,
                end: line_start,
            },
            true,
        );
        return;
    }

    let mut row_text = String::new();
    let mut row_start = line_start;
    let mut row_width = 0usize;
    for (offset, character) in line.slice(..content_len).chars().enumerate() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if !row_text.is_empty() && row_width.saturating_add(character_width) > width {
            if !visit(
                InputVisualLine {
                    text: std::mem::take(&mut row_text),
                    start: row_start,
                    end: line_start + offset,
                },
                false,
            ) {
                return;
            }
            row_start = line_start + offset;
            row_width = 0;
        }
        row_text.push(character);
        row_width = row_width.saturating_add(character_width);
    }
    let line_end = line_start + content_len;
    if row_width >= width && trailing_cursor == Some(line_end) {
        if !visit(
            InputVisualLine {
                text: row_text,
                start: row_start,
                end: line_end,
            },
            false,
        ) {
            return;
        }
        visit(
            InputVisualLine {
                text: String::new(),
                start: line_end,
                end: line_end,
            },
            true,
        );
    } else {
        visit(
            InputVisualLine {
                text: row_text,
                start: row_start,
                end: line_end,
            },
            true,
        );
    }
}

// ---------------------------------------------------------------------------
// Assistant Panel
// ---------------------------------------------------------------------------

pub struct AssistantPanel {
    focused: bool,
    /// Read-only chat/output area with component-owned scroll + viewport state.
    output: ContentRegion<Arc<[ChatEntry]>>,
    /// Metadata snapshot captured during component sync. Entries live in `output`.
    render_model: Arc<helix_view::model::AssistantModel>,
    /// Editable input area backed by a component-owned document.
    input: helix_view::edit_region::EditRegion,
    /// Last assistant/backend error message, shown below the status line.
    panel_error: Option<String>,
    /// Marquee for long error text (scroll, hold, reset, repeat; pauses after inactivity).
    error_marquee: Marquee,
    /// Last input cursor screen position (set during render, read by cursor()).
    last_input_cursor: Option<(u16, u16)>,
    input_layout: InputLayoutCache,
    /// Model panel ID, set on first sync.
    model_panel_id: Option<helix_view::model::PanelId>,
    /// Latest chat-thread layout information for future chat-entry navigation.
    chat_layout: MessageListState,
    /// Shared spinner primitive for lightweight running-status animation.
    spinner: Spinner,
    /// Selection animation that replays when message focus moves back onto a row.
    message_focus_animation: Animation,
    pending_message_g: bool,
    markdown: Option<MarkdownService>,
    presentation: Option<PresentationService>,
    completed_presentation: Option<Arc<PresentationSnapshot>>,
    visible_presentation: Option<Arc<PresentationSnapshot>>,
    mention: MentionPopup,
    elicitation_form: Option<helix_view::assistant::elicitation::FormState>,
    auth_selected: usize,
    auth_transient: bool,
    pending_subagent_jump: Option<PendingSubagentJump>,
    editing: Option<EditState>,
    /// Layer whose key help is currently shown via `editor.autoinfo`.
    shown_help: Option<AssistantLayer>,
}

#[derive(Debug, Clone)]
struct EditState {
    target: helix_view::assistant::thread::EntryId,
    previous_draft: String,
    previous_focus: helix_view::assistant::thread::Focus,
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
    RateUp,
    RateDown,
    EditNote,
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
    EditMessage,
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

const NORMAL_INPUT_BINDINGS: &[AssistantBinding] = &[AssistantBinding::new(
    BindingKey::new(BindingCode::Char('?')),
    AssistantAction::ToggleHelp,
    Some(("?", "help", 10)),
)];

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
        BindingKey::new(BindingCode::Char('e')),
        AssistantAction::EditMessage,
        Some(("e", "edit", 125)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('R')),
        AssistantAction::ToggleReviewMode,
        Some(("R", "review", 110)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('+')),
        AssistantAction::RateUp,
        Some(("+/-", "rate", 105)),
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('-')),
        AssistantAction::RateDown,
        None,
    ),
    AssistantBinding::new(
        BindingKey::new(BindingCode::Char('n')),
        AssistantAction::EditNote,
        Some(("n", "note", 102)),
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

#[derive(Clone, Copy)]
struct AssistantRenderAreas {
    border: Rect,
    inner: Rect,
    header: Rect,
    content_raw: Rect,
    content: Rect,
    plan: Rect,
    context: Rect,
    input_raw: Rect,
    status: Rect,
    error: Rect,
}

struct AssistantInputSnapshot {
    border_area: Rect,
    text_area: Rect,
    visible_lines: Arc<[String]>,
    placeholder: bool,
    rounded: bool,
}

struct AssistantMentionSnapshot {
    area: Rect,
    candidates: Arc<[MentionCandidate]>,
    selected: usize,
}

struct AssistantRenderSnapshot {
    areas: AssistantRenderAreas,
    theme: Arc<helix_view::Theme>,
    model: Arc<helix_view::model::AssistantModel>,
    focused: bool,
    badge: Arc<str>,
    status: Arc<str>,
    position: Option<Arc<str>>,
    error: Option<MarqueeFrame>,
    input: AssistantInputSnapshot,
    mention: Option<AssistantMentionSnapshot>,
    blocks: Arc<[Message]>,
    chat_layout: MessageListState,
    presentation_pending: bool,
    animated_prefix: Option<(usize, Arc<str>)>,
    selected_accent: Option<(GraphicsStyle, f32)>,
}

impl AssistantRenderSnapshot {
    fn paint(
        self,
        surface: &mut crate::render::CellSurface,
        cancellation: &crate::render::RenderCancellation,
    ) {
        if cancellation.is_cancelled() {
            return;
        }
        self.paint_chrome(surface);
        if cancellation.is_cancelled() {
            return;
        }
        self.paint_plan_and_context(surface);
        self.paint_input(surface);
        self.paint_mention(surface);
        if cancellation.is_cancelled() {
            return;
        }
        self.paint_messages(surface);
    }

    fn paint_chrome(&self, surface: &mut crate::render::CellSurface) {
        let background = self.theme.get("ui.background");
        let inner = tui::ratatui::to_ratatui_rect(self.areas.inner);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, inner, surface);
        surface.set_style(inner, tui::ratatui::to_ratatui_style(background));
        crate::widgets::vdivider(surface, self.areas.border, self.theme.get("ui.window"));

        let bar_style = if self.focused {
            self.theme.get("ui.statusline")
        } else {
            self.theme.get("ui.statusline.inactive")
        };
        for area in [self.areas.header, self.areas.status] {
            surface.set_style(
                tui::ratatui::to_ratatui_rect(area),
                tui::ratatui::to_ratatui_style(bar_style),
            );
        }

        let header = self.model.header();
        let trailing_width = header
            .trailing
            .iter()
            .enumerate()
            .map(|(index, item)| {
                UnicodeWidthStr::width(item.label.as_str()) + usize::from(index > 0) * 2
            })
            .sum::<usize>() as u16;
        if !header.trailing.is_empty() {
            let mut x = self
                .areas
                .header
                .x
                .saturating_add(self.areas.header.width.saturating_sub(trailing_width));
            for (index, item) in header.trailing.iter().enumerate() {
                if index > 0 {
                    surface.set_stringn(
                        x,
                        self.areas.header.y,
                        "  ",
                        2,
                        tui::ratatui::to_ratatui_style(bar_style),
                    );
                    x = x.saturating_add(2);
                }
                let style = AssistantPanel::header_item_style(&self.theme, bar_style, item.tone);
                let width = UnicodeWidthStr::width(item.label.as_str());
                surface.set_stringn(
                    x,
                    self.areas.header.y,
                    &item.label,
                    width,
                    tui::ratatui::to_ratatui_style(style),
                );
                x = x.saturating_add(width as u16);
            }
        }
        let mut x = self.areas.header.x.saturating_add(1);
        let right_edge = if header.trailing.is_empty() {
            self.areas.header.right()
        } else {
            self.areas
                .header
                .right()
                .saturating_sub(trailing_width.saturating_add(1))
        };
        for item in &header.leading {
            if x >= right_edge {
                break;
            }
            let style = AssistantPanel::header_item_style(&self.theme, bar_style, item.tone);
            let width = UnicodeWidthStr::width(item.label.as_str()) as u16;
            surface.set_stringn(
                x,
                self.areas.header.y,
                &item.label,
                right_edge.saturating_sub(x) as usize,
                tui::ratatui::to_ratatui_style(style),
            );
            x = x.saturating_add(width.min(right_edge.saturating_sub(x)).saturating_add(1));
        }

        let badge_style = if self.focused {
            self.theme.get("ui.text.focus").add_modifier(Modifier::BOLD)
        } else {
            self.theme.get("ui.text.inactive")
        };
        surface.set_stringn(
            self.areas.status.x.saturating_add(1),
            self.areas.status.y,
            &self.badge,
            UnicodeWidthStr::width(self.badge.as_ref())
                .min(self.areas.status.width.saturating_sub(2) as usize),
            tui::ratatui::to_ratatui_style(badge_style.patch(bar_style)),
        );
        let position_width = self
            .position
            .as_deref()
            .map(UnicodeWidthStr::width)
            .unwrap_or_default() as u16;
        let right_reserved = position_width.saturating_add(u16::from(position_width > 0) * 2);
        let status_start = self
            .areas
            .status
            .x
            .saturating_add(UnicodeWidthStr::width(self.badge.as_ref()) as u16)
            .saturating_add(3);
        let status_end = self
            .areas
            .status
            .right()
            .saturating_sub(1)
            .saturating_sub(right_reserved);
        if !self.status.is_empty() && status_start < status_end {
            surface.set_stringn(
                status_start,
                self.areas.status.y,
                &self.status,
                status_end.saturating_sub(status_start) as usize,
                tui::ratatui::to_ratatui_style(self.theme.get("ui.text.inactive").patch(bar_style)),
            );
        }
        if let Some(position) = &self.position {
            if self.areas.status.width > position_width.saturating_add(2) {
                surface.set_stringn(
                    self.areas
                        .status
                        .right()
                        .saturating_sub(position_width.saturating_add(1)),
                    self.areas.status.y,
                    position,
                    position_width as usize,
                    tui::ratatui::to_ratatui_style(
                        self.theme.get("ui.text.inactive").patch(bar_style),
                    ),
                );
            }
        }

        if let Some(error) = &self.error {
            error.paint(
                Rect::new(
                    self.areas.error.x.saturating_add(1),
                    self.areas.error.y,
                    self.areas.error.width.saturating_sub(2),
                    self.areas.error.height,
                ),
                surface,
                self.theme.get("error"),
            );
        }
    }

    fn paint_plan_and_context(&self, surface: &mut crate::render::CellSurface) {
        if let Some(section) = self.model.plan_section() {
            if self.areas.plan.height > 0 {
                let inset = Rect::new(
                    self.areas.plan.x.saturating_add(1),
                    self.areas.plan.y,
                    self.areas.plan.width.saturating_sub(2),
                    self.areas.plan.height,
                );
                let bar_width = (inset.width as usize).saturating_sub(14).min(24);
                let filled = (section.done * bar_width)
                    .checked_div(section.total)
                    .unwrap_or(0);
                let line = Spans::from(vec![
                    Span::styled(
                        format!("{} ", section.title),
                        self.theme.get("ui.text.focus"),
                    ),
                    Span::styled(
                        format!(
                            "\u{2590}{}{}\u{258c} {}/{}",
                            "\u{2588}".repeat(filled),
                            "\u{2591}".repeat(bar_width.saturating_sub(filled)),
                            section.done,
                            section.total
                        ),
                        self.theme.get("warning"),
                    ),
                ]);
                surface.set_line(
                    inset.x,
                    inset.y,
                    &tui::ratatui::to_ratatui_line(&line),
                    inset.width,
                );
                for (index, item) in section.rows.iter().take(5).enumerate() {
                    let style = match item.tone {
                        helix_view::model::AssistantPlanTone::Completed => {
                            self.theme.get("diff.plus")
                        }
                        helix_view::model::AssistantPlanTone::InProgress => {
                            self.theme.get("warning")
                        }
                        helix_view::model::AssistantPlanTone::Failed => self.theme.get("error"),
                        helix_view::model::AssistantPlanTone::Pending => {
                            self.theme.get("ui.text.inactive")
                        }
                    };
                    let y = inset.y.saturating_add(1 + index as u16);
                    if y >= inset.bottom() {
                        break;
                    }
                    let line = Spans::from(vec![
                        Span::styled(item.icon.to_string(), style),
                        Span::styled(item.content.clone(), style),
                    ]);
                    surface.set_line(
                        inset.x,
                        y,
                        &tui::ratatui::to_ratatui_line(&line),
                        inset.width,
                    );
                }
            }
        }
        if let Some(context) = self.model.context_line() {
            if self.areas.context.height > 0 {
                surface.set_stringn(
                    self.areas.context.x.saturating_add(1),
                    self.areas.context.y,
                    context,
                    self.areas.context.width.saturating_sub(2) as usize,
                    tui::ratatui::to_ratatui_style(self.theme.get("ui.text.info")),
                );
            }
        }
    }

    fn paint_input(&self, surface: &mut crate::render::CellSurface) {
        let area = self.input.border_area;
        let border_style = self
            .theme
            .try_get("ui.assistant.input.border")
            .unwrap_or_else(|| self.theme.get("ui.window"));
        if area.width >= 4 && area.height >= 3 {
            let (top_left, top_right, bottom_left, bottom_right) = if self.input.rounded {
                ("╭", "╮", "╰", "╯")
            } else {
                ("┌", "┐", "└", "┘")
            };
            let width = area.width as usize;
            surface.set_stringn(
                area.x,
                area.y,
                &format!(
                    "{top_left}{}{top_right}",
                    "─".repeat(width.saturating_sub(2))
                ),
                width,
                tui::ratatui::to_ratatui_style(border_style),
            );
            for row in 1..area.height.saturating_sub(1) {
                let y = area.y.saturating_add(row);
                surface.set_stringn(
                    area.x,
                    y,
                    "│",
                    1,
                    tui::ratatui::to_ratatui_style(border_style),
                );
                surface.set_stringn(
                    area.right().saturating_sub(1),
                    y,
                    "│",
                    1,
                    tui::ratatui::to_ratatui_style(border_style),
                );
            }
            surface.set_stringn(
                area.x,
                area.bottom().saturating_sub(1),
                &format!(
                    "{bottom_left}{}{bottom_right}",
                    "─".repeat(width.saturating_sub(2))
                ),
                width,
                tui::ratatui::to_ratatui_style(border_style),
            );
        }
        for (row, line) in self.input.visible_lines.iter().enumerate() {
            if row as u16 >= self.input.text_area.height {
                break;
            }
            surface.set_stringn(
                self.input.text_area.x,
                self.input.text_area.y.saturating_add(row as u16),
                line,
                self.input.text_area.width as usize,
                tui::ratatui::to_ratatui_style(self.theme.get("ui.text")),
            );
        }
        if self.input.placeholder
            && self.input.text_area.width > 0
            && self.input.text_area.height > 0
        {
            surface.set_stringn(
                self.input.text_area.x,
                self.input.text_area.y,
                "Ask anything...",
                self.input.text_area.width as usize,
                tui::ratatui::to_ratatui_style(self.theme.get("ui.text.inactive")),
            );
        }
    }

    fn paint_mention(&self, surface: &mut crate::render::CellSurface) {
        let Some(mention) = &self.mention else {
            return;
        };
        let area = tui::ratatui::to_ratatui_rect(mention.area);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
        let background = self.theme.get("ui.popup");
        let selected = self.theme.get("ui.menu.selected");
        surface.set_style(area, tui::ratatui::to_ratatui_style(background));
        for (row, candidate) in mention.candidates.iter().enumerate() {
            let y = mention.area.y.saturating_add(row as u16);
            let is_selected = row == mention.selected;
            let row_style = if is_selected { selected } else { background };
            surface.set_stringn(
                mention.area.x,
                y,
                " ".repeat(mention.area.width as usize),
                mention.area.width as usize,
                tui::ratatui::to_ratatui_style(row_style),
            );
            surface.set_stringn(
                mention.area.x,
                y,
                &candidate.label,
                mention.area.width as usize,
                tui::ratatui::to_ratatui_style(if is_selected {
                    selected
                } else {
                    self.theme.get("ui.text")
                }),
            );
            let detail_x = mention
                .area
                .x
                .saturating_add(UnicodeWidthStr::width(candidate.label.as_str()) as u16)
                .saturating_add(2);
            if detail_x < mention.area.right() {
                surface.set_stringn(
                    detail_x,
                    y,
                    &candidate.detail,
                    mention.area.right().saturating_sub(detail_x) as usize,
                    tui::ratatui::to_ratatui_style(if is_selected {
                        selected
                    } else {
                        self.theme.get("ui.text.inactive")
                    }),
                );
            }
        }
    }

    fn paint_messages(&self, surface: &mut crate::render::CellSurface) {
        if self.blocks.is_empty() {
            let message = if self.presentation_pending {
                "Preparing messages..."
            } else {
                "Space A for menu"
            };
            let x = self.areas.content.x.saturating_add(
                self.areas
                    .content
                    .width
                    .saturating_sub(UnicodeWidthStr::width(message) as u16)
                    / 2,
            );
            let y = self
                .areas
                .content
                .y
                .saturating_add(self.areas.content.height / 2);
            if y < self.areas.content.bottom() {
                surface.set_stringn(
                    x,
                    y,
                    message,
                    self.areas.content.width as usize,
                    tui::ratatui::to_ratatui_style(self.theme.get("ui.text.inactive")),
                );
            }
            return;
        }
        self.chat_layout.paint_dynamic(
            surface,
            &self.blocks,
            self.animated_prefix
                .as_ref()
                .map(|(index, prefix)| (*index, prefix.as_ref())),
            self.selected_accent,
        );
        if self.chat_layout.total_height > self.areas.content.height as usize {
            let scroll_style = self.theme.get("ui.menu.scroll");
            crate::widgets::Scrollbar::new(
                self.chat_layout.total_height,
                self.chat_layout.scroll(),
                self.areas.content.height as usize,
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
                    self.areas.content_raw.right().saturating_sub(1),
                    self.areas.content.y,
                    1,
                    self.areas.content.height,
                ),
                surface,
            );
        }
    }
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

    fn sync_from_assistant(&mut self, editor: &mut Editor) {
        let model = Self::assistant_model(editor);
        if model.active_thread.is_none() {
            self.output.set_content(Arc::from([]));
            self.output.scroll_to(0);
            self.render_model = Arc::new(model);
            return;
        }

        self.sync_input_from_assistant(editor, &model.input);
        let active_thread = model.active_thread;
        let entries_len = model.entries.len();
        self.output.set_content(Arc::clone(&model.entries));
        if !self.output.is_following_end() {
            self.output.scroll_to(model.content_scroll);
        }
        self.consume_pending_subagent_jump(editor, active_thread, entries_len);
        self.render_model = Arc::new(model);
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
        self.visible_presentation
            .as_ref()
            .and_then(|presentation| presentation.metadata().entry_id_at(index))
            .or_else(|| self.output.content().get(index).map(|entry| entry.id))
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
        self.visible_presentation
            .as_ref()
            .and_then(|presentation| {
                presentation
                    .metadata()
                    .selected_index(model.selected_entry_id())
            })
            .or_else(|| {
                let selected = model.selected_entry_id()?;
                self.output
                    .content()
                    .iter()
                    .position(|entry| entry.id == selected)
            })
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
        let model = Self::assistant_model(editor);
        let Some(thread) = model.active_thread else {
            return;
        };
        if model.content_scroll() == content_scroll {
            self.output.scroll_to(content_scroll);
            return;
        }
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
            scroll: if self.output.is_following_end() {
                Scrollable::scroll(&self.output)
            } else {
                model.content_scroll()
            },
        }
    }

    fn navigation_cursor(&self, editor: &Editor) -> MessageCursor {
        let navigation = self.navigation_state(editor);
        MessageCursor::new(navigation.selected, navigation.scroll)
    }

    fn apply_message_navigation(
        &mut self,
        editor: &mut Editor,
        before: MessageNavigationState,
        cursor: MessageCursor,
    ) -> Option<usize> {
        let selected = cursor.selected();
        if cursor.scroll() != before.scroll {
            self.set_content_scroll(editor, cursor.scroll());
        }
        if selected != before.selected {
            self.set_selected_entry(
                editor,
                selected.and_then(|index| self.entry_id_at(editor, index)),
                true,
            );
        }
        self.follow_end_if_last_message(editor, selected);
        selected
    }

    fn follow_end_if_last_message(&mut self, editor: &mut Editor, selected: Option<usize>) {
        let Some(selected) = selected else {
            return;
        };
        if Some(selected) != self.chat_layout.len().checked_sub(1) {
            return;
        }

        let max_scroll = self.output.max_scroll();
        let before = self.navigation_state(editor);
        self.output.scroll_to_end();
        if before.scroll != max_scroll {
            self.set_content_scroll(editor, max_scroll);
        }
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

    fn status_bar_parts(&self, model: &helix_view::model::AssistantModel) -> Vec<String> {
        let mut parts = model
            .status_items()
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();
        if let Some(profile) = &model.active_profile {
            parts.push(format!("profile {profile}"));
        }
        if let Some(feedback) = model.feedback_label() {
            parts.push(feedback);
        }
        if let Some(status) = &model.agent_status {
            parts.push(status.clone());
        } else if model.agent_busy {
            parts.push("working".to_string());
        }
        if self.editing.is_some() {
            parts.push("editing message".to_string());
        }
        parts
    }

    fn should_drive_activity_spinner(model: &helix_view::model::AssistantModel) -> bool {
        model.has_running_activity()
    }

    pub fn new() -> Self {
        Self {
            focused: true,
            output: ContentRegion::default(),
            render_model: Arc::new(helix_view::model::AssistantModel::default()),
            input: helix_view::edit_region::EditRegion::default(),
            panel_error: None,
            error_marquee: Marquee::new(),
            last_input_cursor: None,
            input_layout: InputLayoutCache::default(),
            model_panel_id: None,
            chat_layout: MessageListState::default(),
            spinner: Spinner::dots(Duration::from_millis(80)),
            message_focus_animation: Animation::new({
                let mut spec = AnimationSpec::new(Duration::from_millis(220));
                spec.timing_function = AnimationTimingFunction::EaseOut;
                spec.iteration_count = AnimationIterationCount::Count(1);
                spec.direction = AnimationDirection::Normal;
                spec.fill_mode = AnimationFillMode::Forwards;
                spec
            }),
            pending_message_g: false,
            markdown: None,
            presentation: None,
            completed_presentation: None,
            visible_presentation: None,
            mention: MentionPopup::default(),
            elicitation_form: None,
            auth_selected: 0,
            auth_transient: false,
            pending_subagent_jump: None,
            editing: None,
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

    fn bindings_for_context(
        layer: AssistantLayer,
        input_mode: Mode,
    ) -> impl Iterator<Item = &'static AssistantBinding> {
        let contextual = match (layer, input_mode) {
            (AssistantLayer::Input, Mode::Normal) => NORMAL_INPUT_BINDINGS,
            _ => &[],
        };
        contextual
            .iter()
            .chain(Self::bindings_for_layer(layer).iter())
    }

    fn binding_for_key(
        layer: AssistantLayer,
        input_mode: Mode,
        key: &KeyEvent,
    ) -> Option<&'static AssistantBinding> {
        Self::bindings_for_context(layer, input_mode).find(|binding| binding.key.matches(key))
    }

    /// Key help for a layer: (key, label) pairs, highest priority first.
    /// Single source: the layer binding tables — this feeds the info popup.
    fn layer_help_entries(
        &self,
        model: &helix_view::model::AssistantModel,
        layer: AssistantLayer,
    ) -> Vec<(&'static str, &'static str)> {
        let mut entries = Self::bindings_for_context(layer, self.input.mode())
            .filter_map(|binding| binding.hint)
            .collect::<Vec<_>>();

        if layer == AssistantLayer::Input && self.editing.is_some() {
            for entry in &mut entries {
                if entry.0 == "enter" {
                    entry.1 = "resubmit";
                } else if entry.0 == "esc" {
                    entry.1 = "cancel edit";
                }
            }
        }

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

        entries.sort_by_key(|entry| std::cmp::Reverse(entry.2));
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
        let title = if layer == AssistantLayer::Input && self.editing.is_some() {
            "Assistant: edit"
        } else {
            Self::layer_title(layer)
        };
        helix_view::info::Info::new(title, &entries)
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

    fn release_focus_to_editor(&mut self, editor: &mut Editor) {
        self.set_focused(false);
        if self.shown_help.take().is_some() {
            editor.autoinfo = None;
        }
    }

    fn replay_key_to_editor(&mut self, key: KeyEvent, cx: &mut Context) -> EventResult {
        self.release_focus_to_editor(cx.editor);
        log::debug!(
            "[assistant_event] released focus and replaying key={} to editor",
            key.key_sequence_format()
        );
        EventResult::Consumed(Some(PostAction::ReplayKeys {
            keys: vec![key],
            count: 1,
            pop_macro_replaying: false,
        }))
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
        now: Instant,
    ) -> Option<(GraphicsStyle, f32)> {
        if model.focus() != helix_view::assistant::thread::Focus::Messages
            || model.selected_entry_id().is_none()
        {
            return None;
        }

        let sample = self.message_focus_animation.sample_at(now);
        let accent = theme
            .try_get("ui.accent")
            .or_else(|| theme.try_get("ui.cursor.primary"))
            .unwrap_or_else(|| theme.get("ui.menu.selected"));
        let color = accent.bg.or(accent.fg).or(accent.underline_color)?;
        Some((GraphicsStyle::default().fg(color), sample.progress))
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

        let mut snapshot = self.render_model.as_ref().clone();

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

    fn markdown_render_style(theme: &helix_view::Theme) -> MarkdownRenderStyle {
        let base = theme.get("ui.text.info");
        MarkdownRenderStyle {
            base,
            lines: MarkdownLineStyles {
                heading: theme.get("markup.heading.1"),
                code: theme.get("markup.raw.inline"),
                bold: base.add_modifier(Modifier::BOLD),
                italic: base.add_modifier(Modifier::ITALIC),
                strike: base.add_modifier(Modifier::CROSSED_OUT),
                link: theme.get("markup.link.url"),
                quote: theme.get("markup.quote"),
                list: theme.get("markup.list.unnumbered"),
                separator: theme.get("ui.statusline.separator"),
            },
        }
    }

    fn prepare_render_snapshot(
        &mut self,
        area: Rect,
        cx: &RenderContext,
    ) -> Option<AssistantRenderSnapshot> {
        use helix_view::layout::{split_horizontal, split_vertical, Size};

        if area.width < 20 || area.height < 6 {
            self.last_input_cursor = None;
            self.chat_layout = MessageListState::default();
            return None;
        }

        let horizontal = split_horizontal(area, &[Size::fixed(1), Size::Fill]);
        let border = horizontal[0];
        let inner = horizontal[1];
        let model = self.render_model.clone();
        let plan_rows = model
            .plan_section()
            .as_ref()
            .map(|section| (section.rows.len() + 1).min(6) as u16)
            .unwrap_or(0);
        let context_rows = u16::from(!model.context_items.is_empty());
        let vertical = split_vertical(
            inner,
            &[
                Size::fixed(1),
                Size::Fill,
                Size::fixed(plan_rows),
                Size::fixed(context_rows),
                Size::fixed(5),
                Size::fixed(1),
                Size::fixed(1),
            ],
        );
        let content_raw = vertical[1];
        let content = Rect::new(
            content_raw.x.saturating_add(1),
            content_raw.y,
            content_raw.width.saturating_sub(2),
            content_raw.height,
        );
        let areas = AssistantRenderAreas {
            border,
            inner,
            header: vertical[0],
            content_raw,
            content,
            plan: vertical[2],
            context: vertical[3],
            input_raw: vertical[4],
            status: vertical[5],
            error: vertical[6],
        };

        let layer = self.active_layer_for_model(&model);
        let badge: Arc<str> = Arc::from(if self.editing.is_some() {
            "EDIT"
        } else {
            match layer {
                AssistantLayer::Input => "INPUT",
                AssistantLayer::Messages => "MESSAGES",
                AssistantLayer::Elicitation => "FORM",
                AssistantLayer::Auth => "AUTH",
            }
        });
        let status: Arc<str> = Arc::from(self.status_bar_parts(&model).join("  "));
        let position = if layer == AssistantLayer::Messages {
            let total = self.output.content().len();
            (total > 0).then(|| {
                Arc::<str>::from(
                    model
                        .selected_entry_id()
                        .and_then(|id| {
                            self.output
                                .content()
                                .iter()
                                .position(|entry| entry.id == id)
                        })
                        .map(|index| format!("{}/{}", index + 1, total))
                        .unwrap_or_else(|| total.to_string()),
                )
            })
        } else {
            None
        };

        let now = cx.frame_time();
        let error_width = areas.error.width.saturating_sub(2);
        let error = self.error_marquee.sample(error_width, now);
        if let Some(next) = error.as_ref().and_then(|frame| frame.next_redraw) {
            cx.request_frame_at(ERROR_MARQUEE_FRAME, next);
        }
        let selected_accent = self.current_message_accent(&model, cx.assistant_theme(), now);
        if let Some((_, progress)) = selected_accent {
            if progress < 1.0 {
                if let Some(next) = self.message_focus_animation.sample_at(now).next_redraw {
                    cx.request_frame_at(MESSAGE_FOCUS_FRAME, next);
                }
            }
        }

        let input_border_area = Rect::new(
            areas.input_raw.x.saturating_add(1),
            areas.input_raw.y,
            areas.input_raw.width.saturating_sub(2),
            areas.input_raw.height,
        );
        let input_area = Rect::new(
            input_border_area.x.saturating_add(2),
            input_border_area.y.saturating_add(1),
            input_border_area.width.saturating_sub(4),
            input_border_area.height.saturating_sub(2),
        );
        self.input.set_area(input_area);
        self.last_input_cursor = None;
        let mut visible_lines: Arc<[String]> = Arc::from([]);
        let mut placeholder = true;
        if input_area.width > 0 && input_area.height > 0 {
            if let Some(doc) = self
                .input
                .doc_id()
                .and_then(|document| cx.component_document(document))
            {
                let text = doc.text();
                let cursor = doc
                    .selections()
                    .get(&self.input.id())
                    .map(|selection| selection.primary().cursor(text.slice(..)))
                    .unwrap_or(0);
                let layout = self.input_layout.layout(
                    InputLayoutKey {
                        document_version: doc.version(),
                        cursor,
                        width: input_area.width as usize,
                        height: input_area.height as usize,
                    },
                    text.slice(..),
                );
                placeholder = layout.placeholder;
                visible_lines = Arc::clone(&layout.visible_lines);
                let cursor_x = input_area.x.saturating_add(layout.cursor_column as u16);
                let cursor_y = input_area.y.saturating_add(layout.cursor_row as u16);
                if cursor_x < input_area.right() && cursor_y < input_area.bottom() {
                    self.last_input_cursor = Some((cursor_x, cursor_y));
                }
            }
        }
        let input = AssistantInputSnapshot {
            border_area: input_border_area,
            text_area: input_area,
            visible_lines,
            placeholder,
            rounded: cx.config().acp.bubble_corners_rounded(),
        };
        let mention = self.mention_render_snapshot(input_area);

        self.output.set_area(content);
        let mut presentation_pending = false;
        let mut presented = None;
        if content.width > 0 && content.height > 0 {
            if let Some(thread) = model.active_thread {
                let markdown_key = MarkdownRequestKey {
                    thread,
                    content_revision: model.content_revision,
                    width: content.width,
                    theme_generation: cx.theme_generation(),
                };
                if self.markdown.is_none() {
                    self.markdown = Some(MarkdownService::spawn(
                        cx.work(),
                        cx.block(),
                        cx.redraw.clone(),
                    ));
                }
                if self
                    .markdown
                    .as_ref()
                    .is_some_and(|service| service.needs(&markdown_key))
                {
                    self.markdown.as_mut().unwrap().submit(
                        markdown_key.clone(),
                        Arc::clone(&model.entries),
                        Self::markdown_render_style(cx.assistant_theme()),
                        cx.assistant_theme(),
                        cx.syntax_loader().load_full(),
                    );
                }
                let markdown = self
                    .markdown
                    .as_ref()
                    .and_then(MarkdownService::snapshot)
                    .filter(|snapshot| snapshot.matches(&markdown_key));
                let presentation_key = PresentationRequestKey::new(
                    &model,
                    content.width,
                    cx.theme_generation(),
                    self.editing.as_ref().map(|editing| editing.target),
                    self.elicitation_form.as_ref(),
                    self.auth_selected,
                    MessageCorners::Squared,
                    markdown.as_ref().map(|snapshot| snapshot.generation()),
                )
                .expect("active assistant thread");
                if self.presentation.is_none() {
                    self.presentation = Some(PresentationService::spawn(
                        cx.work(),
                        cx.block(),
                        cx.redraw.clone(),
                    ));
                }
                if self
                    .presentation
                    .as_ref()
                    .is_some_and(|service| service.needs(&presentation_key))
                {
                    self.presentation.as_mut().unwrap().submit(
                        PresentationSource {
                            key: presentation_key.clone(),
                            entries: Arc::clone(&model.entries),
                            markdown_key,
                            markdown,
                        },
                        cx.assistant_theme_arc(),
                    );
                }

                let published = self
                    .presentation
                    .as_ref()
                    .and_then(PresentationService::snapshot);
                if let Some(snapshot) = published
                    .as_ref()
                    .filter(|snapshot| {
                        snapshot.matches(&presentation_key)
                            || snapshot.geometrically_compatible(&presentation_key)
                    })
                    .cloned()
                {
                    self.completed_presentation = Some(snapshot);
                }
                presented = published
                    .filter(|snapshot| snapshot.matches(&presentation_key))
                    .or_else(|| {
                        self.completed_presentation
                            .as_ref()
                            .filter(|snapshot| snapshot.geometrically_compatible(&presentation_key))
                            .cloned()
                    });
                presentation_pending = presented.is_none();
            }
        }

        self.visible_presentation = presented.clone();
        let blocks = presented
            .as_ref()
            .map(|snapshot| snapshot.messages())
            .unwrap_or_else(|| Arc::from([]));
        let selected = presented.as_ref().and_then(|snapshot| {
            snapshot
                .metadata()
                .selected_index(model.selected_entry_id())
        });
        self.output.set_content_height(
            presented
                .as_ref()
                .map_or(0, |snapshot| snapshot.metadata().total_height),
        );
        let scroll = if self.output.is_following_end() {
            self.output.max_scroll()
        } else {
            model.content_scroll().min(self.output.max_scroll())
        };
        let mut cursor = MessageCursor::new(selected, scroll);
        let chat_layout = MessageListState::layout(content, &blocks, scroll, selected);
        cursor.clamp_selection(&chat_layout);
        self.output.scroll_to(cursor.scroll());
        self.output.set_content_height(chat_layout.total_height);
        self.chat_layout = chat_layout.clone();

        let animated_row = presented.as_ref().and_then(|snapshot| {
            snapshot.metadata().active_animation(
                model.active_thought,
                Self::should_drive_activity_spinner(&model),
            )
        });
        let animated_prefix = animated_row.map(|index| {
            cx.request_frame_at(ACTIVITY_SPINNER_FRAME, self.spinner.next_redraw_at(now));
            (
                index,
                Arc::<str>::from(format!(" {} ", self.spinner.frame_at(now))),
            )
        });

        Some(AssistantRenderSnapshot {
            areas,
            theme: cx.assistant_theme_arc(),
            model,
            focused: self.focused,
            badge,
            status,
            position,
            error,
            input,
            mention,
            blocks,
            chat_layout,
            presentation_pending,
            animated_prefix,
            selected_accent,
        })
    }

    fn mention_render_snapshot(&self, input_area: Rect) -> Option<AssistantMentionSnapshot> {
        if !self.mention.active || self.mention.candidates.is_empty() {
            return None;
        }
        let (cursor_x, cursor_y) = self.last_input_cursor?;
        if input_area.width == 0 || input_area.height == 0 {
            return None;
        }
        let visible = self.mention.candidates.len().min(6);
        let width = self
            .mention
            .candidates
            .iter()
            .take(visible)
            .map(|candidate| {
                UnicodeWidthStr::width(candidate.label.as_str())
                    + 2
                    + UnicodeWidthStr::width(candidate.detail.as_str())
            })
            .max()
            .unwrap_or(18)
            .clamp(18, 56) as u16;
        let width = width.min(input_area.width.max(1));
        let height = visible as u16;
        let x = cursor_x.min(input_area.right().saturating_sub(width));
        let y = if cursor_y >= height {
            cursor_y.saturating_sub(height)
        } else {
            input_area.bottom()
        };
        Some(AssistantMentionSnapshot {
            area: Rect::new(x, y, width, height),
            candidates: Arc::from(
                self.mention
                    .candidates
                    .iter()
                    .take(visible)
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            selected: self.mention.selected,
        })
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
        let before = self.navigation_state(editor);
        if index == before.selected {
            if let Some(index) = index {
                let scroll = self
                    .chat_layout
                    .scroll_to_item(index, before.scroll, viewport_height);
                if scroll != before.scroll {
                    self.set_content_scroll(editor, scroll);
                }
                self.follow_end_if_last_message(editor, Some(index));
            }
            return before.selected;
        }
        let entry = index.and_then(|index| self.entry_id_at(editor, index));
        let selected = self.set_selected_entry(editor, entry, true);
        if let Some(index) = selected.and_then(|_| self.selected_index(editor)) {
            let cursor = MessageCursor::new(Some(index), self.navigation_state(editor).scroll);
            let scroll = self
                .chat_layout
                .scroll_to_item(index, cursor.scroll(), viewport_height);
            if scroll != before.scroll {
                self.set_content_scroll(editor, scroll);
            }
        }
        self.follow_end_if_last_message(editor, self.selected_index(editor));
        self.selected_index(editor)
    }

    pub fn select_prev_message(&mut self, editor: &mut Editor) -> Option<usize> {
        let before = self.navigation_state(editor);
        let mut cursor = self.navigation_cursor(editor);
        cursor.move_prev(&self.chat_layout, self.output.area().height as usize);
        self.apply_message_navigation(editor, before, cursor)
    }

    pub fn select_next_message(&mut self, editor: &mut Editor) -> Option<usize> {
        let before = self.navigation_state(editor);
        let mut cursor = self.navigation_cursor(editor);
        cursor.move_next(&self.chat_layout, self.output.area().height as usize);
        self.apply_message_navigation(editor, before, cursor)
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
        let before = self.navigation_state(editor);
        let mut cursor = self.navigation_cursor(editor);
        cursor.move_prev_page(&self.chat_layout, self.output.area().height as usize);
        self.apply_message_navigation(editor, before, cursor)
    }

    pub fn select_next_message_page(&mut self, editor: &mut Editor) -> Option<usize> {
        let before = self.navigation_state(editor);
        let mut cursor = self.navigation_cursor(editor);
        cursor.move_next_page(&self.chat_layout, self.output.area().height as usize);
        self.apply_message_navigation(editor, before, cursor)
    }

    pub fn selected_entry_ref(&self, editor: &Editor) -> Option<&ChatEntry> {
        let index = self.selected_index(editor)?;
        self.output.content().get(index)
    }

    fn selected_user_message(
        &self,
        editor: &Editor,
    ) -> Option<(helix_view::assistant::thread::EntryId, String)> {
        let entry = self.selected_entry_ref(editor)?;
        let helix_view::model::AssistantEntryKind::UserMessage(text) = &entry.kind else {
            return None;
        };
        Some((entry.id, text.clone()))
    }

    fn enter_edit_selected_message(&mut self, editor: &mut Editor) -> bool {
        let Some((target, text)) = self.selected_user_message(editor) else {
            editor.set_status("Only user messages can be edited");
            return false;
        };
        let model = Self::assistant_model(editor);
        self.editing = Some(EditState {
            target,
            previous_draft: model.input.clone(),
            previous_focus: model.focus(),
        });
        self.sync_input_from_assistant(editor, &text);
        self.set_draft(editor, text);
        self.focus_input_region(editor);
        self.input
            .enter_insert_at(editor, helix_view::edit_region::InsertEntry::AtLineEnd);
        editor.set_status("Editing assistant message");
        true
    }

    fn cancel_edit(&mut self, editor: &mut Editor) -> bool {
        let Some(editing) = self.editing.take() else {
            return false;
        };
        self.sync_input_from_assistant(editor, &editing.previous_draft);
        self.set_draft(editor, editing.previous_draft);
        match editing.previous_focus {
            helix_view::assistant::thread::Focus::Input => self.focus_input_region(editor),
            helix_view::assistant::thread::Focus::Messages => self.focus_messages(editor),
        }
        editor.set_status("Canceled assistant message edit");
        true
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

        log::debug!(
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
        self.release_focus_to_editor(editor);
        self.message_focus_animation.stop();
        log::debug!(
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
        let in_output_scroll = self.chat_layout.contains_scroll_target(x, y);
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
                log::debug!(
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
            MouseEventKind::ScrollUp if in_output_scroll => {
                let lines = editor.config().scroll_lines.unsigned_abs().max(1);
                let scroll = Scrollable::scroll(&self.output).saturating_sub(lines);
                Scrollable::scroll_to(&mut self.output, scroll);
                self.set_content_scroll(editor, Scrollable::scroll(&self.output));
                EventResult::Consumed(None)
            }
            MouseEventKind::ScrollDown if in_output_scroll => {
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

    fn fork_submit_prompt(
        target: helix_view::assistant::thread::EntryId,
        text: String,
        cx: &mut Context,
    ) -> bool {
        let effects = match cx.editor.fork_submit_active_assistant_prompt(target, text) {
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
        if Self::assistant_model(cx.editor).agent_busy {
            self.sync_draft_to_assistant(cx.editor);
            cx.editor
                .set_status("Assistant is busy; wait for the current turn or cancel it.");
            return;
        }
        let text = match self.input.take_text(cx.editor) {
            Some(t) => t,
            None => return,
        };
        if !text.is_empty() {
            self.panel_error = None;
            if let Some(editing) = self.editing.take() {
                if !Self::fork_submit_prompt(editing.target, text.clone(), cx) {
                    self.editing = Some(editing);
                    self.sync_input_from_assistant(cx.editor, &text);
                    self.sync_draft_to_assistant(cx.editor);
                } else {
                    cx.editor.set_status("Resubmitted edited assistant message");
                }
            } else if !Self::submit_prompt(text, cx) {
                self.sync_draft_to_assistant(cx.editor);
            }
        }
    }

    fn open_mode_config_picker(&mut self, cx: &mut Context) -> bool {
        use crate::runtime::ui::command::{AssistantCommand, ModeConfigPickerItem, UiCommand};

        let Some((thread, mode, config, active_profile)) = cx.editor.active_assistant_mode_config()
        else {
            cx.editor.set_status("No active assistant thread");
            return true;
        };

        let mut items = Vec::new();
        items.extend(cx.editor.config().assistant.profiles.iter().map(|profile| {
            ModeConfigPickerItem::Profile {
                profile: profile.defaults(),
                name: profile.name.clone(),
                agent: profile.agent.clone(),
                current: active_profile.as_deref() == Some(profile.name.as_str()),
            }
        }));
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

        cx.submit_ui(UiCommand::Assistant(
            AssistantCommand::PushModeConfigPicker { thread, items },
        ));
        true
    }

    fn set_active_rating(
        &mut self,
        cx: &mut Context,
        rating: helix_view::assistant::thread::Rating,
    ) {
        let effects = match cx.editor.set_active_assistant_rating(rating) {
            Ok(effects) => effects,
            Err(err) => {
                cx.editor.set_error(err.to_string());
                return;
            }
        };
        Self::apply_assistant_effects(cx.editor, effects);
        cx.editor.set_status("Updated assistant thread rating");
    }

    fn open_note_prompt(&mut self, cx: &mut Context) -> Option<crate::compositor::PostAction> {
        let current = cx.editor.active_assistant_note().unwrap_or_default();
        let mut prompt = crate::ui::Prompt::new(
            "assistant note:".into(),
            None,
            crate::ui::completers::none,
            |cx: &mut Context, input: &str, event: crate::ui::PromptEvent| {
                if event != crate::ui::PromptEvent::Validate {
                    return;
                }
                let note = (!input.trim().is_empty()).then(|| input.trim().to_string());
                let effects = match cx.editor.set_active_assistant_note(note) {
                    Ok(effects) => effects,
                    Err(err) => {
                        cx.editor.set_error(err.to_string());
                        return;
                    }
                };
                cx.editor.apply_assistant_effects(effects);
                cx.editor.set_status("Updated assistant thread note");
            },
        );
        prompt.set_line(current, cx.editor);
        Some(crate::compositor::PostAction::PushLayer(Box::new(prompt)))
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
            AssistantAction::Yank if !self.yank_pending_elicitation_url(cx.editor) => {
                self.yank_selected_message(cx.editor);
            }
            AssistantAction::FollowOrJump if !self.jump_selected_subagent(cx.editor) => {
                match cx.editor.toggle_active_assistant_follow() {
                    Ok((status, effects)) => {
                        Self::apply_assistant_effects(cx.editor, effects);
                        cx.editor.set_status(status);
                    }
                    Err(err) => cx.editor.set_status(err.to_string()),
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
            AssistantAction::EditMessage => {
                self.enter_edit_selected_message(cx.editor);
            }
            AssistantAction::ToggleReviewMode => {
                match cx.editor.toggle_active_assistant_review_mode() {
                    Ok((status, effects)) => {
                        Self::apply_assistant_effects(cx.editor, effects);
                        cx.editor.set_status(status);
                    }
                    Err(err) => cx.editor.set_status(err.to_string()),
                }
            }
            AssistantAction::RateUp => {
                self.set_active_rating(cx, helix_view::assistant::thread::Rating::Up);
            }
            AssistantAction::RateDown => {
                self.set_active_rating(cx, helix_view::assistant::thread::Rating::Down);
            }
            AssistantAction::EditNote => {
                return EventResult::Consumed(self.open_note_prompt(cx));
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
            let model = Self::assistant_model(cx.editor);
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
            AssistantAction::TransientSubmit
                if layer == AssistantLayer::Elicitation
                    && self.accept_pending_elicitation(cx.editor) =>
            {
                cx.editor.set_status("Submitted assistant request");
            }
            AssistantAction::TransientSubmit
                if layer == AssistantLayer::Auth && self.accept_auth_method(cx.editor) =>
            {
                self.auth_transient = false;
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
                if self.cancel_edit(cx.editor) {
                    return Some(EventResult::Consumed(None));
                }
                if self.input.mode() == Mode::Insert {
                    self.input.exit_insert_mode();
                    Some(EventResult::Consumed(None))
                } else {
                    self.release_focus_to_editor(cx.editor);
                    Some(EventResult::Consumed(None))
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
            AssistantAction::ToggleHelp => {
                self.toggle_help(cx.editor);
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
    fn sync(&mut self, _viewport: Rect, editor: &mut Editor) {
        self.output.ensure_init(editor);
        self.input.ensure_init(editor);
        if self.focused {
            editor.frontend_mut().focused_modal_input = self.input.input_state();
        }
        self.sync_from_assistant(editor);
        self.sync_to_model(editor);
    }

    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let snapshot = self.prepare_render_snapshot(area, cx);
        crate::render::PreparedRender::deferred(move |cancellation| {
            let mut output = crate::render::RenderOutput::sparse(area);
            if let Some(snapshot) = snapshot {
                snapshot.paint(output.surface_mut(), cancellation);
            }
            output
        })
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
        if let Some(binding) = Self::binding_for_key(layer, self.input.mode(), &key) {
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

        if layer == AssistantLayer::Messages {
            return self.replay_key_to_editor(key, cx);
        }

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

        if layer == AssistantLayer::Auth {
            return EventResult::Consumed(None);
        }

        if layer == AssistantLayer::Input
            && matches!(key.code, KeyCode::Char(' '))
            && key.modifiers.is_empty()
            && self.input.mode() != Mode::Insert
        {
            return self.replay_key_to_editor(key, cx);
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
                    } if modifiers.is_empty() && self.accept_selected_mention(cx.editor) => {
                        return EventResult::Consumed(None);
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
                if self.cancel_edit(cx.editor) {
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

        // Dispatch through the region's own engine + keymaps. If the engine
        // says the key is unbound in Normal mode, explicitly hand focus back
        // to the editor and replay the key through the global keymap. That
        // keeps component focus and editor command dispatch in sync.
        if self.dispatch_input_key(key, cx) {
            log::debug!(
                "[assistant_event] key={} mode={:?} → Consumed (engine handled)",
                key.key_sequence_format(),
                self.input.mode()
            );
            EventResult::Consumed(None)
        } else if self.input.mode() == Mode::Normal {
            self.replay_key_to_editor(key, cx)
        } else {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                if let Some(ch) = key.char() {
                    self.insert_char_into_input(ch, cx.editor);
                    self.sync_draft_to_assistant(cx.editor);
                    self.refresh_mention_popup(cx.editor);
                    return EventResult::Consumed(None);
                }
            }
            log::debug!(
                "[assistant_event] key={} mode={:?} → Consumed (insert-mode unbound)",
                key.key_sequence_format(),
                self.input.mode()
            );
            EventResult::Consumed(None)
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
    fn assistant_render_snapshot_is_send() {
        fn assert_send<T: Send>() {}

        assert_send::<AssistantRenderSnapshot>();
    }

    #[test]
    fn assistant_input_viewport_wraps_and_tracks_cursor_cells() {
        let text = helix_core::Rope::from("abcdef\ng");
        let layout = layout_input_viewport(text.slice(..), 4, 3, 4);

        assert_eq!(
            layout.visible_lines.as_ref(),
            &["abc".to_string(), "def".to_string(), "g".to_string()]
        );
        assert_eq!((layout.cursor_row, layout.cursor_column), (1, 1));
    }

    #[test]
    fn assistant_input_viewport_uses_terminal_width() {
        let text = helix_core::Rope::from("a界b");
        let layout = layout_input_viewport(text.slice(..), 2, 3, 2);

        assert_eq!(
            layout.visible_lines.as_ref(),
            &["a界".to_string(), "b".to_string()]
        );
        assert_eq!((layout.cursor_row, layout.cursor_column), (1, 0));
    }

    #[test]
    fn assistant_input_viewport_wraps_cursor_after_a_full_row() {
        let text = helix_core::Rope::from("abcdef");
        let layout = layout_input_viewport(text.slice(..), text.len_chars(), 3, 3);

        assert_eq!(
            layout.visible_lines.as_ref(),
            &["abc".to_string(), "def".to_string(), String::new()]
        );
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 0));
    }

    #[test]
    fn assistant_large_input_layout_is_viewport_bounded() {
        let draft = "line\n".repeat(200_000);
        let text = helix_core::Rope::from(draft.as_str());
        let layout = layout_input_viewport(text.slice(..), text.len_chars(), 80, 5);

        assert_eq!(layout.visible_lines.len(), 5);
        assert!(layout.inspected_lines <= 5);
    }

    #[test]
    fn assistant_bindings_have_no_layer_collisions() {
        for (layer, input_mode) in [
            (AssistantLayer::Input, Mode::Normal),
            (AssistantLayer::Input, Mode::Select),
            (AssistantLayer::Input, Mode::Insert),
            (AssistantLayer::Messages, Mode::Normal),
            (AssistantLayer::Elicitation, Mode::Normal),
            (AssistantLayer::Auth, Mode::Normal),
        ] {
            let mut keys = HashSet::new();
            for binding in AssistantPanel::bindings_for_context(layer, input_mode) {
                assert!(
                    keys.insert(binding.key),
                    "duplicate assistant binding in {layer:?}/{input_mode:?}: {:?}",
                    binding.key
                );
            }
        }
    }

    #[test]
    fn assistant_hint_bindings_dispatch_to_same_action() {
        for (layer, input_mode) in [
            (AssistantLayer::Input, Mode::Normal),
            (AssistantLayer::Input, Mode::Select),
            (AssistantLayer::Input, Mode::Insert),
            (AssistantLayer::Messages, Mode::Normal),
            (AssistantLayer::Elicitation, Mode::Normal),
            (AssistantLayer::Auth, Mode::Normal),
        ] {
            for binding in AssistantPanel::bindings_for_context(layer, input_mode)
                .filter(|binding| binding.hint.is_some())
            {
                let key = key_event_for_binding(binding.key);
                let dispatched = AssistantPanel::binding_for_key(layer, input_mode, &key)
                    .expect("hinted binding must dispatch");
                assert_eq!(dispatched.action, binding.action);
            }
        }
    }

    #[test]
    fn message_bindings_include_edit_hint() {
        assert!(AssistantPanel::bindings_for_layer(AssistantLayer::Messages)
            .iter()
            .any(|binding| {
                binding.action == AssistantAction::EditMessage
                    && binding
                        .hint
                        .is_some_and(|(key, label, _)| key == "e" && label == "edit")
            }));
    }

    #[test]
    fn message_bindings_include_feedback_hints() {
        let bindings = AssistantPanel::bindings_for_layer(AssistantLayer::Messages);

        assert!(bindings.iter().any(|binding| {
            binding.action == AssistantAction::RateUp
                && binding
                    .hint
                    .is_some_and(|(key, label, _)| key == "+/-" && label == "rate")
        }));
        assert!(bindings
            .iter()
            .any(|binding| binding.action == AssistantAction::RateDown));
        assert!(bindings.iter().any(|binding| {
            binding.action == AssistantAction::EditNote
                && binding
                    .hint
                    .is_some_and(|(key, label, _)| key == "n" && label == "note")
        }));
    }

    #[test]
    fn help_toggle_bound_where_typing_cannot_shadow_it() {
        let has_toggle = |layer, input_mode| {
            AssistantPanel::bindings_for_context(layer, input_mode)
                .any(|binding| binding.action == AssistantAction::ToggleHelp)
        };
        assert!(has_toggle(AssistantLayer::Input, Mode::Normal));
        assert!(has_toggle(AssistantLayer::Messages, Mode::Normal));
        assert!(has_toggle(AssistantLayer::Auth, Mode::Normal));
        // `?` stays typable in form fields and while editing the input box.
        assert!(!has_toggle(AssistantLayer::Elicitation, Mode::Insert));
        assert!(!has_toggle(AssistantLayer::Input, Mode::Insert));
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
        let theme_loader = theme::Loader::new(&[]);
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

    fn with_context<R>(editor: &mut Editor, f: impl FnOnce(&mut Context<'_>) -> R) -> R {
        let (ingress, _ingress_rx) =
            crate::runtime::RuntimeIngress::channel(editor.runtime().clone());
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

    fn plain_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn assert_replays_key(result: EventResult, expected: KeyEvent) {
        match result {
            EventResult::Consumed(Some(PostAction::ReplayKeys {
                keys,
                count,
                pop_macro_replaying,
            })) => {
                assert_eq!(keys, vec![expected]);
                assert_eq!(count, 1);
                assert!(!pop_macro_replaying);
            }
            _ => panic!("expected replay callback"),
        }
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
                                stream: None,
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
    fn editing_user_message_loads_input_and_cancel_restores_draft() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_default_thread(&mut editor);
            let effects = editor
                .set_active_assistant_draft_if_changed("draft".to_string())
                .expect("draft changed");
            editor.apply_assistant_effects(effects);
            let mut panel = AssistantPanel::new();
            panel.sync_from_assistant(&mut editor);
            select_thread_entry(&mut editor, 1);

            assert!(panel.enter_edit_selected_message(&mut editor));

            let target = editor.assistant_entry_id_at(false, 1).expect("entry");
            assert_eq!(
                panel.editing.as_ref().map(|editing| editing.target),
                Some(target)
            );
            let model = editor.assistant_model(false);
            assert_eq!(model.input, "hi there from user");
            assert_eq!(model.focus(), helix_view::assistant::thread::Focus::Input);

            assert!(panel.cancel_edit(&mut editor));

            assert!(panel.editing.is_none());
            let model = editor.assistant_model(false);
            assert_eq!(model.input, "draft");
            assert_eq!(
                model.focus(),
                helix_view::assistant::thread::Focus::Messages
            );
            assert_eq!(model.entries.len(), 2);
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
    fn assistant_input_normal_escape_releases_panel_focus() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
            panel.focus_input_region(&mut editor);

            let result = with_context(&mut editor, |cx| {
                panel.handle_event(&Event::Key(plain_key(KeyCode::Esc)), cx)
            });

            assert!(matches!(result, EventResult::Consumed(None)));
            assert!(!helix_view::traits::Focusable::is_focused(&panel));
        });
    }

    #[test]
    fn assistant_input_normal_space_releases_focus_and_replays_global_leader() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
            panel.focus_input_region(&mut editor);
            let key = plain_key(KeyCode::Char(' '));

            let result = with_context(&mut editor, |cx| panel.handle_event(&Event::Key(key), cx));

            assert_replays_key(result, key);
            assert!(!helix_view::traits::Focusable::is_focused(&panel));
        });
    }

    #[test]
    fn assistant_input_normal_unbound_key_releases_focus_and_replays_to_editor() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
            panel.focus_input_region(&mut editor);
            let key = plain_key(KeyCode::Char(':'));

            let result = with_context(&mut editor, |cx| panel.handle_event(&Event::Key(key), cx));

            assert_replays_key(result, key);
            assert!(!helix_view::traits::Focusable::is_focused(&panel));
        });
    }

    #[test]
    fn assistant_input_normal_question_mark_opens_local_help() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
            panel.focus_input_region(&mut editor);

            let result = with_context(&mut editor, |cx| {
                panel.handle_event(&Event::Key(plain_key(KeyCode::Char('?'))), cx)
            });

            assert!(matches!(result, EventResult::Consumed(None)));
            assert!(helix_view::traits::Focusable::is_focused(&panel));
            assert_eq!(panel.shown_help, Some(AssistantLayer::Input));
            assert_eq!(
                editor.autoinfo.as_ref().map(|info| info.title.as_ref()),
                Some("Assistant: input")
            );
        });
    }

    #[test]
    fn assistant_messages_unbound_key_releases_focus_and_replays_to_editor() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
            panel.focus_messages(&mut editor);
            let key = plain_key(KeyCode::Char(':'));

            let result = with_context(&mut editor, |cx| panel.handle_event(&Event::Key(key), cx));

            assert_replays_key(result, key);
            assert!(!helix_view::traits::Focusable::is_focused(&panel));
        });
    }

    #[test]
    fn assistant_input_insert_space_stays_in_assistant_input() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
            panel.focus_input_region(&mut editor);
            panel.input.enter_insert_mode("insert_mode".into());

            let result = with_context(&mut editor, |cx| {
                panel.handle_event(&Event::Key(plain_key(KeyCode::Char(' '))), cx)
            });

            assert!(matches!(result, EventResult::Consumed(None)));
            assert!(helix_view::traits::Focusable::is_focused(&panel));
            assert_eq!(panel.input.text(&editor).as_deref(), Some(" "));
        });
    }

    #[test]
    fn assistant_input_insert_question_mark_inserts_text() {
        with_test_runtime(|| {
            let mut editor = test_editor();
            seed_default_thread(&mut editor);
            let mut panel = AssistantPanel::new();
            panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
            panel.focus_input_region(&mut editor);
            panel.input.enter_insert_mode("insert_mode".into());

            let result = with_context(&mut editor, |cx| {
                panel.handle_event(&Event::Key(plain_key(KeyCode::Char('?'))), cx)
            });

            assert!(matches!(result, EventResult::Consumed(None)));
            assert!(helix_view::traits::Focusable::is_focused(&panel));
            assert_eq!(panel.input.text(&editor).as_deref(), Some("?"));
            assert_eq!(panel.shown_help, None);
            assert!(editor.autoinfo.is_none());
        });
    }
}
