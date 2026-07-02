use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::sync::Arc;

use super::{
    execute_target, finish_recording, record_insert_key, Engine, EngineResult, InputState,
    InsertRecording, KeymapQuery, Lookup, RecordedAction, RepeatableCommand,
};
use crate::core::*;

pub struct Helix<Ctx> {
    registry: Arc<Registry<Ctx>>,
    count: Option<NonZeroUsize>,
    register: Option<char>,
    last_action: Option<RecordedAction>,
    pending_display: String,
    insert_recording: Option<InsertRecording>,
}

impl<Ctx> Helix<Ctx> {
    /// Create a Helix-style engine from a shared registry.
    #[must_use]
    pub fn new(registry: Arc<Registry<Ctx>>) -> Self {
        Self {
            registry,
            count: None,
            register: None,
            last_action: None,
            pending_display: String::new(),
            insert_recording: None,
        }
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        command: CommandToken,
        count: Option<NonZeroUsize>,
        register: Option<char>,
        extend: bool,
    ) -> EngineResult {
        let count_value = count.map_or(1, NonZeroUsize::get);
        match self.registry.resolve(command) {
            Some(CommandRef::Motion(motion)) => {
                ((motion.make)(count))(
                    ctx,
                    MotionArgs {
                        count: count_value,
                        kind: MotionMode::from_extend(extend),
                    },
                );
                EngineResult::Executed
            }
            Some(CommandRef::Operator(operator)) => {
                (operator.execute)(ctx, OperatorArgs { register });
                self.last_action = Some(RecordedAction::CountedAction {
                    command: RepeatableCommand::Operator(operator.id),
                    count: NonZeroUsize::new(count_value).unwrap_or(NonZeroUsize::MIN),
                    register,
                });
                EngineResult::Executed
            }
            Some(CommandRef::TextObject(text_object)) => {
                ((text_object.make)(count_value))(
                    ctx,
                    TextObjectArgs {
                        count: count_value,
                        kind: TextObject::Around,
                    },
                );
                EngineResult::Executed
            }
            Some(CommandRef::Action(action)) => {
                (action.execute)(
                    ctx,
                    ActionArgs {
                        count: count_value,
                        register,
                    },
                );
                self.last_action = Some(RecordedAction::CountedAction {
                    command: RepeatableCommand::Action(action.id),
                    count: NonZeroUsize::new(count_value).unwrap_or(NonZeroUsize::MIN),
                    register,
                });
                EngineResult::Executed
            }
            Some(CommandRef::CharPending(_)) | None => EngineResult::Unbound,
        }
    }

    fn execute_char_pending(
        &self,
        ctx: &mut Ctx,
        command: CharPendingId,
        ch: char,
        count: usize,
        register: Option<char>,
        extend: bool,
    ) -> EngineResult {
        let Some(entry) = self.registry.char_pending(command) else {
            return EngineResult::Unbound;
        };
        match (entry.resolve)(ch, count) {
            CharPendingCommand::Motion(motion) => {
                motion(
                    ctx,
                    MotionArgs {
                        count,
                        kind: MotionMode::from_extend(extend),
                    },
                );
            }
            CharPendingCommand::Action(action) => {
                action(
                    ctx,
                    CharActionArgs {
                        count,
                        register,
                        ch,
                    },
                );
            }
        }
        EngineResult::Executed
    }
}

impl<Ctx> Engine<Ctx> for Helix<Ctx> {
    fn pre_resolve(
        &mut self,
        ctx: &mut Ctx,
        mode: Mode,
        keymaps: &dyn KeymapQuery,
        key: Key,
    ) -> Option<EngineResult> {
        if mode == Mode::Insert {
            return None;
        }

        if let Some(digit) = key.digit() {
            if let Some(count) = self.count {
                let new = count.get() * 10 + digit;
                if new <= 100_000_000 {
                    self.count = NonZeroUsize::new(new);
                }
                return Some(EngineResult::Pending);
            }
            if digit > 0 && !keymaps.contains_key(mode, key) {
                self.count = NonZeroUsize::new(digit);
                return Some(EngineResult::Pending);
            }
        }

        if key.is_char('.') && keymaps.pending_is_empty() {
            let count = self.count.take().unwrap_or(NonZeroUsize::MIN);
            return Some(self.repeat_last(ctx, count));
        }

        None
    }

    fn process_lookup(
        &mut self,
        ctx: &mut Ctx,
        mode: Mode,
        key: Key,
        lookup: Lookup,
    ) -> EngineResult {
        let result = if mode == Mode::Insert {
            match lookup {
                Lookup::Matched(command) => self.execute(ctx, command, None, None, false),
                Lookup::MatchedSequence(commands) => {
                    for command in commands {
                        let _ = self.execute(ctx, command, None, None, false);
                    }
                    EngineResult::Executed
                }
                Lookup::Pending => EngineResult::Pending,
                Lookup::NotFound => key
                    .char_value()
                    .map_or(EngineResult::Unbound, EngineResult::InsertChar),
                Lookup::Cancelled(keys) => EngineResult::CancelledInsert(keys),
                Lookup::Fallback(command, ch) => {
                    self.execute_char_pending(ctx, command, ch, 1, None, false)
                }
            }
        } else {
            let count = self.count;
            let count_value = count.map_or(1, NonZeroUsize::get);
            let register = self.register;
            let extend = mode == Mode::Select;
            let result = match lookup {
                Lookup::Matched(command) => self.execute(ctx, command, count, register, extend),
                Lookup::MatchedSequence(commands) => {
                    for command in commands {
                        let _ = self.execute(ctx, command, count, register, extend);
                    }
                    EngineResult::Executed
                }
                Lookup::Pending => EngineResult::Pending,
                Lookup::NotFound => EngineResult::Unbound,
                Lookup::Cancelled(_) => EngineResult::Executed,
                Lookup::Fallback(command, ch) => {
                    self.execute_char_pending(ctx, command, ch, count_value, register, extend)
                }
            };
            if !matches!(result, EngineResult::Pending) {
                self.count = None;
                self.register = None;
            }
            result
        };

        record_insert_key(&mut self.insert_recording, key, &result);
        result
    }

    fn mode_name(&self) -> &str {
        ""
    }

    fn pending_display(&self) -> &str {
        &self.pending_display
    }

    fn is_pending(&self) -> bool {
        false
    }

    fn reset(&mut self) {
        self.count = None;
        self.register = None;
        self.pending_display.clear();
    }

    fn last_action(&self) -> Option<&RecordedAction> {
        self.last_action.as_ref()
    }

    fn repeat_last(&mut self, ctx: &mut Ctx, count: NonZeroUsize) -> EngineResult {
        let Some(action) = self.last_action.clone() else {
            return EngineResult::Executed;
        };
        match action {
            RecordedAction::CountedAction {
                command,
                count: original,
                register,
            } => {
                let effective = if count.get() > 1 { count } else { original };
                match command {
                    RepeatableCommand::Operator(id) => {
                        if let Some(operator) = self.registry.operator(id) {
                            (operator.execute)(ctx, OperatorArgs { register });
                            EngineResult::Executed
                        } else {
                            EngineResult::Unbound
                        }
                    }
                    RepeatableCommand::Action(id) => {
                        if let Some(action) = self.registry.action(id) {
                            (action.execute)(
                                ctx,
                                ActionArgs {
                                    count: effective.get(),
                                    register,
                                },
                            );
                            EngineResult::Executed
                        } else {
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
                execute_target(&self.registry, ctx, target, total, true);
                if let Some(operator) = self.registry.operator(operator) {
                    (operator.execute)(ctx, OperatorArgs { register });
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

    fn begin_insert_recording(&mut self, entry_command: impl Into<Cow<'static, str>>) {
        self.insert_recording = Some(InsertRecording {
            entry_command: entry_command.into(),
            keys: Vec::new(),
        });
    }

    fn end_insert_recording(&mut self) {
        if let Some(action) = finish_recording(self.insert_recording.take()) {
            self.last_action = Some(action);
        }
    }

    fn input_state(&self) -> InputState {
        InputState {
            count: self.count,
            selected_register: self.register,
        }
    }

    fn set_input_state(&mut self, state: InputState) {
        self.count = state.count;
        self.register = state.selected_register;
    }
}
