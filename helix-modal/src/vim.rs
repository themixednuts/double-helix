//! Vim editing engine — verb→object paradigm.
//!
//! Implements Vim's operator-pending composition: an operator key (`d`, `c`, `y`, `>`, `gu`,
//! ...) enters a pending state, then the next motion or text object defines the range. Motions
//! that move between lines (`j`, `k`, `gg`, `G`) make the operator linewise. Visual modes are
//! characterwise (`v`), linewise (`V`) and blockwise (`C-v`), and `.` repeats with count
//! multiplication.
//!
//! The editor's mode is the source of truth: the engine re-derives its own state from it on
//! every key, so a mode change made elsewhere (a mouse click, the command palette) never leaves
//! the two out of step.

use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::sync::Arc;

use helix_core::movement::Movement;
use helix_core::textobject::TextObject;
use helix_core::{Range, Selection};
use helix_view::commands::editing;
use helix_view::document::Mode;
use helix_view::engine::{
    CharPendingId, CommandToken, EditingEngine, EngineResult, KeymapLookup, KeymapQuery,
    ModalInputState, MotionId, OperatorId, OperatorTargetId, RecordedAction, RepeatableCommandId,
    TextObjectId,
};
use helix_view::input::KeyEvent;
use helix_view::keyboard::KeyCode;
use helix_view::{DocumentId, Editor, ViewId};

use crate::registry::{CharPendingResolution, CommandRef, CommandRegistry, MotionEntry};
use crate::{
    document_version, finalize_insert_recording, is_char_key, is_repeatable_edit, key_to_digit,
    record_insert_key, InsertRecording,
};

/// Vim's mode state machine, finer grained than the editor's `Mode`.
#[derive(Debug, Clone)]
enum SubMode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    VisualBlock,
    OperatorPending(PendingOp),
}

impl SubMode {
    fn is_visual(&self) -> bool {
        matches!(self, Self::Visual | Self::VisualLine | Self::VisualBlock)
    }
}

/// State stored while an operator waits for its motion or text object.
#[derive(Debug, Clone)]
struct PendingOp {
    operator: OperatorId,
    register: Option<char>,
    /// Operator-side count (the count before the operator key).
    count: NonZeroUsize,
    /// A count was typed before the operator.
    count_given: bool,
    /// `i` or `a` was typed: the next key names a text object.
    text_object_kind: Option<TextObject>,
}

/// Where a linewise or blockwise visual selection started and where its cursor is. The
/// selection is rebuilt from these after every motion.
#[derive(Debug, Clone, Copy)]
struct VisualSpan {
    view: ViewId,
    anchor: usize,
    cursor: usize,
}

/// A char-pending command (`f`, `t`, `r`) bound directly to a key waits for the next key.
#[derive(Debug, Clone)]
struct AwaitingChar {
    command: CharPendingId,
    count: usize,
    register: Option<char>,
    /// The operator the character's motion completes (`d` then a bound `find_next_char`).
    operator: Option<PendingOp>,
}

/// Vim-specific operator behavior (local to VimEngine, not shared in registry).
struct VimOperatorBehavior {
    pending_display: &'static str,
    /// The key that repeats the operator to act on whole lines (`dd`, `>>`, `guu`).
    doubled_key: char,
    /// Afterwards the cursor goes to the start of the range the operator acted on (Vim's `y`,
    /// `>`, `gu`); deleting operators leave it where the text was.
    cursor_to_start: bool,
}

const VIM_OPERATORS: &[(OperatorId, VimOperatorBehavior)] = &[
    (OperatorId::new("delete_selection"), op("d", 'd', false)),
    (
        OperatorId::new("delete_selection_noyank"),
        op("d", 'd', false),
    ),
    (OperatorId::new("change_selection"), op("c", 'c', false)),
    (
        OperatorId::new("change_selection_noyank"),
        op("c", 'c', false),
    ),
    (OperatorId::new("yank"), op("y", 'y', true)),
    (OperatorId::new("vim_indent"), op(">", '>', true)),
    (OperatorId::new("vim_unindent"), op("<", '<', true)),
    (OperatorId::new("vim_lowercase"), op("gu", 'u', true)),
    (OperatorId::new("vim_uppercase"), op("gU", 'U', true)),
    (OperatorId::new("vim_toggle_case"), op("g~", '~', true)),
];

const fn op(
    pending_display: &'static str,
    doubled_key: char,
    cursor_to_start: bool,
) -> VimOperatorBehavior {
    VimOperatorBehavior {
        pending_display,
        doubled_key,
        cursor_to_start,
    }
}

fn vim_operator_behavior(id: OperatorId) -> Option<&'static VimOperatorBehavior> {
    VIM_OPERATORS
        .iter()
        .find(|(op, _)| *op == id)
        .map(|(_, b)| b)
}

fn is_change(operator: OperatorId) -> bool {
    matches!(
        operator.as_str(),
        "change_selection" | "change_selection_noyank"
    )
}

fn is_delete(operator: OperatorId) -> bool {
    matches!(
        operator.as_str(),
        "delete_selection" | "delete_selection_noyank"
    )
}

/// Motions that make an operator act on whole lines (`dj`, `yG`, `>gg`, `dL`).
const LINEWISE_MOTIONS: &[&str] = &[
    "move_line_down",
    "move_line_up",
    "move_visual_line_down",
    "move_visual_line_up",
    "goto_file_start",
    "goto_file_end",
    "goto_last_line",
    "goto_line",
    "vim_goto_line",
    "goto_window_top",
    "goto_window_center",
    "goto_window_bottom",
];

fn is_linewise(motion: MotionId) -> bool {
    LINEWISE_MOTIONS.contains(&motion.as_str())
}

/// A text object named by the key after `i` or `a` (`diw`, `ca(`, `yaf`).
#[derive(Debug, Clone, Copy)]
enum VimObject {
    Word {
        long: bool,
    },
    Paragraph,
    /// A bracket or quote pair, named by its opening character.
    Pair(char),
    /// The nearest enclosing pair of any kind.
    ClosestPair,
    /// A tree-sitter text object (`function`, `class`, `parameter`, `comment`).
    TreeSitter(&'static str),
}

fn vim_object(key: char) -> Option<VimObject> {
    Some(match key {
        'w' => VimObject::Word { long: false },
        'W' => VimObject::Word { long: true },
        'p' => VimObject::Paragraph,
        '(' | ')' | 'b' => VimObject::Pair('('),
        '{' | '}' | 'B' => VimObject::Pair('{'),
        '[' | ']' => VimObject::Pair('['),
        '<' | '>' => VimObject::Pair('<'),
        '"' | '\'' | '`' => VimObject::Pair(key),
        'm' => VimObject::ClosestPair,
        'f' => VimObject::TreeSitter("function"),
        'c' => VimObject::TreeSitter("class"),
        'a' => VimObject::TreeSitter("parameter"),
        '/' => VimObject::TreeSitter("comment"),
        _ => return None,
    })
}

fn select_vim_object(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    object: VimObject,
    kind: TextObject,
    count: usize,
) {
    match object {
        VimObject::Word { long } => {
            editing::textobject_word(editor, view_id, doc_id, kind, count, long)
        }
        VimObject::Paragraph => editing::textobject_paragraph(editor, view_id, doc_id, kind, count),
        VimObject::Pair(ch) => {
            editing::textobject_surrounding_pair(editor, view_id, doc_id, kind, ch, None, count)
        }
        VimObject::ClosestPair => {
            editing::textobject_closest_surrounding_pair(editor, view_id, doc_id, kind, count)
        }
        VimObject::TreeSitter(name) => {
            let suffix = match kind {
                TextObject::Inside => "inside",
                TextObject::Around | TextObject::Movement => "around",
            };
            editing::textobject_treesitter(
                editor,
                view_id,
                doc_id,
                kind,
                &format!("{name}.{suffix}"),
                count,
            );
        }
    }
}

/// Collapse every range to a point at its cursor: an operator's motion starts from the cursor,
/// not from whatever the last command selected. Returns the cursors, in range order.
fn collapse_to_cursor(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) -> Vec<usize> {
    let doc = helix_view::doc_mut!(editor, &doc_id);
    let text = doc.text().slice(..);
    let cursors: Vec<usize> = doc
        .selection(view_id)
        .iter()
        .map(|range| range.cursor(text))
        .collect();
    let selection = doc
        .selection(view_id)
        .clone()
        .transform(|range| Range::point(range.cursor(text)));
    doc.set_selection(view_id, selection);
    cursors
}

/// Vim's charwise motions backwards (`h`, `b`, `0`, `F`) and `l` are exclusive: the operator
/// stops short of the character the motion ends on. A Helix cursor covers its character, so a
/// backward extend takes the cursor's character and `l` one too many.
fn exclude_motion_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    cursors: &[usize],
    char_right: bool,
) {
    let doc = helix_view::doc_mut!(editor, &doc_id);
    let text = doc.text().slice(..);
    let ranges = doc.selection(view_id).ranges().to_vec();
    if ranges.len() != cursors.len() {
        return;
    }
    let primary = doc.selection(view_id).primary_index();
    let ranges = ranges
        .into_iter()
        .zip(cursors)
        .map(|(range, &cursor)| {
            if range.from() < cursor {
                Range::new(cursor, range.from())
            } else if char_right {
                let end = helix_core::graphemes::prev_grapheme_boundary(text, range.to());
                Range::new(cursor, end.max(cursor))
            } else {
                range
            }
        })
        .collect::<Vec<_>>();
    doc.set_selection(view_id, Selection::new(ranges.into(), primary));
}

/// Vim's special case for `dw`, `yw`, `2dw`: when the last word moved over ends a line, the
/// operated text ends there, not at the next line's first word.
fn stop_word_motion_at_line_end(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = helix_view::doc_mut!(editor, &doc_id);
    let text = doc.text().slice(..);
    let selection = doc.selection(view_id).clone().transform(|range| {
        let (from, to) = (range.from(), range.to());
        if to <= from {
            return range;
        }
        // The last character the motion covers: when it ends a line, stop at that line's end.
        let last_line = text.char_to_line(to - 1);
        let line_end = helix_core::line_ending::line_end_char_index(&text, last_line);
        if to > line_end && line_end > from {
            Range::new(from, line_end)
        } else {
            range
        }
    });
    doc.set_selection(view_id, selection);
}

/// In normal mode Vim's cursor rests on a character, never on a line break (unless the line is
/// empty).
fn keep_cursor_off_line_break(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = helix_view::doc_mut!(editor, &doc_id);
    let text = doc.text().slice(..);
    let len = text.len_chars();
    let selection = doc.selection(view_id).clone().transform(|range| {
        let cursor = range.cursor(text);
        let single = range.from() == range.to()
            || helix_core::graphemes::next_grapheme_boundary(text, range.from()) == range.to();
        // Past the final line break (the lines up to the end were deleted): onto the last line.
        if single && len > 0 && cursor >= len {
            return Range::point(text.line_to_char(text.char_to_line(len - 1)));
        }
        let line = text.char_to_line(cursor);
        let line_start = text.line_to_char(line);
        let at_line_break = cursor >= helix_core::line_ending::line_end_char_index(&text, line);
        if single && at_line_break && cursor > line_start {
            Range::point(helix_core::graphemes::prev_grapheme_boundary(text, cursor))
        } else {
            range
        }
    });
    doc.set_selection(view_id, selection);
}

/// After a paste the cursor goes where Vim puts it: at the start of pasted lines, on the last
/// character of pasted text.
fn cursor_after_paste(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = helix_view::doc_mut!(editor, &doc_id);
    let text = doc.text().slice(..);
    let selection = doc.selection(view_id).clone().transform(|range| {
        let pasted = text.slice(range.from()..range.to());
        let linewise = helix_core::line_ending::get_line_ending(&pasted).is_some();
        if linewise {
            Range::point(range.from())
        } else {
            Range::point(helix_core::graphemes::prev_grapheme_boundary(
                text,
                range.to(),
            ))
        }
    });
    doc.set_selection(view_id, selection);
}

/// Grow every range to the whole lines it touches, line breaks included. When the lines run to
/// the end of a file without a final line break, a delete takes the line break before them
/// instead, so no empty line is left behind.
fn expand_to_lines(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, deleting: bool) {
    let doc = helix_view::doc_mut!(editor, &doc_id);
    let text = doc.text().slice(..);
    let selection = doc.selection(view_id).clone().transform(|range| {
        let first = text.char_to_line(range.from());
        let last = text.char_to_line(range.to().saturating_sub(1).max(range.from()));
        let mut start = text.line_to_char(first);
        let end = text.line_to_char((last + 1).min(text.len_lines()));
        let ends_without_break = end == text.len_chars()
            && helix_core::line_ending::get_line_ending(&text.line(last)).is_none();
        if deleting && ends_without_break && first > 0 {
            start = helix_core::line_ending::line_end_char_index(&text, first - 1);
        }
        Range::new(start, end)
    });
    doc.set_selection(view_id, selection);
}

fn collapse_to_start(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = helix_view::doc_mut!(editor, &doc_id);
    let text = doc.text().slice(..);
    let selection = doc.selection(view_id).clone().transform(|range| {
        let pos = range.from().min(text.len_chars());
        Range::point(pos)
    });
    doc.set_selection(view_id, selection);
}

fn primary_cursor(editor: &Editor, view_id: ViewId, doc_id: DocumentId) -> usize {
    let doc = helix_view::doc!(editor, &doc_id);
    let text = doc.text().slice(..);
    doc.selection(view_id).primary().cursor(text)
}

/// The Vim editing engine.
pub struct VimEngine {
    registry: Arc<CommandRegistry>,
    sub_mode: SubMode,
    /// Operator-side count (before operator key). `None` means no count specified.
    count: Option<NonZeroUsize>,
    /// Motion-side count (after operator key in operator-pending mode).
    motion_count: Option<NonZeroUsize>,
    register: Option<char>,
    last_action: Option<RecordedAction>,
    pending_display_buf: String,
    /// Active insert recording, present while in insert mode.
    insert_recording: Option<InsertRecording>,
    /// `"` was pressed; the next character names the register.
    awaiting_register: bool,
    awaiting_char: Option<AwaitingChar>,
    /// The linewise or blockwise visual selection, rebuilt after every motion.
    visual: Option<VisualSpan>,
    /// `i` or `a` in visual mode: the next key names a text object to select.
    visual_text_object: Option<TextObject>,
    /// The last command executed, the entry command if it entered insert mode.
    last_command: Option<CommandToken>,
}

impl VimEngine {
    pub fn new(registry: Arc<CommandRegistry>) -> Self {
        Self {
            registry,
            sub_mode: SubMode::Normal,
            count: None,
            motion_count: None,
            register: None,
            last_action: None,
            pending_display_buf: String::new(),
            insert_recording: None,
            awaiting_register: false,
            awaiting_char: None,
            visual: None,
            visual_text_object: None,
            last_command: None,
        }
    }

    /// Re-derive the engine's mode from the editor's. Insert mode entered by a frontend
    /// command, select mode entered by `v` or a mouse drag, and normal mode restored by a click
    /// in another split all show up here.
    fn sync_mode(&mut self, editor: &Editor, view_id: ViewId) {
        match editor.mode() {
            Mode::Insert => {
                if !matches!(self.sub_mode, SubMode::Insert) {
                    self.sub_mode = SubMode::Insert;
                    self.visual = None;
                    self.visual_text_object = None;
                }
            }
            Mode::Select => {
                let span_elsewhere = self.visual.is_some_and(|span| span.view != view_id);
                if !self.sub_mode.is_visual() || span_elsewhere {
                    self.sub_mode = SubMode::Visual;
                    self.visual = None;
                }
            }
            Mode::Normal => {
                if matches!(self.sub_mode, SubMode::Insert) || self.sub_mode.is_visual() {
                    self.sub_mode = SubMode::Normal;
                    self.visual = None;
                    self.visual_text_object = None;
                }
            }
        }
    }

    // ─── Pre-resolve: count/register/escape/dot-repeat (before keymap) ───

    /// A register name after `"`, shared by normal and visual mode.
    fn take_register_key(&mut self, key: KeyEvent) -> Option<EngineResult> {
        if !self.awaiting_register {
            return None;
        }
        self.awaiting_register = false;
        if let Some(register) = key.char().filter(|_| key.modifiers.is_empty()) {
            self.register = Some(register);
        }
        Some(EngineResult::Pending)
    }

    /// Normal mode pre-resolve: count accumulation, register selection, dot-repeat.
    fn pre_resolve_normal(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult> {
        if let Some(result) = self.take_register_key(key) {
            return Some(result);
        }

        // A key the pending sequence takes (`f1`, `f"`) belongs to it, not to a count or a
        // register prefix.
        let sequence_pending =
            !keymaps.pending().is_empty() && keymaps.contains_key(editor.mode(), key);

        if let Some(digit) = key_to_digit(key).filter(|_| !sequence_pending) {
            if self.count.is_some() || digit > 0 {
                let current = self.count.map_or(0, NonZeroUsize::get);
                let new = current * 10 + digit;
                if new <= 100_000_000 {
                    self.count = NonZeroUsize::new(new);
                }
                return Some(EngineResult::Pending);
            }
        }

        if is_char_key(key, '"') && !sequence_pending {
            self.awaiting_register = true;
            return Some(EngineResult::Pending);
        }

        if is_char_key(key, '.') && keymaps.pending().is_empty() {
            let count = self.count.take().unwrap_or(NonZeroUsize::MIN);
            return Some(self.repeat_last(editor, view_id, doc_id, count));
        }

        None
    }

    /// Operator-pending pre-resolve: escape, motion-side count, `i`/`a` text objects, doubled
    /// operator.
    fn pre_resolve_operator_pending(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult> {
        if key.code == KeyCode::Esc {
            self.cancel_pending();
            return Some(EngineResult::Executed);
        }

        let pending = match &self.sub_mode {
            SubMode::OperatorPending(p) => p.clone(),
            _ => unreachable!(),
        };

        // `i` or `a` was typed: this key names the text object.
        if let Some(kind) = pending.text_object_kind {
            let Some(object) = key
                .char()
                .filter(|_| key.modifiers.is_empty())
                .and_then(vim_object)
            else {
                self.cancel_pending();
                return Some(EngineResult::Executed);
            };
            let motion_count_typed = self.motion_count.take();
            let motion_count = motion_count_typed.unwrap_or(NonZeroUsize::MIN);
            self.finish_pending();
            let total = pending.count.get() * motion_count.get();
            let ch = key.char().expect("checked above");
            self.apply_operator_object(editor, view_id, doc_id, &pending, object, kind, total);
            self.last_action = Some(RecordedAction::OperatorMotion {
                operator: pending.operator,
                target: OperatorTargetId::Object(ch, kind),
                motion_count,
                operator_count: pending.count,
                count_given: pending.count_given || motion_count_typed.is_some(),
                register: pending.register,
            });
            return Some(EngineResult::Executed);
        }

        // A key the pending sequence (`dg` waiting for `g`) takes is not a count.
        let sequence_pending =
            !keymaps.pending().is_empty() && keymaps.contains_key(editor.mode(), key);
        if let Some(digit) = key_to_digit(key).filter(|_| !sequence_pending) {
            if self.motion_count.is_some() || digit > 0 {
                let current = self.motion_count.map_or(0, NonZeroUsize::get);
                let new = current * 10 + digit;
                if new <= 100_000_000 {
                    self.motion_count = NonZeroUsize::new(new);
                }
                return Some(EngineResult::Pending);
            }
        }

        if keymaps.pending().is_empty() {
            if is_char_key(key, 'i') || is_char_key(key, 'a') {
                let mut pending = pending;
                pending.text_object_kind = Some(if is_char_key(key, 'i') {
                    TextObject::Inside
                } else {
                    TextObject::Around
                });
                self.sub_mode = SubMode::OperatorPending(pending);
                return Some(EngineResult::Pending);
            }

            // Doubled operator = linewise (dd, yy, cc, >>, guu)
            if self.is_same_operator_key(key, &pending) {
                let motion_count_typed = self.motion_count.take();
                let motion_count = motion_count_typed.unwrap_or(NonZeroUsize::MIN);
                let total_count = pending.count.get() * motion_count.get();
                self.finish_pending();
                self.apply_linewise_operator(editor, view_id, doc_id, &pending, total_count);
                self.last_action = Some(RecordedAction::OperatorMotion {
                    operator: pending.operator,
                    target: OperatorTargetId::Linewise,
                    motion_count,
                    operator_count: pending.count,
                    count_given: pending.count_given || motion_count_typed.is_some(),
                    register: pending.register,
                });
                return Some(EngineResult::Executed);
            }
        }

        None // let the frontend resolve the keymap, then call process_lookup
    }

    /// Visual mode pre-resolve: escape, registers, counts, `i`/`a` text objects.
    fn pre_resolve_visual(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult> {
        if key.code == KeyCode::Esc {
            self.exit_visual(editor, view_id, doc_id);
            return Some(EngineResult::Executed);
        }
        if let Some(result) = self.take_register_key(key) {
            return Some(result);
        }

        if let Some(kind) = self.visual_text_object.take() {
            if let Some(object) = key
                .char()
                .filter(|_| key.modifiers.is_empty())
                .and_then(vim_object)
            {
                let count = self.count.take().map_or(1, NonZeroUsize::get);
                // A text object selects characters, whatever the visual mode was.
                self.visual = None;
                self.sub_mode = SubMode::Visual;
                select_vim_object(editor, view_id, doc_id, object, kind, count);
            }
            return Some(EngineResult::Executed);
        }

        if !keymaps.pending().is_empty() {
            return None;
        }
        if let Some(digit) = key_to_digit(key) {
            if self.count.is_some() || digit > 0 {
                let current = self.count.map_or(0, NonZeroUsize::get);
                self.count = NonZeroUsize::new(current * 10 + digit);
                return Some(EngineResult::Pending);
            }
        }
        if is_char_key(key, '"') {
            self.awaiting_register = true;
            return Some(EngineResult::Pending);
        }
        if is_char_key(key, 'i') || is_char_key(key, 'a') {
            self.visual_text_object = Some(if is_char_key(key, 'i') {
                TextObject::Inside
            } else {
                TextObject::Around
            });
            return Some(EngineResult::Pending);
        }
        None
    }

    /// The key after a directly bound char-pending command.
    fn resolve_awaited_char(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        waiting: AwaitingChar,
        key: KeyEvent,
    ) -> EngineResult {
        if key.code == KeyCode::Esc {
            if waiting.operator.is_some() {
                self.cancel_pending();
            }
            return EngineResult::Executed;
        }
        match waiting.operator {
            Some(pending) => {
                self.finish_pending();
                self.apply_char_pending_operator(
                    editor,
                    view_id,
                    doc_id,
                    &pending,
                    waiting.command,
                    key,
                    waiting.count,
                );
                self.last_action = Some(RecordedAction::OperatorMotion {
                    operator: pending.operator,
                    target: OperatorTargetId::CharPending(waiting.command, key),
                    motion_count: NonZeroUsize::new(waiting.count / pending.count.get())
                        .unwrap_or(NonZeroUsize::MIN),
                    operator_count: pending.count,
                    count_given: pending.count_given,
                    register: pending.register,
                });
            }
            None => self.execute_char_pending(
                editor,
                view_id,
                doc_id,
                waiting.command,
                key,
                waiting.count,
                waiting.register,
            ),
        }
        EngineResult::Executed
    }

    // ─── Process lookup: execute pre-resolved keymap result ──────────

    /// Normal mode: execute the pre-resolved keymap lookup.
    fn process_lookup_normal(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        lookup: KeymapLookup,
    ) -> EngineResult {
        let count = self.count.take();
        let count_val = count.map_or(1, NonZeroUsize::get);
        let register = self.register.take();

        editor.autoinfo = keymaps.sticky_infobox();

        match lookup {
            KeymapLookup::Matched(command) => {
                self.dispatch_normal(editor, view_id, doc_id, command, count, register)
            }
            KeymapLookup::MatchedSequence(ref commands) => {
                for &command in commands.iter() {
                    self.dispatch_normal(editor, view_id, doc_id, command, count, register);
                }
                EngineResult::Executed
            }
            KeymapLookup::Pending(infobox) => {
                // Don't consume count/register for pending — put them back.
                self.count = count;
                self.register = register;
                if let Some(info) = infobox {
                    editor.autoinfo = Some(info);
                }
                EngineResult::Pending
            }
            KeymapLookup::NotFound => EngineResult::Unbound,
            KeymapLookup::Cancelled(_) => EngineResult::Executed,
            KeymapLookup::Fallback(command, key) => {
                self.execute_char_pending(
                    editor, view_id, doc_id, command, key, count_val, register,
                );
                EngineResult::Executed
            }
        }
    }

    /// Operator-pending mode: execute the pre-resolved keymap lookup as motion/text-object.
    fn process_lookup_operator_pending(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        lookup: KeymapLookup,
    ) -> EngineResult {
        let pending = match &self.sub_mode {
            SubMode::OperatorPending(p) => p.clone(),
            _ => unreachable!(),
        };

        let motion_count = self.motion_count.take();
        let motion_count_nz = motion_count.unwrap_or(NonZeroUsize::MIN);
        let total_count = pending.count.get() * motion_count_nz.get();
        let count_given = pending.count_given || motion_count.is_some();

        match lookup {
            KeymapLookup::Matched(command) => self.resolve_operator_target(
                editor,
                view_id,
                doc_id,
                command,
                &pending,
                motion_count_nz,
                total_count,
                count_given,
            ),
            KeymapLookup::Pending(infobox) => {
                if let Some(info) = infobox {
                    editor.autoinfo = Some(info);
                }
                // Put motion_count back since we didn't consume it.
                self.motion_count = motion_count;
                EngineResult::Pending
            }
            KeymapLookup::Fallback(command, key) => {
                self.finish_pending();
                self.apply_char_pending_operator(
                    editor,
                    view_id,
                    doc_id,
                    &pending,
                    command,
                    key,
                    total_count,
                );
                self.last_action = Some(RecordedAction::OperatorMotion {
                    operator: pending.operator,
                    target: OperatorTargetId::CharPending(command, key),
                    motion_count: motion_count_nz,
                    operator_count: pending.count,
                    count_given,
                    register: pending.register,
                });
                EngineResult::Executed
            }
            _ => {
                self.cancel_pending();
                EngineResult::Unbound
            }
        }
    }

    /// Visual mode: execute the pre-resolved keymap lookup.
    fn process_lookup_visual(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        lookup: KeymapLookup,
    ) -> EngineResult {
        let count = self.count.take();
        let count_val = count.map_or(1, NonZeroUsize::get);
        let register = self.register.take();

        editor.autoinfo = keymaps.sticky_infobox();

        match lookup {
            KeymapLookup::Matched(command) => {
                self.dispatch_visual(editor, view_id, doc_id, command, count, register)
            }
            KeymapLookup::MatchedSequence(ref commands) => {
                for &command in commands.iter() {
                    self.dispatch_visual(editor, view_id, doc_id, command, count, register);
                }
                EngineResult::Executed
            }
            KeymapLookup::Pending(infobox) => {
                self.count = count;
                self.register = register;
                if let Some(info) = infobox {
                    editor.autoinfo = Some(info);
                }
                EngineResult::Pending
            }
            KeymapLookup::Fallback(command, key) => {
                self.execute_char_pending(
                    editor, view_id, doc_id, command, key, count_val, register,
                );
                EngineResult::Executed
            }
            _ => EngineResult::Unbound,
        }
    }

    /// Insert mode: execute the pre-resolved keymap lookup. Motions (arrows, Home, End) move.
    fn process_lookup_insert(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        key: KeyEvent,
        lookup: KeymapLookup,
    ) -> EngineResult {
        editor.autoinfo = keymaps.sticky_infobox();

        let result = match lookup {
            KeymapLookup::Matched(command) => {
                self.run_insert_command(editor, view_id, doc_id, command);
                EngineResult::Executed
            }
            KeymapLookup::MatchedSequence(ref commands) => {
                for &command in commands.iter() {
                    self.run_insert_command(editor, view_id, doc_id, command);
                }
                EngineResult::Executed
            }
            KeymapLookup::Pending(infobox) => {
                if let Some(info) = infobox {
                    editor.autoinfo = Some(info);
                }
                EngineResult::Pending
            }
            KeymapLookup::NotFound => match key.char() {
                Some(ch) => EngineResult::InsertChar(ch),
                None => EngineResult::Unbound,
            },
            KeymapLookup::Cancelled(pending_keys) => EngineResult::CancelledInsert(pending_keys),
            _ => EngineResult::Unbound,
        };

        record_insert_key(&mut self.insert_recording, key, &result);
        result
    }

    fn run_insert_command(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        command: CommandToken,
    ) {
        self.last_command = Some(command);
        match self.registry.resolve(command) {
            Some(CommandRef::Action(a)) => (a.execute)(editor, view_id, doc_id, 1, None),
            Some(CommandRef::Motion(m)) => {
                m.make.make(None)(editor, view_id, doc_id, Movement::Move)
            }
            _ => {}
        }
    }

    // ─── Shared helpers ──────────────────────────────────────────────

    /// Dispatch a command in normal mode — may enter operator-pending or visual mode.
    fn dispatch_normal(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        command: CommandToken,
        count: Option<NonZeroUsize>,
        register: Option<char>,
    ) -> EngineResult {
        let Some(kind) = self.registry.resolve(command) else {
            return EngineResult::Unbound;
        };
        self.last_command = Some(command);
        let count_val = count.map_or(1, NonZeroUsize::get);

        match kind {
            CommandRef::Operator(op) => {
                self.sub_mode = SubMode::OperatorPending(PendingOp {
                    operator: op.id,
                    register,
                    count: NonZeroUsize::new(count_val).unwrap_or(NonZeroUsize::MIN),
                    count_given: count.is_some(),
                    text_object_kind: None,
                });
                self.update_pending_display();
                EngineResult::Pending
            }
            CommandRef::Motion(m) => {
                let motion = m.make.make(count);
                motion(editor, view_id, doc_id, Movement::Move);
                keep_cursor_off_line_break(editor, view_id, doc_id);
                EngineResult::Executed
            }
            CommandRef::Action(a) => {
                let cursor = primary_cursor(editor, view_id, doc_id);
                let version_before = document_version(editor, doc_id);
                (a.execute)(editor, view_id, doc_id, count_val, register);
                match a.id.as_str() {
                    "vim_visual_line" => {
                        let doc = helix_view::doc!(editor, &doc_id);
                        let text = doc.text().slice(..);
                        let line = (text.char_to_line(cursor) + count_val - 1)
                            .min(text.len_lines().saturating_sub(1));
                        let end_cursor = if count_val > 1 {
                            text.line_to_char(line)
                        } else {
                            cursor
                        };
                        self.enter_visual_span(
                            editor,
                            view_id,
                            doc_id,
                            SubMode::VisualLine,
                            cursor,
                            end_cursor,
                        );
                    }
                    "vim_visual_block" => self.enter_visual_span(
                        editor,
                        view_id,
                        doc_id,
                        SubMode::VisualBlock,
                        cursor,
                        cursor,
                    ),
                    id => {
                        if matches!(id, "paste_after" | "paste_before") {
                            cursor_after_paste(editor, view_id, doc_id);
                        }
                        if editor.mode() == Mode::Normal {
                            keep_cursor_off_line_break(editor, view_id, doc_id);
                        }
                        if is_repeatable_edit(
                            a.id.as_str(),
                            version_before,
                            document_version(editor, doc_id),
                        ) {
                            self.last_action = Some(RecordedAction::CountedAction {
                                command: RepeatableCommandId::Action(a.id),
                                count: NonZeroUsize::new(count_val).unwrap_or(NonZeroUsize::MIN),
                                register,
                            });
                        }
                    }
                }
                EngineResult::Executed
            }
            CommandRef::TextObject(_) => EngineResult::Unbound,
            CommandRef::CharPending(cp) => {
                self.awaiting_char = Some(AwaitingChar {
                    command: cp.id,
                    count: count_val,
                    register,
                    operator: None,
                });
                EngineResult::Pending
            }
        }
    }

    /// Dispatch a command in visual mode: motions move the selection's cursor, operators act
    /// on the selection and leave visual mode.
    fn dispatch_visual(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        command: CommandToken,
        count: Option<NonZeroUsize>,
        register: Option<char>,
    ) -> EngineResult {
        let Some(kind) = self.registry.resolve(command) else {
            return EngineResult::Unbound;
        };
        self.last_command = Some(command);
        let count_val = count.map_or(1, NonZeroUsize::get);

        match kind {
            CommandRef::Motion(m) => {
                let motion = m.make.make(count);
                self.visual_motion(editor, view_id, doc_id, |editor, movement| {
                    motion(editor, view_id, doc_id, movement)
                });
                EngineResult::Executed
            }
            CommandRef::Operator(op) => {
                (op.execute)(editor, view_id, doc_id, register);
                self.after_operator(editor, view_id, doc_id, op.id);
                self.leave_visual_after_edit(editor);
                EngineResult::Executed
            }
            CommandRef::TextObject(to) => {
                let obj_fn = (to.make)(count_val);
                self.visual = None;
                self.sub_mode = SubMode::Visual;
                obj_fn(editor, view_id, doc_id, TextObject::Around);
                EngineResult::Executed
            }
            CommandRef::Action(a) => {
                match (a.id.as_str(), &self.sub_mode) {
                    ("vim_visual_line", SubMode::VisualLine)
                    | ("vim_visual_block", SubMode::VisualBlock)
                    | ("vim_visual_char", SubMode::Visual) => {
                        self.exit_visual(editor, view_id, doc_id);
                    }
                    ("vim_visual_line" | "vim_visual_block" | "vim_visual_char", _) => {
                        self.switch_visual(editor, view_id, doc_id, a.id.as_str());
                    }
                    ("flip_selections", SubMode::VisualLine | SubMode::VisualBlock) => {
                        if let Some(span) = &mut self.visual {
                            std::mem::swap(&mut span.anchor, &mut span.cursor);
                        }
                        self.rebuild_visual(editor, view_id, doc_id);
                    }
                    _ => {
                        let version_before = document_version(editor, doc_id);
                        (a.execute)(editor, view_id, doc_id, count_val, register);
                        if version_before != document_version(editor, doc_id) {
                            // An edit (`J`, `p`, `r`) ends visual mode, as in Vim.
                            self.leave_visual_after_edit(editor);
                        }
                    }
                }
                EngineResult::Executed
            }
            CommandRef::CharPending(cp) => {
                self.awaiting_char = Some(AwaitingChar {
                    command: cp.id,
                    count: count_val,
                    register,
                    operator: None,
                });
                EngineResult::Pending
            }
        }
    }

    /// Resolve the target of an operator (motion or text object).
    #[allow(clippy::too_many_arguments)]
    fn resolve_operator_target(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        command: CommandToken,
        pending: &PendingOp,
        motion_count: NonZeroUsize,
        total_count: usize,
        count_given: bool,
    ) -> EngineResult {
        let registry = Arc::clone(&self.registry);
        let Some(kind) = registry.resolve(command) else {
            self.cancel_pending();
            return EngineResult::Unbound;
        };

        match kind {
            CommandRef::Motion(m) => {
                self.finish_pending();
                let count = count_given
                    .then(|| NonZeroUsize::new(total_count))
                    .flatten();
                self.apply_operator_motion(editor, view_id, doc_id, pending, m, count);
                self.last_action = Some(RecordedAction::OperatorMotion {
                    operator: pending.operator,
                    target: OperatorTargetId::Motion(m.id),
                    motion_count,
                    operator_count: pending.count,
                    count_given,
                    register: pending.register,
                });
                EngineResult::Executed
            }
            CommandRef::TextObject(to) => {
                self.finish_pending();
                let kind = pending.text_object_kind.unwrap_or(TextObject::Inside);
                self.apply_operator_text_object(
                    editor,
                    view_id,
                    doc_id,
                    pending,
                    to.id,
                    kind,
                    total_count,
                );
                self.last_action = Some(RecordedAction::OperatorMotion {
                    operator: pending.operator,
                    target: OperatorTargetId::TextObject(to.id, kind),
                    motion_count,
                    operator_count: pending.count,
                    count_given,
                    register: pending.register,
                });
                EngineResult::Executed
            }
            CommandRef::CharPending(cp) => {
                self.awaiting_char = Some(AwaitingChar {
                    command: cp.id,
                    count: total_count,
                    register: pending.register,
                    operator: Some(pending.clone()),
                });
                EngineResult::Pending
            }
            CommandRef::Operator(_) | CommandRef::Action(_) => {
                self.cancel_pending();
                EngineResult::Unbound
            }
        }
    }

    /// Run `operator` over what `motion` covers from the cursor. `cw` changes to the end of
    /// the word (Vim's special case), and motions between lines act on whole lines.
    fn apply_operator_motion(
        &self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        pending: &PendingOp,
        motion: &MotionEntry,
        count: Option<NonZeroUsize>,
    ) {
        let motion = if is_change(pending.operator) {
            let end_of_word = match motion.id.as_str() {
                "move_next_word_start" => Some("move_next_word_end"),
                "move_next_long_word_start" => Some("move_next_long_word_end"),
                _ => None,
            };
            end_of_word
                .and_then(|id| self.registry.motion(MotionId::new(id)))
                .unwrap_or(motion)
        } else {
            motion
        };
        let cursors = collapse_to_cursor(editor, view_id, doc_id);
        motion.make.make(count)(editor, view_id, doc_id, Movement::Extend);
        if is_linewise(motion.id) {
            expand_to_lines(editor, view_id, doc_id, is_delete(pending.operator));
        } else {
            exclude_motion_end(
                editor,
                view_id,
                doc_id,
                &cursors,
                motion.id.as_str() == "move_char_right",
            );
            if matches!(
                motion.id.as_str(),
                "move_next_word_start" | "move_next_long_word_start"
            ) {
                stop_word_motion_at_line_end(editor, view_id, doc_id);
            }
        }
        self.run_operator(editor, view_id, doc_id, pending.operator, pending.register);
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_operator_text_object(
        &self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        pending: &PendingOp,
        text_object: TextObjectId,
        kind: TextObject,
        count: usize,
    ) {
        let Some(to) = self.registry.text_object(text_object) else {
            return;
        };
        collapse_to_cursor(editor, view_id, doc_id);
        (to.make)(count)(editor, view_id, doc_id, kind);
        self.run_operator(editor, view_id, doc_id, pending.operator, pending.register);
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_operator_object(
        &self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        pending: &PendingOp,
        object: VimObject,
        kind: TextObject,
        count: usize,
    ) {
        collapse_to_cursor(editor, view_id, doc_id);
        select_vim_object(editor, view_id, doc_id, object, kind, count);
        self.run_operator(editor, view_id, doc_id, pending.operator, pending.register);
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_char_pending_operator(
        &self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        pending: &PendingOp,
        command: CharPendingId,
        key: KeyEvent,
        count: usize,
    ) {
        let Some(cp) = self.registry.char_pending(command) else {
            return;
        };
        match (cp.resolve)(key, count) {
            CharPendingResolution::Motion(motion) => {
                let cursors = collapse_to_cursor(editor, view_id, doc_id);
                motion(editor, view_id, doc_id, Movement::Extend);
                exclude_motion_end(editor, view_id, doc_id, &cursors, false);
                self.run_operator(editor, view_id, doc_id, pending.operator, pending.register);
            }
            CharPendingResolution::Action(action) => {
                action(editor, view_id, doc_id, pending.register);
            }
        }
    }

    /// Execute a named operator on the current selection, then place the cursor as Vim does.
    fn run_operator(
        &self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        operator: OperatorId,
        register: Option<char>,
    ) {
        let Some(op) = self.registry.operator(operator) else {
            log::warn!("Unknown operator: {operator}");
            return;
        };
        (op.execute)(editor, view_id, doc_id, register);
        self.after_operator(editor, view_id, doc_id, operator);
    }

    fn after_operator(
        &self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        operator: OperatorId,
    ) {
        if editor.mode() == Mode::Insert {
            return;
        }
        if vim_operator_behavior(operator).is_some_and(|b| b.cursor_to_start) {
            collapse_to_start(editor, view_id, doc_id);
        }
        keep_cursor_off_line_break(editor, view_id, doc_id);
    }

    /// Linewise operator (dd, yy, cc, >>, <<, guu).
    fn apply_linewise_operator(
        &self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        pending: &PendingOp,
        count: usize,
    ) {
        {
            let doc = helix_view::doc_mut!(editor, &doc_id);
            let text = doc.text().slice(..);
            let selection = doc.selection(view_id).clone().transform(|range| {
                let line = text.char_to_line(range.cursor(text));
                let last = (line + count - 1).min(text.len_lines().saturating_sub(1));
                let end = text.line_to_char((last + 1).min(text.len_lines()));
                Range::new(text.line_to_char(line), end.max(text.line_to_char(line)))
            });
            doc.set_selection(view_id, selection);
        }
        // `cc` keeps the line break: it replaces the lines' text.
        if is_change(pending.operator) {
            let doc = helix_view::doc_mut!(editor, &doc_id);
            let text = doc.text().slice(..);
            let selection = doc.selection(view_id).clone().transform(|range| {
                let last = text.char_to_line(range.to().saturating_sub(1).max(range.from()));
                let end = helix_core::line_ending::line_end_char_index(&text, last);
                Range::new(range.from(), end)
            });
            doc.set_selection(view_id, selection);
        } else {
            expand_to_lines(editor, view_id, doc_id, is_delete(pending.operator));
        }
        self.run_operator(editor, view_id, doc_id, pending.operator, pending.register);
    }

    /// Check if a key is the same operator as pending (for doubled operators).
    fn is_same_operator_key(&self, key: KeyEvent, pending: &PendingOp) -> bool {
        match key.char() {
            Some(ch) if key.modifiers.is_empty() || ch.is_ascii_uppercase() || ch == '~' => {
                vim_operator_behavior(pending.operator).is_some_and(|b| ch == b.doubled_key)
            }
            _ => false,
        }
    }

    /// Execute a char-pending command (find_char, etc.). Motions are recorded so `A-.`
    /// repeats them.
    #[allow(
        clippy::too_many_arguments,
        reason = "modal command dispatch carries editor, view, document, key, count, and register context"
    )]
    fn execute_char_pending(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        command: CharPendingId,
        key: KeyEvent,
        count: usize,
        register: Option<char>,
    ) {
        let Some(cp) = self.registry.char_pending(command) else {
            return;
        };
        match (cp.resolve)(key, count) {
            CharPendingResolution::Motion(motion) => {
                let motion: Arc<dyn Fn(&mut Editor, ViewId, DocumentId, Movement) + Send + Sync> =
                    Arc::from(motion);
                if matches!(self.sub_mode, SubMode::VisualLine | SubMode::VisualBlock) {
                    self.visual_motion(editor, view_id, doc_id, |editor, movement| {
                        motion(editor, view_id, doc_id, movement)
                    });
                } else {
                    let movement = if self.sub_mode.is_visual() {
                        Movement::Extend
                    } else {
                        Movement::Move
                    };
                    editor.apply_motion_in(view_id, doc_id, move |editor, view_id, doc_id| {
                        motion(editor, view_id, doc_id, movement)
                    });
                }
            }
            CharPendingResolution::Action(action) => {
                let version_before = document_version(editor, doc_id);
                action(editor, view_id, doc_id, register);
                if self.sub_mode.is_visual() && version_before != document_version(editor, doc_id) {
                    self.leave_visual_after_edit(editor);
                }
            }
        }
    }

    // ─── Visual modes ────────────────────────────────────────────────

    /// Apply a motion in visual mode. Characterwise visual extends the selection; linewise
    /// and blockwise visual move the span's cursor and rebuild the selection around it.
    fn visual_motion(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        motion: impl FnOnce(&mut Editor, Movement),
    ) {
        let Some(span) = self
            .visual
            .filter(|_| matches!(self.sub_mode, SubMode::VisualLine | SubMode::VisualBlock))
        else {
            motion(editor, Movement::Extend);
            return;
        };
        helix_view::doc_mut!(editor, &doc_id).set_selection(view_id, Selection::point(span.cursor));
        motion(editor, Movement::Move);
        let cursor = primary_cursor(editor, view_id, doc_id);
        if let Some(span) = &mut self.visual {
            span.cursor = cursor;
        }
        self.rebuild_visual(editor, view_id, doc_id);
    }

    fn enter_visual_span(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        mode: SubMode,
        anchor: usize,
        cursor: usize,
    ) {
        editor.mode = Mode::Select;
        self.sub_mode = mode;
        self.visual = Some(VisualSpan {
            view: view_id,
            anchor,
            cursor,
        });
        self.rebuild_visual(editor, view_id, doc_id);
    }

    /// `v`, `V` or `C-v` in another visual mode switches to that one, keeping both ends.
    fn switch_visual(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        to: &str,
    ) {
        let (anchor, cursor) = match self.visual {
            Some(span) => (span.anchor, span.cursor),
            None => {
                let doc = helix_view::doc!(editor, &doc_id);
                let text = doc.text().slice(..);
                let primary = doc.selection(view_id).primary();
                let anchor = if primary.anchor <= primary.head {
                    primary.anchor
                } else {
                    helix_core::graphemes::prev_grapheme_boundary(text, primary.anchor)
                };
                (anchor, primary.cursor(text))
            }
        };
        match to {
            "vim_visual_line" => {
                self.enter_visual_span(editor, view_id, doc_id, SubMode::VisualLine, anchor, cursor)
            }
            "vim_visual_block" => self.enter_visual_span(
                editor,
                view_id,
                doc_id,
                SubMode::VisualBlock,
                anchor,
                cursor,
            ),
            _ => {
                self.visual = None;
                self.sub_mode = SubMode::Visual;
                editor.mode = Mode::Select;
                let doc = helix_view::doc_mut!(editor, &doc_id);
                let text = doc.text().slice(..);
                let range = Range::point(anchor).put_cursor(text, cursor, true);
                doc.set_selection(view_id, Selection::single(range.anchor, range.head));
            }
        }
    }

    /// Rebuild a linewise or blockwise visual selection from its span.
    fn rebuild_visual(&self, editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
        let Some(span) = self.visual else {
            return;
        };
        let doc = helix_view::doc_mut!(editor, &doc_id);
        let text = doc.text().slice(..);
        let len = text.len_chars();
        let (anchor, cursor) = (span.anchor.min(len), span.cursor.min(len));
        let (anchor_line, cursor_line) = (text.char_to_line(anchor), text.char_to_line(cursor));
        let (first, last) = (anchor_line.min(cursor_line), anchor_line.max(cursor_line));
        let selection = match self.sub_mode {
            SubMode::VisualLine => {
                let start = text.line_to_char(first);
                let end = text.line_to_char((last + 1).min(text.len_lines()));
                if cursor_line >= anchor_line {
                    Selection::single(start, end)
                } else {
                    Selection::single(end, start)
                }
            }
            SubMode::VisualBlock => {
                let anchor_col = anchor - text.line_to_char(anchor_line);
                let cursor_col = cursor - text.line_to_char(cursor_line);
                let (left, right) = (anchor_col.min(cursor_col), anchor_col.max(cursor_col));
                let mut ranges: Vec<Range> = Vec::new();
                let mut primary = 0;
                for line in first..=last {
                    let line_start = text.line_to_char(line);
                    let line_end = helix_core::line_ending::line_end_char_index(&text, line);
                    let start = line_start + left;
                    // Lines too short to reach the block have no part in it.
                    if start > line_end || (start == line_end && line != cursor_line) {
                        continue;
                    }
                    let end = (line_start + right + 1).min(line_end).max(start);
                    if line == cursor_line {
                        primary = ranges.len();
                    }
                    ranges.push(if cursor_col >= anchor_col {
                        Range::new(start, end)
                    } else {
                        Range::new(end, start)
                    });
                }
                if ranges.is_empty() {
                    ranges.push(Range::point(cursor));
                }
                Selection::new(ranges.into(), primary)
            }
            _ => return,
        };
        doc.set_selection(view_id, selection);
    }

    /// Leave visual mode with the cursor where the selection's cursor was.
    fn exit_visual(&mut self, editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
        let cursor = match self.visual {
            Some(span) => Some(span.cursor),
            None => None,
        };
        {
            let doc = helix_view::doc_mut!(editor, &doc_id);
            let text = doc.text().slice(..);
            let selection = match cursor {
                Some(cursor) => Selection::point(cursor.min(text.len_chars())),
                None => doc
                    .selection(view_id)
                    .clone()
                    .transform(|range| Range::point(range.cursor(text))),
            };
            doc.set_selection(view_id, selection);
        }
        editor.mode = Mode::Normal;
        self.sub_mode = SubMode::Normal;
        self.visual = None;
        self.visual_text_object = None;
        self.count = None;
    }

    /// An edit or operator ran on the visual selection: back to normal mode, or on into insert
    /// mode for `c`.
    fn leave_visual_after_edit(&mut self, editor: &mut Editor) {
        let was_block = matches!(self.sub_mode, SubMode::VisualBlock);
        self.visual = None;
        self.visual_text_object = None;
        if editor.mode() == Mode::Insert {
            // A block change keeps a cursor on every line, so the typed text goes on each.
            self.sub_mode = SubMode::Insert;
            return;
        }
        editor.mode = Mode::Normal;
        self.sub_mode = SubMode::Normal;
        let view_id = editor.tree.focus;
        let Some(doc_id) = editor.tree.try_get(view_id).map(|view| view.doc) else {
            return;
        };
        if was_block {
            // Back to one cursor, at the block's top left.
            let doc = helix_view::doc_mut!(editor, &doc_id);
            let start = doc.selection(view_id).ranges()[0].from();
            doc.set_selection(view_id, Selection::point(start));
        }
        keep_cursor_off_line_break(editor, view_id, doc_id);
    }

    // ─── Pending state ───────────────────────────────────────────────

    /// The operator got its target.
    fn finish_pending(&mut self) {
        self.sub_mode = SubMode::Normal;
        self.pending_display_buf.clear();
    }

    /// The operator was abandoned (`Esc`, or a key that isn't a target).
    fn cancel_pending(&mut self) {
        self.sub_mode = SubMode::Normal;
        self.motion_count = None;
        self.awaiting_char = None;
        self.pending_display_buf.clear();
    }

    /// Update the pending display buffer for statusline.
    fn update_pending_display(&mut self) {
        self.pending_display_buf.clear();
        if let Some(count) = self.count {
            use std::fmt::Write;
            let _ = write!(self.pending_display_buf, "{count}");
        }
        if let SubMode::OperatorPending(ref pending) = self.sub_mode {
            let display = vim_operator_behavior(pending.operator)
                .map(|b| b.pending_display)
                .unwrap_or_else(|| pending.operator.as_str());
            self.pending_display_buf.push_str(display);
        }
    }
}

impl EditingEngine for VimEngine {
    fn pre_resolve(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult> {
        self.sync_mode(editor, view_id);
        if let Some(waiting) = self.awaiting_char.take() {
            return Some(self.resolve_awaited_char(editor, view_id, doc_id, waiting, key));
        }
        match &self.sub_mode {
            SubMode::Normal => self.pre_resolve_normal(editor, view_id, doc_id, keymaps, key),
            SubMode::OperatorPending(_) => {
                self.pre_resolve_operator_pending(editor, view_id, doc_id, keymaps, key)
            }
            SubMode::Visual | SubMode::VisualLine | SubMode::VisualBlock => {
                self.pre_resolve_visual(editor, view_id, doc_id, keymaps, key)
            }
            SubMode::Insert => None,
        }
    }

    fn process_lookup(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &mut dyn KeymapQuery,
        key: KeyEvent,
        lookup: KeymapLookup,
    ) -> EngineResult {
        self.sync_mode(editor, view_id);
        self.last_command = None;
        match &self.sub_mode {
            SubMode::Normal => self.process_lookup_normal(editor, view_id, doc_id, keymaps, lookup),
            SubMode::OperatorPending(_) => {
                self.process_lookup_operator_pending(editor, view_id, doc_id, lookup)
            }
            SubMode::Visual | SubMode::VisualLine | SubMode::VisualBlock => {
                self.process_lookup_visual(editor, view_id, doc_id, keymaps, lookup)
            }
            SubMode::Insert => {
                self.process_lookup_insert(editor, view_id, doc_id, keymaps, key, lookup)
            }
        }
    }

    fn mode_name(&self) -> &str {
        match self.sub_mode {
            SubMode::Normal => "NOR",
            SubMode::Insert => "INS",
            SubMode::Visual => "VIS",
            SubMode::VisualLine => "VLN",
            SubMode::VisualBlock => "VBL",
            SubMode::OperatorPending(_) => "OPR",
        }
    }

    fn pending_display(&self) -> &str {
        &self.pending_display_buf
    }

    fn is_pending(&self) -> bool {
        matches!(self.sub_mode, SubMode::OperatorPending(_))
            || self.count.is_some()
            || self.awaiting_register
            || self.awaiting_char.is_some()
            || self.visual_text_object.is_some()
    }

    fn reset(&mut self) {
        if matches!(self.sub_mode, SubMode::OperatorPending(_)) {
            self.sub_mode = SubMode::Normal;
        }
        self.count = None;
        self.motion_count = None;
        self.register = None;
        self.awaiting_register = false;
        self.awaiting_char = None;
        self.visual_text_object = None;
        self.pending_display_buf.clear();
    }

    fn name(&self) -> &str {
        "vim"
    }

    fn repeat_last(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        count: NonZeroUsize,
    ) -> EngineResult {
        let Some(action) = self.last_action.clone() else {
            return EngineResult::Executed;
        };

        match action {
            RecordedAction::CountedAction {
                command,
                count: original_count,
                register,
            } => {
                let effective = if count.get() > 1 {
                    count.get()
                } else {
                    original_count.get()
                };
                match command {
                    RepeatableCommandId::Action(action) => {
                        if let Some(a) = self.registry.action(action) {
                            (a.execute)(editor, view_id, doc_id, effective, register);
                        }
                    }
                    RepeatableCommandId::Operator(operator) => {
                        self.run_operator(editor, view_id, doc_id, operator, register);
                    }
                }
                EngineResult::Executed
            }
            RecordedAction::OperatorMotion {
                operator,
                target,
                motion_count,
                operator_count,
                count_given,
                register,
            } => {
                let count_given = count_given || count.get() > 1;
                let total = if count.get() > 1 {
                    count.get()
                } else {
                    operator_count.get() * motion_count.get()
                };
                let pending = PendingOp {
                    operator,
                    register,
                    count: NonZeroUsize::new(total).unwrap_or(NonZeroUsize::MIN),
                    count_given,
                    text_object_kind: None,
                };

                match target {
                    OperatorTargetId::Linewise => {
                        self.apply_linewise_operator(editor, view_id, doc_id, &pending, total);
                    }
                    OperatorTargetId::Motion(motion) => {
                        if let Some(m) = self.registry.motion(motion) {
                            let count = count_given.then(|| NonZeroUsize::new(total)).flatten();
                            self.apply_operator_motion(editor, view_id, doc_id, &pending, m, count);
                        }
                    }
                    OperatorTargetId::TextObject(text_object, kind) => {
                        self.apply_operator_text_object(
                            editor,
                            view_id,
                            doc_id,
                            &pending,
                            text_object,
                            kind,
                            total,
                        );
                    }
                    OperatorTargetId::Object(ch, kind) => {
                        if let Some(object) = vim_object(ch) {
                            self.apply_operator_object(
                                editor, view_id, doc_id, &pending, object, kind, total,
                            );
                        }
                    }
                    OperatorTargetId::CharPending(command, key) => {
                        self.apply_char_pending_operator(
                            editor, view_id, doc_id, &pending, command, key, total,
                        );
                    }
                }

                EngineResult::Executed
            }
            RecordedAction::InsertSequence {
                entry_command,
                keys,
            } => EngineResult::ReplayInsert {
                entry_command,
                keys,
            },
        }
    }

    fn begin_insert_recording(&mut self, entry_command: Cow<'static, str>) {
        self.sub_mode = SubMode::Insert;
        self.visual = None;
        self.insert_recording = Some(InsertRecording {
            entry_command,
            keys: Vec::new(),
        });
    }

    fn record_frontend_insert_key(&mut self, key: KeyEvent) {
        if let Some(recording) = &mut self.insert_recording {
            recording.keys.push(key);
        }
    }

    fn end_insert_recording(&mut self) {
        if let Some(action) = finalize_insert_recording(self.insert_recording.take()) {
            self.last_action = Some(action);
        }
        self.sub_mode = SubMode::Normal;
    }

    /// Leaving insert mode steps each cursor back onto the character before it, as in Vim.
    fn insert_exited(&mut self, editor: &mut Editor) {
        if editor.mode() != Mode::Normal {
            return;
        }
        let view_id = editor.tree.focus;
        let Some(doc_id) = editor.tree.try_get(view_id).map(|view| view.doc) else {
            return;
        };
        let doc = helix_view::doc_mut!(editor, &doc_id);
        let text = doc.text().slice(..);
        let selection = doc.selection(view_id).clone().transform(|range| {
            let cursor = range.cursor(text);
            let line_start = text.line_to_char(text.char_to_line(cursor));
            if cursor > line_start {
                Range::point(helix_core::graphemes::prev_grapheme_boundary(text, cursor))
            } else {
                Range::point(cursor)
            }
        });
        doc.set_selection(view_id, selection);
    }

    fn last_command_name(&self) -> Option<&'static str> {
        self.last_command.map(CommandToken::as_str)
    }

    fn input_state(&self) -> ModalInputState {
        ModalInputState {
            count: self.count,
            selected_register: self.register,
        }
    }

    fn set_input_state(&mut self, state: ModalInputState) {
        self.count = state.count;
        self.register = state.selected_register;
        self.update_pending_display();
    }
}
