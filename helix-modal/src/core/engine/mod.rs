mod helix;
mod vim;

pub use helix::Helix;
pub use vim::Vim;

use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::sync::Arc;

use super::*;

/// Result of host keymap resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The key resolved to one command.
    Matched(CommandToken),
    /// The key resolved to multiple commands.
    MatchedSequence(Vec<CommandToken>),
    /// The key is a prefix and more input is required.
    Pending,
    /// The key was not bound.
    NotFound,
    /// A pending insert-mode key sequence was cancelled.
    Cancelled(Vec<Key>),
    /// The key supplied the character for a char-pending command.
    Fallback(CharPendingId, char),
}

/// Minimal keymap queries needed before command lookup.
pub trait KeymapQuery {
    /// Return whether `key` is bound in `mode`.
    fn contains_key(&self, mode: Mode, key: Key) -> bool;

    /// Return whether no multi-key sequence is currently pending.
    fn pending_is_empty(&self) -> bool;
}

impl KeymapQuery for () {
    fn contains_key(&self, _mode: Mode, _key: Key) -> bool {
        false
    }

    fn pending_is_empty(&self) -> bool {
        true
    }
}

/// A command that can be replayed with a count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepeatableCommand {
    /// Replay an operator.
    Operator(OperatorId),
    /// Replay an action.
    Action(ActionId),
}

/// The target used by a replayed operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatorTarget {
    /// A motion target.
    Motion(MotionId),
    /// A text object target.
    TextObject(TextObjectId, TextObject),
    /// A char-pending motion target.
    CharPending(CharPendingId, char),
    /// A linewise doubled operator target.
    Linewise,
}

/// A dot-repeat recording.
#[derive(Debug, Clone)]
pub enum RecordedAction {
    /// An operator applied to a target.
    OperatorMotion {
        /// Operator command.
        operator: OperatorId,
        /// Target command.
        target: OperatorTarget,
        /// Count supplied after the operator.
        motion_count: NonZeroUsize,
        /// Count supplied before the operator.
        operator_count: NonZeroUsize,
        /// Selected register, if any.
        register: Option<char>,
    },
    /// An insert sequence.
    InsertSequence {
        /// Command that entered insert mode.
        entry_command: Cow<'static, str>,
        /// Recorded keys.
        keys: Arc<[Key]>,
    },
    /// A direct countable command.
    CountedAction {
        /// Command to repeat.
        command: RepeatableCommand,
        /// Original count.
        count: NonZeroUsize,
        /// Selected register, if any.
        register: Option<char>,
    },
}

/// Transient count and register state owned by an engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputState {
    /// Pending count, if any.
    pub count: Option<NonZeroUsize>,
    /// Selected register, if any.
    pub selected_register: Option<char>,
}

/// Result returned after a key is processed.
#[derive(Debug, Clone)]
pub enum EngineResult {
    /// Host state was mutated.
    Executed,
    /// The engine consumed input and is waiting for more.
    Pending,
    /// Insert this literal character.
    InsertChar(char),
    /// Replay cancelled insert keys as literal input.
    CancelledInsert(Vec<Key>),
    /// The key was not bound.
    Unbound,
    /// Replay a recorded insert sequence.
    ReplayInsert {
        /// Command that originally entered insert mode.
        entry_command: Cow<'static, str>,
        /// Recorded keys.
        keys: Arc<[Key]>,
    },
}

pub(super) struct InsertRecording {
    pub(super) entry_command: Cow<'static, str>,
    pub(super) keys: Vec<Key>,
}

pub(super) fn record_insert_key(
    recording: &mut Option<InsertRecording>,
    key: Key,
    result: &EngineResult,
) {
    if let Some(recording) = recording {
        match result {
            EngineResult::InsertChar(_) | EngineResult::Executed => recording.keys.push(key),
            EngineResult::CancelledInsert(keys) => recording.keys.extend_from_slice(keys),
            EngineResult::Pending | EngineResult::Unbound | EngineResult::ReplayInsert { .. } => {}
        }
    }
}

pub(super) fn finish_recording(recording: Option<InsertRecording>) -> Option<RecordedAction> {
    recording.map(|recording| RecordedAction::InsertSequence {
        entry_command: recording.entry_command,
        keys: Arc::from(recording.keys.into_boxed_slice()),
    })
}

/// Shared behavior for standalone modal engines.
pub trait Engine<Ctx> {
    /// Handle count accumulation, escape, and dot-repeat before keymap lookup.
    fn pre_resolve(
        &mut self,
        ctx: &mut Ctx,
        mode: Mode,
        keymaps: &dyn KeymapQuery,
        key: Key,
    ) -> Option<EngineResult>;

    /// Process a host keymap lookup result.
    fn process_lookup(
        &mut self,
        ctx: &mut Ctx,
        mode: Mode,
        key: Key,
        lookup: Lookup,
    ) -> EngineResult;

    /// Return a short mode name for display.
    fn mode_name(&self) -> &str;

    /// Return pending keys or command text for display.
    fn pending_display(&self) -> &str;

    /// Return whether the engine expects more input.
    fn is_pending(&self) -> bool;

    /// Clear transient pending input state.
    fn reset(&mut self);

    /// Return the last recorded repeat action.
    fn last_action(&self) -> Option<&RecordedAction>;

    /// Replay the last recorded action.
    fn repeat_last(&mut self, ctx: &mut Ctx, count: NonZeroUsize) -> EngineResult;

    /// Start recording insert-mode input for dot-repeat.
    fn begin_insert_recording(&mut self, entry_command: impl Into<Cow<'static, str>>);

    /// Finish recording insert-mode input for dot-repeat.
    fn end_insert_recording(&mut self);

    /// Return count/register input state.
    fn input_state(&self) -> InputState;

    /// Replace count/register input state.
    fn set_input_state(&mut self, state: InputState);
}

pub(super) fn execute_target<Ctx>(
    registry: &Registry<Ctx>,
    ctx: &mut Ctx,
    target: OperatorTarget,
    total: usize,
    extend: bool,
) {
    match target {
        OperatorTarget::Motion(id) => {
            if let Some(motion) = registry.motion(id) {
                ((motion.make)(Some(NonZeroUsize::new(total).unwrap_or(NonZeroUsize::MIN))))(
                    ctx,
                    MotionArgs {
                        count: total,
                        kind: MotionMode::from_extend(extend),
                    },
                );
            }
        }
        OperatorTarget::TextObject(id, kind) => {
            if let Some(text_object) = registry.text_object(id) {
                ((text_object.make)(total))(ctx, TextObjectArgs { count: total, kind });
            }
        }
        OperatorTarget::CharPending(id, ch) => {
            if let Some(entry) = registry.char_pending(id) {
                if let CharPendingCommand::Motion(motion) = (entry.resolve)(ch, total) {
                    motion(
                        ctx,
                        MotionArgs {
                            count: total,
                            kind: MotionMode::from_extend(extend),
                        },
                    );
                }
            }
        }
        OperatorTarget::Linewise => {}
    }
}
