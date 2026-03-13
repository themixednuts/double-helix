//! Vim editing engine — verb→object paradigm.
//!
//! Implements Vim's operator-pending composition: an operator key (`d`, `c`, `y`, etc.)
//! enters a pending state, then the next motion or text object defines the range.
//! Supports visual modes (characterwise, linewise, blockwise), replace mode,
//! and Vim-style dot-repeat with count multiplication.

use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::sync::Arc;

use helix_core::movement::Movement;
use helix_core::textobject::TextObject;
use helix_view::document::Mode;
use helix_view::engine::{
    CharPendingId, CommandToken, EditingEngine, EngineResult, KeymapLookup, KeymapQuery,
    ModalInputState, OperatorId, OperatorTargetId, RecordedAction, RepeatableCommandId,
};
use helix_view::input::KeyEvent;
use helix_view::{DocumentId, Editor, ViewId};

use crate::registry::{CommandRef, CommandRegistry};
use crate::{
    finalize_insert_recording, is_char_key, key_to_digit, record_insert_key, InsertRecording,
};

/// Vim's internal mode state machine.
///
/// Maps to editor `Mode` for cursor shape and gutter behavior,
/// but provides finer-grained states for composition.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants populated as mode transitions are wired up
enum SubMode {
    Normal,
    Insert,
    Replace,
    Visual,
    VisualLine,
    VisualBlock,
    OperatorPending(PendingOp),
}

/// State stored when an operator is pending (waiting for motion/text-object).
#[derive(Debug, Clone)]
struct PendingOp {
    operator: OperatorId,
    register: Option<char>,
    /// Operator-side count (the count before the operator key).
    count: NonZeroUsize,
    /// Whether the next text object should use `Inside` or `Around`.
    /// Set by `i` or `a` prefix in operator-pending mode.
    text_object_kind: Option<TextObject>,
}

/// Vim-specific operator behavior (local to VimEngine, not shared in registry).
struct VimOperatorBehavior {
    pending_display: &'static str,
    doubled_key: char,
}

/// Static table of Vim operator behaviors. Only 5 entries, searched linearly.
const VIM_OPERATORS: &[(OperatorId, VimOperatorBehavior)] = &[
    (
        OperatorId::new("delete_selection"),
        VimOperatorBehavior {
            pending_display: "d",
            doubled_key: 'd',
        },
    ),
    (
        OperatorId::new("delete_selection_noyank"),
        VimOperatorBehavior {
            pending_display: "d",
            doubled_key: 'd',
        },
    ),
    (
        OperatorId::new("change_selection"),
        VimOperatorBehavior {
            pending_display: "c",
            doubled_key: 'c',
        },
    ),
    (
        OperatorId::new("change_selection_noyank"),
        VimOperatorBehavior {
            pending_display: "c",
            doubled_key: 'c',
        },
    ),
    (
        OperatorId::new("yank"),
        VimOperatorBehavior {
            pending_display: "y",
            doubled_key: 'y',
        },
    ),
];

fn vim_operator_behavior(id: OperatorId) -> Option<&'static VimOperatorBehavior> {
    VIM_OPERATORS
        .iter()
        .find(|(op, _)| *op == id)
        .map(|(_, b)| b)
}

/// The Vim editing engine.
///
/// Uses verb→object paradigm: operators enter a pending state, then a motion
/// or text object completes the composition. Supports visual modes and
/// count multiplication (`2d3w` = delete 6 words).
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
        }
    }

    // ─── Pre-resolve: count/register/escape/dot-repeat (before keymap) ───

    /// Normal mode pre-resolve: count accumulation, register selection, dot-repeat.
    fn pre_resolve_normal(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult> {
        // Count accumulation
        if let Some(digit) = key_to_digit(key) {
            if self.count.is_some() || digit > 0 {
                let current = self.count.map_or(0, NonZeroUsize::get);
                let new = current * 10 + digit;
                if new <= 100_000_000 {
                    self.count = NonZeroUsize::new(new);
                }
                return Some(EngineResult::Pending);
            }
        }

        // Register selection: " prefix
        if is_char_key(key, '"') && self.register.is_none() {
            return Some(EngineResult::Pending);
        }

        // Dot-repeat
        if is_char_key(key, '.') && keymaps.pending().is_empty() {
            let count = self.count.take().unwrap_or(NonZeroUsize::MIN);
            return Some(self.repeat_last(editor, view_id, doc_id, count));
        }

        None
    }

    /// Operator-pending pre-resolve: escape, motion-side count, i/a prefix, doubled operator.
    fn pre_resolve_operator_pending(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        _keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult> {
        // Escape → cancel
        if key.code == helix_view::keyboard::KeyCode::Esc {
            self.sub_mode = SubMode::Normal;
            self.pending_display_buf.clear();
            return Some(EngineResult::Executed);
        }

        // Motion-side count accumulation (`d3w`)
        if let Some(digit) = key_to_digit(key) {
            if self.motion_count.is_some() || digit > 0 {
                let current = self.motion_count.map_or(0, NonZeroUsize::get);
                let new = current * 10 + digit;
                if new <= 100_000_000 {
                    self.motion_count = NonZeroUsize::new(new);
                }
                return Some(EngineResult::Pending);
            }
        }

        let pending = match &self.sub_mode {
            SubMode::OperatorPending(p) => p,
            _ => unreachable!(),
        };

        // `i` / `a` prefix for text objects
        if pending.text_object_kind.is_none() {
            if is_char_key(key, 'i') {
                let mut new_pending = pending.clone();
                new_pending.text_object_kind = Some(TextObject::Inside);
                self.sub_mode = SubMode::OperatorPending(new_pending);
                return Some(EngineResult::Pending);
            }
            if is_char_key(key, 'a') {
                let mut new_pending = pending.clone();
                new_pending.text_object_kind = Some(TextObject::Around);
                self.sub_mode = SubMode::OperatorPending(new_pending);
                return Some(EngineResult::Pending);
            }
        }

        // Doubled operator = linewise (dd, yy, cc)
        if self.is_same_operator_key(key, pending) {
            let pending = pending.clone();
            let motion_count = self.motion_count.take().unwrap_or(NonZeroUsize::MIN);
            let total_count = pending.count.get() * motion_count.get();
            self.sub_mode = SubMode::Normal;
            self.pending_display_buf.clear();
            self.apply_linewise_operator(editor, view_id, doc_id, &pending, total_count);
            self.last_action = Some(RecordedAction::OperatorMotion {
                operator: pending.operator,
                target: OperatorTargetId::Linewise,
                motion_count,
                operator_count: pending.count,
                register: pending.register,
            });
            return Some(EngineResult::Executed);
        }

        None // let the frontend resolve the keymap, then call process_lookup
    }

    /// Visual mode pre-resolve: escape, count accumulation.
    fn pre_resolve_visual(
        &mut self,
        editor: &mut Editor,
        _view_id: ViewId,
        _doc_id: DocumentId,
        _keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult> {
        if key.code == helix_view::keyboard::KeyCode::Esc {
            self.sub_mode = SubMode::Normal;
            editor.mode = Mode::Normal;
            return Some(EngineResult::Executed);
        }

        if let Some(digit) = key_to_digit(key) {
            if self.count.is_some() || digit > 0 {
                let current = self.count.map_or(0, NonZeroUsize::get);
                self.count = NonZeroUsize::new(current * 10 + digit);
                return Some(EngineResult::Pending);
            }
        }

        None
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
            KeymapLookup::Fallback(command, ch) => {
                self.execute_char_pending(editor, view_id, doc_id, command, ch, count_val);
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
        _keymaps: &dyn KeymapQuery,
        lookup: KeymapLookup,
    ) -> EngineResult {
        let pending = match &self.sub_mode {
            SubMode::OperatorPending(p) => p.clone(),
            _ => unreachable!(),
        };

        let motion_count = self.motion_count.take();
        let motion_count_nz = motion_count.unwrap_or(NonZeroUsize::MIN);
        let total_count = pending.count.get() * motion_count_nz.get();

        match lookup {
            KeymapLookup::Matched(command) => self.resolve_operator_target(
                editor,
                view_id,
                doc_id,
                command,
                &pending,
                motion_count_nz,
                total_count,
            ),
            KeymapLookup::Pending(infobox) => {
                // Still waiting for more keys — keep operator-pending.
                if let Some(info) = infobox {
                    editor.autoinfo = Some(info);
                }
                // Put motion_count back since we didn't consume it.
                self.motion_count = motion_count;
                EngineResult::Pending
            }
            KeymapLookup::Fallback(command, ch) => {
                self.sub_mode = SubMode::Normal;
                self.pending_display_buf.clear();

                if let Some(cp) = self.registry.char_pending(command) {
                    let motion = (cp.resolve)(ch, total_count);
                    motion(editor, view_id, doc_id, Movement::Extend);
                    self.execute_operator(
                        editor,
                        view_id,
                        doc_id,
                        pending.operator,
                        pending.register,
                    );
                    self.last_action = Some(RecordedAction::OperatorMotion {
                        operator: pending.operator,
                        target: OperatorTargetId::CharPending(command, ch),
                        motion_count: motion_count_nz,
                        operator_count: pending.count,
                        register: pending.register,
                    });
                }
                EngineResult::Executed
            }
            _ => {
                self.sub_mode = SubMode::Normal;
                self.pending_display_buf.clear();
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
                let Some(kind) = self.registry.resolve(command) else {
                    return EngineResult::Unbound;
                };

                match kind {
                    CommandRef::Motion(m) => {
                        let motion = m.make.make(count);
                        motion(editor, view_id, doc_id, Movement::Extend);
                        EngineResult::Executed
                    }
                    CommandRef::Operator(op) => {
                        (op.execute)(editor, view_id, doc_id, register);
                        self.sub_mode = SubMode::Normal;
                        editor.mode = Mode::Normal;
                        EngineResult::Executed
                    }
                    CommandRef::TextObject(to) => {
                        let obj_fn = (to.make)(count_val);
                        obj_fn(editor, view_id, doc_id, TextObject::Around);
                        EngineResult::Executed
                    }
                    CommandRef::Action(a) => {
                        (a.execute)(editor, view_id, doc_id, count_val, register);
                        EngineResult::Executed
                    }
                    CommandRef::CharPending(_) => EngineResult::Pending,
                }
            }
            KeymapLookup::Pending(infobox) => {
                // Put count/register back for pending.
                self.count = count;
                self.register = register;
                if let Some(info) = infobox {
                    editor.autoinfo = Some(info);
                }
                EngineResult::Pending
            }
            KeymapLookup::Fallback(command, ch) => {
                self.execute_char_pending(editor, view_id, doc_id, command, ch, count_val);
                EngineResult::Executed
            }
            _ => EngineResult::Unbound,
        }
    }

    /// Insert mode: execute the pre-resolved keymap lookup.
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
                if let Some(CommandRef::Action(a)) = self.registry.resolve(command) {
                    (a.execute)(editor, view_id, doc_id, 1, None);
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

    // ─── Shared helpers ──────────────────────────────────────────────

    /// Dispatch a command in normal mode — may enter operator-pending.
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
        let count_val = count.map_or(1, NonZeroUsize::get);

        match kind {
            CommandRef::Operator(op) => {
                self.sub_mode = SubMode::OperatorPending(PendingOp {
                    operator: op.id,
                    register,
                    count: NonZeroUsize::new(count_val).unwrap_or(NonZeroUsize::MIN),
                    text_object_kind: None,
                });
                self.update_pending_display();
                EngineResult::Pending
            }
            CommandRef::Motion(m) => {
                let motion = m.make.make(count);
                motion(editor, view_id, doc_id, Movement::Move);
                EngineResult::Executed
            }
            CommandRef::Action(a) => {
                (a.execute)(editor, view_id, doc_id, count_val, register);
                self.last_action = Some(RecordedAction::CountedAction {
                    command: RepeatableCommandId::Action(a.id),
                    count: NonZeroUsize::new(count_val).unwrap_or(NonZeroUsize::MIN),
                    register,
                });
                EngineResult::Executed
            }
            CommandRef::TextObject(_) => EngineResult::Unbound,
            CommandRef::CharPending(_) => EngineResult::Pending,
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
    ) -> EngineResult {
        let Some(kind) = self.registry.resolve(command) else {
            self.sub_mode = SubMode::Normal;
            self.pending_display_buf.clear();
            return EngineResult::Unbound;
        };

        match kind {
            CommandRef::Motion(m) => {
                self.sub_mode = SubMode::Normal;
                self.pending_display_buf.clear();

                let motion_fn = m.make.make(Some(
                    NonZeroUsize::new(total_count).unwrap_or(NonZeroUsize::MIN),
                ));
                motion_fn(editor, view_id, doc_id, Movement::Extend);
                self.execute_operator(editor, view_id, doc_id, pending.operator, pending.register);

                self.last_action = Some(RecordedAction::OperatorMotion {
                    operator: pending.operator,
                    target: OperatorTargetId::Motion(m.id),
                    motion_count,
                    operator_count: pending.count,
                    register: pending.register,
                });
                EngineResult::Executed
            }
            CommandRef::TextObject(to) => {
                self.sub_mode = SubMode::Normal;
                self.pending_display_buf.clear();

                let obj_kind = pending.text_object_kind.unwrap_or(TextObject::Inside);
                let obj_fn = (to.make)(total_count);
                obj_fn(editor, view_id, doc_id, obj_kind);
                self.execute_operator(editor, view_id, doc_id, pending.operator, pending.register);

                self.last_action = Some(RecordedAction::OperatorMotion {
                    operator: pending.operator,
                    target: OperatorTargetId::TextObject(to.id),
                    motion_count,
                    operator_count: pending.count,
                    register: pending.register,
                });
                EngineResult::Executed
            }
            CommandRef::CharPending(_) => EngineResult::Pending,
            _ => {
                self.sub_mode = SubMode::Normal;
                self.pending_display_buf.clear();
                EngineResult::Unbound
            }
        }
    }

    /// Execute a named operator on the current selection.
    fn execute_operator(
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
    }

    /// Linewise operator (dd, yy, cc, >>, <<, ==).
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
                let start = text.line_to_char(line);
                let end_line = (line + count).min(text.len_lines());
                let end = text.line_to_char(end_line);
                helix_core::Range::new(start, end)
            });
            doc.set_selection(view_id, selection);
        }

        self.execute_operator(editor, view_id, doc_id, pending.operator, pending.register);
    }

    /// Check if a key is the same operator as pending (for doubled operators).
    fn is_same_operator_key(&self, key: KeyEvent, pending: &PendingOp) -> bool {
        if let Some(ch) = key.char() {
            if key.modifiers.is_empty() {
                return vim_operator_behavior(pending.operator)
                    .is_some_and(|b| ch == b.doubled_key);
            }
        }
        false
    }

    /// Execute a char-pending command (find_char, etc.).
    fn execute_char_pending(
        &self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        command: CharPendingId,
        ch: char,
        count: usize,
    ) {
        if let Some(cp) = self.registry.char_pending(command) {
            let movement = if matches!(
                self.sub_mode,
                SubMode::Visual | SubMode::VisualLine | SubMode::VisualBlock
            ) {
                Movement::Extend
            } else {
                Movement::Move
            };
            let motion = (cp.resolve)(ch, count);
            motion(editor, view_id, doc_id, movement);
        }
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
        match &self.sub_mode {
            SubMode::Normal => self.pre_resolve_normal(editor, view_id, doc_id, keymaps, key),
            SubMode::OperatorPending(_) => {
                self.pre_resolve_operator_pending(editor, view_id, doc_id, keymaps, key)
            }
            SubMode::Visual | SubMode::VisualLine | SubMode::VisualBlock => {
                self.pre_resolve_visual(editor, view_id, doc_id, keymaps, key)
            }
            SubMode::Insert | SubMode::Replace => None, // insert has no pre-resolve
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
        match &self.sub_mode {
            SubMode::Normal => self.process_lookup_normal(editor, view_id, doc_id, keymaps, lookup),
            SubMode::OperatorPending(_) => {
                self.process_lookup_operator_pending(editor, view_id, doc_id, keymaps, lookup)
            }
            SubMode::Visual | SubMode::VisualLine | SubMode::VisualBlock => {
                self.process_lookup_visual(editor, view_id, doc_id, keymaps, lookup)
            }
            SubMode::Insert | SubMode::Replace => {
                self.process_lookup_insert(editor, view_id, doc_id, keymaps, key, lookup)
            }
        }
    }

    fn mode_name(&self) -> &str {
        match self.sub_mode {
            SubMode::Normal => "NOR",
            SubMode::Insert => "INS",
            SubMode::Replace => "REP",
            SubMode::Visual => "VIS",
            SubMode::VisualLine => "VLN",
            SubMode::VisualBlock => "VBL",
            SubMode::OperatorPending(_) => "OPR",
        }
    }

    fn editor_mode(&self) -> Mode {
        match self.sub_mode {
            SubMode::Normal | SubMode::OperatorPending(_) => Mode::Normal,
            SubMode::Insert | SubMode::Replace => Mode::Insert,
            SubMode::Visual | SubMode::VisualLine | SubMode::VisualBlock => Mode::Select,
        }
    }

    fn pending_display(&self) -> &str {
        &self.pending_display_buf
    }

    fn is_pending(&self) -> bool {
        matches!(self.sub_mode, SubMode::OperatorPending(_)) || self.count.is_some()
    }

    fn reset(&mut self) {
        self.sub_mode = SubMode::Normal;
        self.count = None;
        self.motion_count = None;
        self.register = None;
        self.pending_display_buf.clear();
    }

    fn name(&self) -> &str {
        "vim"
    }

    fn last_action(&self) -> Option<&RecordedAction> {
        self.last_action.as_ref()
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
                        self.execute_operator(editor, view_id, doc_id, operator, register);
                    }
                }
                EngineResult::Executed
            }
            RecordedAction::OperatorMotion {
                operator,
                target,
                motion_count,
                operator_count,
                register,
            } => {
                let total = if count.get() > 1 {
                    count.get()
                } else {
                    operator_count.get() * motion_count.get()
                };

                match target {
                    OperatorTargetId::Linewise => {
                        let pending = PendingOp {
                            operator,
                            register,
                            count: NonZeroUsize::new(total).unwrap_or(NonZeroUsize::MIN),
                            text_object_kind: None,
                        };
                        self.apply_linewise_operator(editor, view_id, doc_id, &pending, total);
                    }
                    OperatorTargetId::Motion(motion) => {
                        if let Some(m) = self.registry.motion(motion) {
                            let motion_fn = m
                                .make
                                .make(Some(NonZeroUsize::new(total).unwrap_or(NonZeroUsize::MIN)));
                            motion_fn(editor, view_id, doc_id, Movement::Extend);
                            self.execute_operator(editor, view_id, doc_id, operator, register);
                        }
                    }
                    OperatorTargetId::TextObject(text_object) => {
                        if let Some(to) = self.registry.text_object(text_object) {
                            let obj_fn = (to.make)(total);
                            obj_fn(editor, view_id, doc_id, TextObject::Inside);
                            self.execute_operator(editor, view_id, doc_id, operator, register);
                        }
                    }
                    OperatorTargetId::CharPending(command, ch) => {
                        if let Some(cp) = self.registry.char_pending(command) {
                            let motion = (cp.resolve)(ch, total);
                            motion(editor, view_id, doc_id, Movement::Extend);
                            self.execute_operator(editor, view_id, doc_id, operator, register);
                        }
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
        self.insert_recording = Some(InsertRecording {
            entry_command,
            keys: Vec::new(),
        });
    }

    fn end_insert_recording(&mut self) {
        if let Some(action) = finalize_insert_recording(self.insert_recording.take()) {
            self.last_action = Some(action);
        }
        self.sub_mode = SubMode::Normal;
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
