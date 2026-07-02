use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::sync::Arc;

use super::{
    execute_target, finish_recording, record_insert_key, Engine, EngineResult, InputState,
    InsertRecording, KeymapQuery, Lookup, OperatorTarget, RecordedAction, RepeatableCommand,
};
use crate::core::*;

pub struct Vim<Ctx> {
    registry: Arc<Registry<Ctx>>,
    sub_mode: SubMode,
    count: Option<NonZeroUsize>,
    motion_count: Option<NonZeroUsize>,
    register: Option<char>,
    last_action: Option<RecordedAction>,
    pending_display: String,
    insert_recording: Option<InsertRecording>,
}

#[derive(Debug, Clone)]
enum SubMode {
    Normal,
    Insert,
    OperatorPending(PendingOp),
}

#[derive(Debug, Clone)]
struct PendingOp {
    operator: OperatorId,
    register: Option<char>,
    count: NonZeroUsize,
    text_object_kind: Option<TextObject>,
}

impl<Ctx> Vim<Ctx> {
    /// Create a Vim-style engine from a shared registry.
    #[must_use]
    pub fn new(registry: Arc<Registry<Ctx>>) -> Self {
        Self {
            registry,
            sub_mode: SubMode::Normal,
            count: None,
            motion_count: None,
            register: None,
            last_action: None,
            pending_display: String::new(),
            insert_recording: None,
        }
    }

    fn dispatch_normal(
        &mut self,
        ctx: &mut Ctx,
        command: CommandToken,
        count: Option<NonZeroUsize>,
        register: Option<char>,
    ) -> EngineResult {
        let count_value = count.map_or(1, NonZeroUsize::get);
        match self.registry.resolve(command) {
            Some(CommandRef::Operator(operator)) => {
                self.sub_mode = SubMode::OperatorPending(PendingOp {
                    operator: operator.id,
                    register,
                    count: NonZeroUsize::new(count_value).unwrap_or(NonZeroUsize::MIN),
                    text_object_kind: None,
                });
                self.update_pending_display();
                EngineResult::Pending
            }
            Some(CommandRef::Motion(motion)) => {
                ((motion.make)(count))(
                    ctx,
                    MotionArgs {
                        count: count_value,
                        kind: MotionMode::Move,
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
            Some(CommandRef::TextObject(_)) | Some(CommandRef::CharPending(_)) | None => {
                EngineResult::Unbound
            }
        }
    }

    fn resolve_operator_target(
        &mut self,
        ctx: &mut Ctx,
        command: CommandToken,
        pending: PendingOp,
        motion_count: NonZeroUsize,
        total_count: usize,
    ) -> EngineResult {
        match self.registry.resolve(command) {
            Some(CommandRef::Motion(motion)) => {
                self.sub_mode = SubMode::Normal;
                self.pending_display.clear();
                ((motion.make)(Some(
                    NonZeroUsize::new(total_count).unwrap_or(NonZeroUsize::MIN),
                )))(
                    ctx,
                    MotionArgs {
                        count: total_count,
                        kind: MotionMode::Extend,
                    },
                );
                self.execute_operator(ctx, pending.operator, pending.register);
                self.last_action = Some(RecordedAction::OperatorMotion {
                    operator: pending.operator,
                    target: OperatorTarget::Motion(motion.id),
                    motion_count,
                    operator_count: pending.count,
                    register: pending.register,
                });
                EngineResult::Executed
            }
            Some(CommandRef::TextObject(text_object)) => {
                self.sub_mode = SubMode::Normal;
                self.pending_display.clear();
                let kind = pending.text_object_kind.unwrap_or(TextObject::Inside);
                ((text_object.make)(total_count))(
                    ctx,
                    TextObjectArgs {
                        count: total_count,
                        kind,
                    },
                );
                self.execute_operator(ctx, pending.operator, pending.register);
                self.last_action = Some(RecordedAction::OperatorMotion {
                    operator: pending.operator,
                    target: OperatorTarget::TextObject(text_object.id, kind),
                    motion_count,
                    operator_count: pending.count,
                    register: pending.register,
                });
                EngineResult::Executed
            }
            _ => {
                self.sub_mode = SubMode::Normal;
                self.pending_display.clear();
                EngineResult::Unbound
            }
        }
    }

    fn execute_operator(&self, ctx: &mut Ctx, operator: OperatorId, register: Option<char>) {
        if let Some(operator) = self.registry.operator(operator) {
            (operator.execute)(ctx, OperatorArgs { register });
        }
    }

    fn update_pending_display(&mut self) {
        self.pending_display.clear();
        if let SubMode::OperatorPending(ref pending) = self.sub_mode {
            if let Some(count) = self.count {
                use std::fmt::Write;
                let _ = write!(self.pending_display, "{count}");
            }
            let display = self
                .registry
                .operator(pending.operator)
                .map_or(pending.operator.as_str(), |entry| entry.pending_display);
            self.pending_display.push_str(display);
        }
    }
}

impl<Ctx> Engine<Ctx> for Vim<Ctx> {
    fn pre_resolve(
        &mut self,
        ctx: &mut Ctx,
        mode: Mode,
        keymaps: &dyn KeymapQuery,
        key: Key,
    ) -> Option<EngineResult> {
        match self.sub_mode.clone() {
            SubMode::Normal => {
                if mode == Mode::Insert {
                    return None;
                }
                if let Some(digit) = key.digit() {
                    if self.count.is_some() || digit > 0 {
                        let current = self.count.map_or(0, NonZeroUsize::get);
                        let new = current * 10 + digit;
                        if new <= 100_000_000 {
                            self.count = NonZeroUsize::new(new);
                        }
                        return Some(EngineResult::Pending);
                    }
                }
                if key.is_char('.') && keymaps.pending_is_empty() {
                    let count = self.count.take().unwrap_or(NonZeroUsize::MIN);
                    return Some(self.repeat_last(ctx, count));
                }
                None
            }
            SubMode::OperatorPending(pending) => {
                if key.code == KeyCode::Esc {
                    self.sub_mode = SubMode::Normal;
                    self.pending_display.clear();
                    return Some(EngineResult::Executed);
                }
                if let Some(digit) = key.digit() {
                    if self.motion_count.is_some() || digit > 0 {
                        let current = self.motion_count.map_or(0, NonZeroUsize::get);
                        let new = current * 10 + digit;
                        if new <= 100_000_000 {
                            self.motion_count = NonZeroUsize::new(new);
                        }
                        return Some(EngineResult::Pending);
                    }
                }
                if pending.text_object_kind.is_none() {
                    if key.is_char('i') {
                        let mut pending = pending;
                        pending.text_object_kind = Some(TextObject::Inside);
                        self.sub_mode = SubMode::OperatorPending(pending);
                        return Some(EngineResult::Pending);
                    }
                    if key.is_char('a') {
                        let mut pending = pending;
                        pending.text_object_kind = Some(TextObject::Around);
                        self.sub_mode = SubMode::OperatorPending(pending);
                        return Some(EngineResult::Pending);
                    }
                }
                if let Some(operator) = self.registry.operator(pending.operator) {
                    if operator.doubled_key.is_some_and(|ch| key.is_char(ch)) {
                        let motion_count = self.motion_count.take().unwrap_or(NonZeroUsize::MIN);
                        self.sub_mode = SubMode::Normal;
                        self.pending_display.clear();
                        self.execute_operator(ctx, pending.operator, pending.register);
                        self.last_action = Some(RecordedAction::OperatorMotion {
                            operator: pending.operator,
                            target: OperatorTarget::Linewise,
                            motion_count,
                            operator_count: pending.count,
                            register: pending.register,
                        });
                        return Some(EngineResult::Executed);
                    }
                }
                None
            }
            SubMode::Insert => None,
        }
    }

    fn process_lookup(
        &mut self,
        ctx: &mut Ctx,
        mode: Mode,
        key: Key,
        lookup: Lookup,
    ) -> EngineResult {
        let result = match self.sub_mode.clone() {
            SubMode::Normal => {
                if mode == Mode::Insert {
                    match lookup {
                        Lookup::Matched(command) => {
                            if let Some(CommandRef::Action(action)) = self.registry.resolve(command)
                            {
                                (action.execute)(
                                    ctx,
                                    ActionArgs {
                                        count: 1,
                                        register: None,
                                    },
                                );
                            }
                            EngineResult::Executed
                        }
                        Lookup::Pending => EngineResult::Pending,
                        Lookup::NotFound => key
                            .char_value()
                            .map_or(EngineResult::Unbound, EngineResult::InsertChar),
                        Lookup::Cancelled(keys) => EngineResult::CancelledInsert(keys),
                        _ => EngineResult::Unbound,
                    }
                } else {
                    let count = self.count.take();
                    let register = self.register.take();
                    match lookup {
                        Lookup::Matched(command) => {
                            self.dispatch_normal(ctx, command, count, register)
                        }
                        Lookup::MatchedSequence(commands) => {
                            for command in commands {
                                let _ = self.dispatch_normal(ctx, command, count, register);
                            }
                            EngineResult::Executed
                        }
                        Lookup::Pending => {
                            self.count = count;
                            self.register = register;
                            EngineResult::Pending
                        }
                        Lookup::NotFound => EngineResult::Unbound,
                        Lookup::Cancelled(_) => EngineResult::Executed,
                        Lookup::Fallback(command, ch) => {
                            let count = count.map_or(1, NonZeroUsize::get);
                            if let Some(entry) = self.registry.char_pending(command) {
                                match (entry.resolve)(ch, count) {
                                    CharPendingCommand::Motion(motion) => motion(
                                        ctx,
                                        MotionArgs {
                                            count,
                                            kind: MotionMode::Move,
                                        },
                                    ),
                                    CharPendingCommand::Action(action) => action(
                                        ctx,
                                        CharActionArgs {
                                            count,
                                            register,
                                            ch,
                                        },
                                    ),
                                }
                                EngineResult::Executed
                            } else {
                                EngineResult::Unbound
                            }
                        }
                    }
                }
            }
            SubMode::OperatorPending(pending) => {
                let motion_count = self.motion_count.take();
                let motion_count_nz = motion_count.unwrap_or(NonZeroUsize::MIN);
                let total_count = pending.count.get() * motion_count_nz.get();
                match lookup {
                    Lookup::Matched(command) => self.resolve_operator_target(
                        ctx,
                        command,
                        pending,
                        motion_count_nz,
                        total_count,
                    ),
                    Lookup::Pending => {
                        self.motion_count = motion_count;
                        EngineResult::Pending
                    }
                    Lookup::Fallback(command, ch) => {
                        self.sub_mode = SubMode::Normal;
                        self.pending_display.clear();
                        if let Some(entry) = self.registry.char_pending(command) {
                            match (entry.resolve)(ch, total_count) {
                                CharPendingCommand::Motion(motion) => {
                                    motion(
                                        ctx,
                                        MotionArgs {
                                            count: total_count,
                                            kind: MotionMode::Extend,
                                        },
                                    );
                                    self.execute_operator(ctx, pending.operator, pending.register);
                                    self.last_action = Some(RecordedAction::OperatorMotion {
                                        operator: pending.operator,
                                        target: OperatorTarget::CharPending(command, ch),
                                        motion_count: motion_count_nz,
                                        operator_count: pending.count,
                                        register: pending.register,
                                    });
                                }
                                CharPendingCommand::Action(action) => {
                                    action(
                                        ctx,
                                        CharActionArgs {
                                            count: total_count,
                                            register: pending.register,
                                            ch,
                                        },
                                    );
                                }
                            }
                            EngineResult::Executed
                        } else {
                            EngineResult::Unbound
                        }
                    }
                    _ => {
                        self.sub_mode = SubMode::Normal;
                        self.pending_display.clear();
                        EngineResult::Unbound
                    }
                }
            }
            SubMode::Insert => match lookup {
                Lookup::Matched(command) => {
                    if let Some(CommandRef::Action(action)) = self.registry.resolve(command) {
                        (action.execute)(
                            ctx,
                            ActionArgs {
                                count: 1,
                                register: None,
                            },
                        );
                    }
                    EngineResult::Executed
                }
                Lookup::Pending => EngineResult::Pending,
                Lookup::NotFound => key
                    .char_value()
                    .map_or(EngineResult::Unbound, EngineResult::InsertChar),
                Lookup::Cancelled(keys) => EngineResult::CancelledInsert(keys),
                _ => EngineResult::Unbound,
            },
        };
        record_insert_key(&mut self.insert_recording, key, &result);
        result
    }

    fn mode_name(&self) -> &str {
        match self.sub_mode {
            SubMode::Normal => "NOR",
            SubMode::Insert => "INS",
            SubMode::OperatorPending(_) => "OPR",
        }
    }

    fn pending_display(&self) -> &str {
        &self.pending_display
    }

    fn is_pending(&self) -> bool {
        matches!(self.sub_mode, SubMode::OperatorPending(_)) || self.count.is_some()
    }

    fn reset(&mut self) {
        self.sub_mode = SubMode::Normal;
        self.count = None;
        self.motion_count = None;
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
                    RepeatableCommand::Action(id) => {
                        if let Some(action) = self.registry.action(id) {
                            (action.execute)(
                                ctx,
                                ActionArgs {
                                    count: effective.get(),
                                    register,
                                },
                            );
                        }
                    }
                    RepeatableCommand::Operator(id) => self.execute_operator(ctx, id, register),
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
                execute_target(&self.registry, ctx, target, total, true);
                self.execute_operator(ctx, operator, register);
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
        self.sub_mode = SubMode::Insert;
        self.insert_recording = Some(InsertRecording {
            entry_command: entry_command.into(),
            keys: Vec::new(),
        });
    }

    fn end_insert_recording(&mut self) {
        if let Some(action) = finish_recording(self.insert_recording.take()) {
            self.last_action = Some(action);
        }
        self.sub_mode = SubMode::Normal;
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
        self.update_pending_display();
    }
}
