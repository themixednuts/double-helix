//! Helix editing engine — select→act paradigm.
//!
//! Wraps the existing dispatch model: keymap resolves to command, command executes
//! immediately. Motions update selection in-place. Count and register are accumulated
//! then passed to the resolved command.

use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::sync::Arc;

use helix_core::movement::Movement;
use helix_view::bench::log_run_phase;
use helix_view::document::Mode;
use helix_view::engine::{
    CharPendingId, CommandToken, EditingEngine, EngineResult, KeymapLookup, KeymapQuery,
    ModalInputState, OperatorTargetId, RecordedAction, RepeatableCommandId,
};
use helix_view::input::KeyEvent;
use helix_view::{DocumentId, Editor, ViewId};

use crate::registry::{CharPendingResolution, CommandRef, CommandRegistry};
use crate::{
    finalize_insert_recording, is_char_key, key_to_digit, record_insert_key, InsertRecording,
};

/// The Helix editing engine.
///
/// Uses select→act paradigm: motions update selection, operators act on it.
/// No operator-pending mode — every command executes immediately.
pub struct HelixEngine {
    registry: Arc<CommandRegistry>,
    count: Option<NonZeroUsize>,
    register: Option<char>,
    last_action: Option<RecordedAction>,
    pending_display_buf: String,
    /// Active insert recording, present while in insert mode.
    insert_recording: Option<InsertRecording>,
}

impl HelixEngine {
    pub fn new(registry: Arc<CommandRegistry>) -> Self {
        Self {
            registry,
            count: None,
            register: None,
            last_action: None,
            pending_display_buf: String::new(),
            insert_recording: None,
        }
    }

    /// Normal/select mode pre-resolve: count accumulation, dot-repeat.
    /// Returns `Some` if consumed, `None` if keymap resolution is needed.
    fn pre_resolve_normal(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult> {
        let mode = editor.mode();

        // Count accumulation: digit with existing count → append
        if let Some(digit) = key_to_digit(key) {
            if let Some(count) = self.count {
                let new = count.get() * 10 + digit;
                if new <= 100_000_000 {
                    self.count = NonZeroUsize::new(new);
                }
                return Some(EngineResult::Pending);
            }
            // Non-zero digit starts count if not bound in keymap
            if digit > 0 && !keymaps.contains_key(mode, key) {
                self.count = NonZeroUsize::new(digit);
                return Some(EngineResult::Pending);
            }
        }

        // Dot-repeat
        if is_char_key(key, '.') && keymaps.pending().is_empty() {
            let repeat_count = self.count.take().unwrap_or(NonZeroUsize::MIN);
            return Some(self.repeat_last(editor, view_id, doc_id, repeat_count));
        }

        None
    }

    /// Normal/select mode: process a pre-resolved keymap lookup.
    fn process_lookup_normal(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        lookup: KeymapLookup,
    ) -> EngineResult {
        let count = self.count;
        let count_val = count.map_or(1, NonZeroUsize::get);
        let register = self.register;

        // Update autoinfo from sticky keymap
        editor.autoinfo = keymaps.sticky_infobox();

        match lookup {
            KeymapLookup::Matched(command) => {
                let result = self.execute(editor, view_id, doc_id, command, count, register);
                if keymaps.pending().is_empty() {
                    self.count = None;
                    self.register = None;
                }
                result
            }
            KeymapLookup::MatchedSequence(ref commands) => {
                for &command in commands.iter() {
                    let _ = self.execute(editor, view_id, doc_id, command, count, register);
                }
                if keymaps.pending().is_empty() {
                    self.count = None;
                    self.register = None;
                }
                EngineResult::Executed
            }
            KeymapLookup::Pending(infobox) => {
                if let Some(info) = infobox {
                    editor.autoinfo = Some(info);
                }
                EngineResult::Pending
            }
            KeymapLookup::NotFound => {
                self.count = None;
                self.register = None;
                EngineResult::Unbound
            }
            KeymapLookup::Cancelled(_) => {
                self.count = None;
                self.register = None;
                EngineResult::Executed
            }
            KeymapLookup::Fallback(command, ch) => {
                let result = self.execute_char_pending(
                    editor, view_id, doc_id, command, ch, count_val, register,
                );
                if keymaps.pending().is_empty() {
                    self.count = None;
                    self.register = None;
                }
                result
            }
        }
    }

    /// Insert mode: process a pre-resolved keymap lookup.
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
                self.execute(editor, view_id, doc_id, command, None, None)
            }
            KeymapLookup::MatchedSequence(ref commands) => {
                for &command in commands.iter() {
                    let _ = self.execute(editor, view_id, doc_id, command, None, None);
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
            KeymapLookup::Fallback(command, ch) => {
                self.execute_char_pending(editor, view_id, doc_id, command, ch, 1, None)
            }
        };

        record_insert_key(&mut self.insert_recording, key, &result);
        result
    }

    /// Execute a resolved command.
    fn execute(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        command: CommandToken,
        count: Option<NonZeroUsize>,
        register: Option<char>,
    ) -> EngineResult {
        let execute_start = std::time::Instant::now();
        let Some(kind) = self.registry.resolve(command) else {
            log::warn!("engine command missing from registry: {command}");
            return EngineResult::Unbound;
        };
        let count_val = count.map_or(1, NonZeroUsize::get);

        let result = match kind {
            CommandRef::Motion(m) => {
                let movement = movement_from_mode(editor);
                let motion = m.make.make(count);
                motion(editor, view_id, doc_id, movement);
                EngineResult::Executed
            }
            CommandRef::Operator(op) => {
                (op.execute)(editor, view_id, doc_id, register);
                self.last_action = Some(RecordedAction::CountedAction {
                    command: RepeatableCommandId::Operator(op.id),
                    count: NonZeroUsize::new(count_val).unwrap_or(NonZeroUsize::MIN),
                    register,
                });
                EngineResult::Executed
            }
            CommandRef::TextObject(to) => {
                let obj_fn = (to.make)(count_val);
                obj_fn(
                    editor,
                    view_id,
                    doc_id,
                    helix_core::textobject::TextObject::Around,
                );
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
            CommandRef::CharPending(_) => EngineResult::Unbound,
        };
        log_run_phase("engine_execute", "helix", execute_start.elapsed(), || {
            format!(
                "token={} kind={} view_id={:?} doc_id={:?} count={} register={:?}",
                command,
                match kind {
                    CommandRef::Motion(_) => "motion",
                    CommandRef::Operator(_) => "operator",
                    CommandRef::TextObject(_) => "textobject",
                    CommandRef::Action(_) => "action",
                    CommandRef::CharPending(_) => "char_pending",
                },
                view_id,
                doc_id,
                count_val,
                register
            )
        });
        result
    }

    /// Execute a char-pending command (find_char, surround, etc.).
    #[allow(
        clippy::too_many_arguments,
        reason = "modal command dispatch carries editor, view, document, key, count, and register context"
    )]
    fn execute_char_pending(
        &self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        command: CharPendingId,
        ch: char,
        count: usize,
        register: Option<char>,
    ) -> EngineResult {
        let Some(cp) = self.registry.char_pending(command) else {
            log::warn!("char-pending command missing from registry: {command}");
            return EngineResult::Unbound;
        };

        match (cp.resolve)(ch, count) {
            CharPendingResolution::Motion(motion) => {
                let movement = movement_from_mode(editor);
                motion(editor, view_id, doc_id, movement);
            }
            CharPendingResolution::Action(action) => {
                action(editor, view_id, doc_id, register);
            }
        }
        EngineResult::Executed
    }
}

impl EditingEngine for HelixEngine {
    fn pre_resolve(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult> {
        let start = std::time::Instant::now();
        let result = match editor.mode() {
            Mode::Insert => None, // insert mode has no pre-resolve logic
            Mode::Normal | Mode::Select => {
                self.pre_resolve_normal(editor, view_id, doc_id, keymaps, key)
            }
        };
        log_run_phase("engine_dispatch", "pre_resolve", start.elapsed(), || {
            format!(
                "key={} mode={:?} view_id={:?} doc_id={:?} consumed={}",
                key.key_sequence_format(),
                editor.mode(),
                view_id,
                doc_id,
                result.is_some()
            )
        });
        result
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
        let start = std::time::Instant::now();
        let result = match editor.mode() {
            Mode::Insert => {
                self.process_lookup_insert(editor, view_id, doc_id, keymaps, key, lookup)
            }
            Mode::Normal | Mode::Select => {
                self.process_lookup_normal(editor, view_id, doc_id, keymaps, lookup)
            }
        };
        log_run_phase("engine_dispatch", "process_lookup", start.elapsed(), || {
            format!(
                "key={} mode={:?} view_id={:?} doc_id={:?}",
                key.key_sequence_format(),
                editor.mode(),
                view_id,
                doc_id
            )
        });
        result
    }

    fn mode_name(&self) -> &str {
        // Helix engine delegates mode display to the editor's mode.
        // Return empty — the frontend reads editor.mode() for display.
        ""
    }

    fn editor_mode(&self) -> Mode {
        // Helix engine doesn't track mode internally — it reads editor.mode().
        // This is only used as a fallback; callers should prefer editor.mode().
        Mode::Normal
    }

    fn pending_display(&self) -> &str {
        &self.pending_display_buf
    }

    fn is_pending(&self) -> bool {
        false
    }

    fn reset(&mut self) {
        self.count = None;
        self.register = None;
        self.pending_display_buf.clear();
    }

    fn name(&self) -> &str {
        "helix"
    }

    fn last_action(&self) -> Option<&RecordedAction> {
        self.last_action.as_ref()
    }

    fn begin_insert_recording(&mut self, entry_command: Cow<'static, str>) {
        self.insert_recording = Some(InsertRecording {
            entry_command,
            keys: Vec::new(),
        });
    }

    fn end_insert_recording(&mut self) {
        if let Some(action) = finalize_insert_recording(self.insert_recording.take()) {
            self.last_action = Some(action);
        }
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
                    RepeatableCommandId::Operator(operator) => {
                        if let Some(op) = self.registry.operator(operator) {
                            (op.execute)(editor, view_id, doc_id, register);
                            EngineResult::Executed
                        } else {
                            log::warn!("repeat operator missing from registry: {operator}");
                            EngineResult::Unbound
                        }
                    }
                    RepeatableCommandId::Action(action) => {
                        if let Some(a) = self.registry.action(action) {
                            (a.execute)(editor, view_id, doc_id, effective, register);
                            EngineResult::Executed
                        } else {
                            log::warn!("repeat action missing from registry: {action}");
                            EngineResult::Unbound
                        }
                    }
                }
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
                    OperatorTargetId::Motion(motion) => {
                        if let Some(m) = self.registry.motion(motion) {
                            let motion_fn = m
                                .make
                                .make(Some(NonZeroUsize::new(total).unwrap_or(NonZeroUsize::MIN)));
                            motion_fn(editor, view_id, doc_id, Movement::Extend);
                        }
                    }
                    OperatorTargetId::TextObject(text_object) => {
                        if let Some(to) = self.registry.text_object(text_object) {
                            let obj_fn = (to.make)(total);
                            obj_fn(
                                editor,
                                view_id,
                                doc_id,
                                helix_core::textobject::TextObject::Around,
                            );
                        }
                    }
                    OperatorTargetId::CharPending(command, ch) => {
                        if let Some(cp) = self.registry.char_pending(command) {
                            if let CharPendingResolution::Motion(motion) = (cp.resolve)(ch, total) {
                                motion(editor, view_id, doc_id, Movement::Extend);
                            }
                        }
                    }
                    OperatorTargetId::Linewise => {}
                }

                if let Some(op) = self.registry.operator(operator) {
                    (op.execute)(editor, view_id, doc_id, register);
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
}

/// Determine movement behavior from editor mode.
fn movement_from_mode(editor: &Editor) -> Movement {
    if editor.mode() == Mode::Select {
        Movement::Extend
    } else {
        Movement::Move
    }
}
