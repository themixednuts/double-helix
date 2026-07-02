//! Dependency-free modal editing state machines.
//!
//! The core layer owns modal state: counts, operator-pending flow,
//! char-pending resolution, dot-repeat, insert recording, and reset behavior.
//! It is generic over a host context type so embedders can wire commands to any
//! editor model without depending on Helix crates.

use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::sync::Arc;

/// A keyboard modifier set inspected by the modal state machines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    bits: u8,
}

impl Modifiers {
    /// Shift key modifier.
    pub const SHIFT: Self = Self { bits: 0b0001 };
    /// Control key modifier.
    pub const CONTROL: Self = Self { bits: 0b0010 };
    /// Alt key modifier.
    pub const ALT: Self = Self { bits: 0b0100 };
    /// Super, command, or Windows key modifier.
    pub const SUPER: Self = Self { bits: 0b1000 };

    /// Return an empty modifier set.
    #[must_use]
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    /// Return whether this set contains no modifiers.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Return whether this set contains `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.bits & other.bits == other.bits
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self {
            bits: self.bits | rhs.bits,
        }
    }
}

/// A compact key code used by the modal state machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KeyCode {
    /// A printable character.
    Char(char),
    /// Escape.
    Esc,
    /// Enter or return.
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Any host-specific key not otherwise inspected by the core.
    Other(u16),
}

/// A key event consumed by the standalone modal engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// The key code.
    pub code: KeyCode,
    /// Active modifiers.
    pub modifiers: Modifiers,
}

impl Key {
    /// Create an unmodified character key.
    #[must_use]
    pub const fn char(ch: char) -> Self {
        Self {
            code: KeyCode::Char(ch),
            modifiers: Modifiers::empty(),
        }
    }

    /// Create an escape key.
    #[must_use]
    pub const fn esc() -> Self {
        Self {
            code: KeyCode::Esc,
            modifiers: Modifiers::empty(),
        }
    }

    /// Return the character represented by an unmodified character key.
    #[must_use]
    pub const fn char_value(self) -> Option<char> {
        match self.code {
            KeyCode::Char(ch) if self.modifiers.is_empty() => Some(ch),
            _ => None,
        }
    }

    fn is_char(self, ch: char) -> bool {
        self.code == KeyCode::Char(ch) && self.modifiers.is_empty()
    }

    fn digit(self) -> Option<usize> {
        let ch = self.char_value()?;
        if ch.is_ascii_digit() {
            Some(ch.to_digit(10).expect("ASCII digit has numeric value") as usize)
        } else {
            None
        }
    }
}

/// Host-visible editor mode used for keymap decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Normal command mode.
    Normal,
    /// Insert text mode.
    Insert,
    /// Selection-extending mode.
    Select,
}

/// A command identifier specialized by marker type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id<K> {
    name: &'static str,
    _kind: std::marker::PhantomData<K>,
}

impl<K> Id<K> {
    /// Create an identifier from a stable command name.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _kind: std::marker::PhantomData,
        }
    }

    /// Return the stable command name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.name
    }
}

impl<K> std::fmt::Display for Id<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

/// Marker for motion command IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MotionTag {}
/// Marker for operator command IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OperatorTag {}
/// Marker for text object command IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TextObjectTag {}
/// Marker for action command IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionTag {}
/// Marker for char-pending command IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CharPendingTag {}

/// Identifier for a motion.
pub type MotionId = Id<MotionTag>;
/// Identifier for an operator.
pub type OperatorId = Id<OperatorTag>;
/// Identifier for a text object.
pub type TextObjectId = Id<TextObjectTag>;
/// Identifier for an action.
pub type ActionId = Id<ActionTag>;
/// Identifier for a command that waits for one character.
pub type CharPendingId = Id<CharPendingTag>;

/// A typed command token returned by a host keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandToken {
    /// A motion command.
    Motion(MotionId),
    /// An operator command.
    Operator(OperatorId),
    /// A text object command.
    TextObject(TextObjectId),
    /// A direct action command.
    Action(ActionId),
    /// A command that resolves after one more character.
    CharPending(CharPendingId),
}

impl CommandToken {
    /// Return the stable command name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Motion(id) => id.as_str(),
            Self::Operator(id) => id.as_str(),
            Self::TextObject(id) => id.as_str(),
            Self::Action(id) => id.as_str(),
            Self::CharPending(id) => id.as_str(),
        }
    }
}

impl std::fmt::Display for CommandToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a motion should move the cursor or extend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MotionMode {
    /// Move without extending selection.
    Move,
    /// Extend selection.
    Extend,
}

impl MotionMode {
    fn from_extend(extend: bool) -> Self {
        if extend {
            Self::Extend
        } else {
            Self::Move
        }
    }
}

/// Arguments passed to a motion command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MotionArgs {
    /// Effective count, with omitted counts normalized to one.
    pub count: usize,
    /// Whether this motion moves or extends.
    pub kind: MotionMode,
}

/// Arguments passed to an operator command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperatorArgs {
    /// Selected register, if any.
    pub register: Option<char>,
}

/// Text object selection flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextObject {
    /// Select inside the object.
    Inside,
    /// Select around the object.
    Around,
}

/// Arguments passed to a text object command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextObjectArgs {
    /// Effective count, with omitted counts normalized to one.
    pub count: usize,
    /// Inside or around selection.
    pub kind: TextObject,
}

/// Arguments passed to a direct action command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionArgs {
    /// Effective count, with omitted counts normalized to one.
    pub count: usize,
    /// Selected register, if any.
    pub register: Option<char>,
}

/// Arguments passed to a char-pending action command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CharActionArgs {
    /// Effective count, with omitted counts normalized to one.
    pub count: usize,
    /// Selected register, if any.
    pub register: Option<char>,
    /// Character supplied by the pending key.
    pub ch: char,
}

/// A boxed motion implementation.
pub type MotionFn<Ctx> = Box<dyn Fn(&mut Ctx, MotionArgs) + Send + Sync + 'static>;
/// A boxed operator implementation.
pub type OperatorFn<Ctx> = Box<dyn Fn(&mut Ctx, OperatorArgs) + Send + Sync + 'static>;
/// A boxed text object implementation.
pub type TextObjectFn<Ctx> = Box<dyn Fn(&mut Ctx, TextObjectArgs) + Send + Sync + 'static>;
/// A boxed direct action implementation.
pub type ActionFn<Ctx> = Box<dyn Fn(&mut Ctx, ActionArgs) + Send + Sync + 'static>;
/// A boxed char-pending action implementation.
pub type CharActionFn<Ctx> = Box<dyn Fn(&mut Ctx, CharActionArgs) + Send + Sync + 'static>;

/// A char-pending command after its character has been supplied.
pub enum CharPendingCommand<Ctx> {
    /// The command behaves like a motion.
    Motion(MotionFn<Ctx>),
    /// The command behaves like a direct action.
    Action(CharActionFn<Ctx>),
}

/// A registered motion.
pub struct MotionEntry<Ctx> {
    /// Command identifier.
    pub id: MotionId,
    make: Box<dyn Fn(Option<NonZeroUsize>) -> MotionFn<Ctx> + Send + Sync + 'static>,
}

/// A registered operator.
pub struct OperatorEntry<Ctx> {
    /// Command identifier.
    pub id: OperatorId,
    /// Operator implementation.
    pub execute: OperatorFn<Ctx>,
    pending_display: &'static str,
    doubled_key: Option<char>,
}

/// A registered text object.
pub struct TextObjectEntry<Ctx> {
    /// Command identifier.
    pub id: TextObjectId,
    make: Box<dyn Fn(usize) -> TextObjectFn<Ctx> + Send + Sync + 'static>,
}

/// A registered direct action.
pub struct ActionEntry<Ctx> {
    /// Command identifier.
    pub id: ActionId,
    /// Action implementation.
    pub execute: ActionFn<Ctx>,
}

/// A registered char-pending command.
pub struct CharPendingEntry<Ctx> {
    /// Command identifier.
    pub id: CharPendingId,
    resolve: Box<dyn Fn(char, usize) -> CharPendingCommand<Ctx> + Send + Sync + 'static>,
}

/// A command category resolved from a registry.
pub enum CommandRef<'a, Ctx> {
    /// A motion entry.
    Motion(&'a MotionEntry<Ctx>),
    /// An operator entry.
    Operator(&'a OperatorEntry<Ctx>),
    /// A text object entry.
    TextObject(&'a TextObjectEntry<Ctx>),
    /// A direct action entry.
    Action(&'a ActionEntry<Ctx>),
    /// A char-pending entry.
    CharPending(&'a CharPendingEntry<Ctx>),
}

/// Mutable builder for a [`Registry`].
pub struct Builder<Ctx> {
    motions: Vec<MotionEntry<Ctx>>,
    operators: Vec<OperatorEntry<Ctx>>,
    text_objects: Vec<TextObjectEntry<Ctx>>,
    actions: Vec<ActionEntry<Ctx>>,
    char_pending: Vec<CharPendingEntry<Ctx>>,
}

impl<Ctx> Default for Builder<Ctx> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Ctx> Builder<Ctx> {
    /// Create an empty registry builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            motions: Vec::new(),
            operators: Vec::new(),
            text_objects: Vec::new(),
            actions: Vec::new(),
            char_pending: Vec::new(),
        }
    }

    /// Register a counted motion.
    pub fn motion_counted<F>(&mut self, id: MotionId, make: F)
    where
        F: Fn(usize) -> MotionFn<Ctx> + Send + Sync + 'static,
    {
        self.motions.push(MotionEntry {
            id,
            make: Box::new(move |count| make(count.map_or(1, NonZeroUsize::get))),
        });
    }

    /// Register a motion that can observe whether the count was explicit.
    pub fn motion<F>(&mut self, id: MotionId, make: F)
    where
        F: Fn(Option<NonZeroUsize>) -> MotionFn<Ctx> + Send + Sync + 'static,
    {
        self.motions.push(MotionEntry {
            id,
            make: Box::new(make),
        });
    }

    /// Register an operator.
    pub fn operator<F>(&mut self, id: OperatorId, execute: F)
    where
        F: Fn(&mut Ctx, OperatorArgs) + Send + Sync + 'static,
    {
        self.operator_with_pending(id, id.as_str(), None, execute);
    }

    /// Register an operator with display and doubled-key metadata.
    pub fn operator_with_pending<F>(
        &mut self,
        id: OperatorId,
        pending_display: &'static str,
        doubled_key: Option<char>,
        execute: F,
    ) where
        F: Fn(&mut Ctx, OperatorArgs) + Send + Sync + 'static,
    {
        self.operators.push(OperatorEntry {
            id,
            execute: Box::new(execute),
            pending_display,
            doubled_key,
        });
    }

    /// Register a text object.
    pub fn text_object<F>(&mut self, id: TextObjectId, make: F)
    where
        F: Fn(usize) -> TextObjectFn<Ctx> + Send + Sync + 'static,
    {
        self.text_objects.push(TextObjectEntry {
            id,
            make: Box::new(make),
        });
    }

    /// Register a direct action.
    pub fn action<F>(&mut self, id: ActionId, execute: F)
    where
        F: Fn(&mut Ctx, ActionArgs) + Send + Sync + 'static,
    {
        self.actions.push(ActionEntry {
            id,
            execute: Box::new(execute),
        });
    }

    /// Register a char-pending command.
    pub fn char_pending<F>(&mut self, id: CharPendingId, resolve: F)
    where
        F: Fn(char, usize) -> CharPendingCommand<Ctx> + Send + Sync + 'static,
    {
        self.char_pending.push(CharPendingEntry {
            id,
            resolve: Box::new(resolve),
        });
    }

    /// Sort and freeze this builder into an immutable registry.
    #[must_use]
    pub fn freeze(mut self) -> Registry<Ctx> {
        self.motions.sort_unstable_by_key(|entry| entry.id.as_str());
        self.operators
            .sort_unstable_by_key(|entry| entry.id.as_str());
        self.text_objects
            .sort_unstable_by_key(|entry| entry.id.as_str());
        self.actions.sort_unstable_by_key(|entry| entry.id.as_str());
        self.char_pending
            .sort_unstable_by_key(|entry| entry.id.as_str());

        Registry {
            motions: self.motions.into_boxed_slice(),
            operators: self.operators.into_boxed_slice(),
            text_objects: self.text_objects.into_boxed_slice(),
            actions: self.actions.into_boxed_slice(),
            char_pending: self.char_pending.into_boxed_slice(),
        }
    }
}

/// Immutable command registry for a host context.
pub struct Registry<Ctx> {
    motions: Box<[MotionEntry<Ctx>]>,
    operators: Box<[OperatorEntry<Ctx>]>,
    text_objects: Box<[TextObjectEntry<Ctx>]>,
    actions: Box<[ActionEntry<Ctx>]>,
    char_pending: Box<[CharPendingEntry<Ctx>]>,
}

impl<Ctx> Registry<Ctx> {
    /// Resolve a typed command token.
    #[must_use]
    pub fn resolve(&self, token: CommandToken) -> Option<CommandRef<'_, Ctx>> {
        match token {
            CommandToken::Motion(id) => self.motion(id).map(CommandRef::Motion),
            CommandToken::Operator(id) => self.operator(id).map(CommandRef::Operator),
            CommandToken::TextObject(id) => self.text_object(id).map(CommandRef::TextObject),
            CommandToken::Action(id) => self.action(id).map(CommandRef::Action),
            CommandToken::CharPending(id) => self.char_pending(id).map(CommandRef::CharPending),
        }
    }

    /// Resolve a motion by id.
    #[must_use]
    pub fn motion(&self, id: MotionId) -> Option<&MotionEntry<Ctx>> {
        self.motions
            .binary_search_by_key(&id.as_str(), |entry| entry.id.as_str())
            .ok()
            .map(|index| &self.motions[index])
    }

    /// Resolve an operator by id.
    #[must_use]
    pub fn operator(&self, id: OperatorId) -> Option<&OperatorEntry<Ctx>> {
        self.operators
            .binary_search_by_key(&id.as_str(), |entry| entry.id.as_str())
            .ok()
            .map(|index| &self.operators[index])
    }

    /// Resolve a text object by id.
    #[must_use]
    pub fn text_object(&self, id: TextObjectId) -> Option<&TextObjectEntry<Ctx>> {
        self.text_objects
            .binary_search_by_key(&id.as_str(), |entry| entry.id.as_str())
            .ok()
            .map(|index| &self.text_objects[index])
    }

    /// Resolve an action by id.
    #[must_use]
    pub fn action(&self, id: ActionId) -> Option<&ActionEntry<Ctx>> {
        self.actions
            .binary_search_by_key(&id.as_str(), |entry| entry.id.as_str())
            .ok()
            .map(|index| &self.actions[index])
    }

    /// Resolve a char-pending command by id.
    #[must_use]
    pub fn char_pending(&self, id: CharPendingId) -> Option<&CharPendingEntry<Ctx>> {
        self.char_pending
            .binary_search_by_key(&id.as_str(), |entry| entry.id.as_str())
            .ok()
            .map(|index| &self.char_pending[index])
    }

    /// Return whether a token is present.
    #[must_use]
    pub fn contains(&self, token: CommandToken) -> bool {
        self.resolve(token).is_some()
    }

    /// Iterate all registered command tokens.
    pub fn tokens(&self) -> impl Iterator<Item = CommandToken> + '_ {
        self.motions
            .iter()
            .map(|entry| CommandToken::Motion(entry.id))
            .chain(
                self.operators
                    .iter()
                    .map(|entry| CommandToken::Operator(entry.id)),
            )
            .chain(
                self.text_objects
                    .iter()
                    .map(|entry| CommandToken::TextObject(entry.id)),
            )
            .chain(
                self.actions
                    .iter()
                    .map(|entry| CommandToken::Action(entry.id)),
            )
            .chain(
                self.char_pending
                    .iter()
                    .map(|entry| CommandToken::CharPending(entry.id)),
            )
    }

    /// Return the number of registered commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.motions.len()
            + self.operators.len()
            + self.text_objects.len()
            + self.actions.len()
            + self.char_pending.len()
    }

    /// Return whether no commands are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

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

struct InsertRecording {
    entry_command: Cow<'static, str>,
    keys: Vec<Key>,
}

fn record_insert_key(recording: &mut Option<InsertRecording>, key: Key, result: &EngineResult) {
    if let Some(recording) = recording {
        match result {
            EngineResult::InsertChar(_) | EngineResult::Executed => recording.keys.push(key),
            EngineResult::CancelledInsert(keys) => recording.keys.extend_from_slice(keys),
            EngineResult::Pending | EngineResult::Unbound | EngineResult::ReplayInsert { .. } => {}
        }
    }
}

fn finish_recording(recording: Option<InsertRecording>) -> Option<RecordedAction> {
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

/// Select-then-act modal state machine.
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

/// Vim-style verb-then-object modal state machine.
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

fn execute_target<Ctx>(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Toy {
        pos: usize,
        deleted: usize,
        marks: Vec<String>,
    }

    fn registry() -> Arc<Registry<Toy>> {
        let mut builder = Builder::new();
        builder.motion_counted(MotionId::new("right"), |count| {
            Box::new(move |toy: &mut Toy, args| {
                assert_eq!(args.count, count);
                toy.pos += args.count;
            })
        });
        builder.operator_with_pending(
            OperatorId::new("delete"),
            "d",
            Some('d'),
            |toy: &mut Toy, _args| {
                toy.deleted += 1;
            },
        );
        builder.action(ActionId::new("mark"), |toy: &mut Toy, args| {
            toy.marks.push(format!("mark:{}", args.count));
        });
        builder.char_pending(CharPendingId::new("find"), |ch, count| {
            CharPendingCommand::Motion(Box::new(move |toy: &mut Toy, args| {
                assert_eq!(args.count, count);
                toy.pos += ch as usize % 10 + args.count;
            }))
        });
        builder.char_pending(CharPendingId::new("replace"), |ch, count| {
            CharPendingCommand::Action(Box::new(move |toy: &mut Toy, args| {
                assert_eq!(args.count, count);
                toy.marks.push(format!("replace:{ch}:{}", args.count));
            }))
        });
        Arc::new(builder.freeze())
    }

    struct EmptyKeys;

    impl KeymapQuery for EmptyKeys {
        fn contains_key(&self, _mode: Mode, _key: Key) -> bool {
            false
        }

        fn pending_is_empty(&self) -> bool {
            true
        }
    }

    #[test]
    fn count_accumulation_applies_to_motion() {
        let registry = registry();
        let mut engine = Helix::new(registry);
        let mut toy = Toy::default();

        assert!(matches!(
            engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('1')),
            Some(EngineResult::Pending)
        ));
        assert!(matches!(
            engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('2')),
            Some(EngineResult::Pending)
        ));
        assert!(matches!(
            engine.process_lookup(
                &mut toy,
                Mode::Normal,
                Key::char('l'),
                Lookup::Matched(CommandToken::Motion(MotionId::new("right"))),
            ),
            EngineResult::Executed
        ));
        assert_eq!(toy.pos, 12);
    }

    #[test]
    fn operator_pending_flow_executes_motion_then_operator() {
        let registry = registry();
        let mut engine = Vim::new(registry);
        let mut toy = Toy::default();

        assert!(matches!(
            engine.process_lookup(
                &mut toy,
                Mode::Normal,
                Key::char('d'),
                Lookup::Matched(CommandToken::Operator(OperatorId::new("delete"))),
            ),
            EngineResult::Pending
        ));
        assert!(matches!(
            engine.process_lookup(
                &mut toy,
                Mode::Normal,
                Key::char('w'),
                Lookup::Matched(CommandToken::Motion(MotionId::new("right"))),
            ),
            EngineResult::Executed
        ));
        assert_eq!(toy.pos, 1);
        assert_eq!(toy.deleted, 1);
    }

    #[test]
    fn char_pending_motion_and_action_both_execute() {
        let registry = registry();
        let mut engine = Helix::new(registry);
        let mut toy = Toy::default();

        assert!(matches!(
            engine.process_lookup(
                &mut toy,
                Mode::Normal,
                Key::char('f'),
                Lookup::Fallback(CharPendingId::new("find"), 'a'),
            ),
            EngineResult::Executed
        ));
        assert!(toy.pos > 1);

        assert!(matches!(
            engine.process_lookup(
                &mut toy,
                Mode::Normal,
                Key::char('r'),
                Lookup::Fallback(CharPendingId::new("replace"), 'x'),
            ),
            EngineResult::Executed
        ));
        assert_eq!(toy.marks, ["replace:x:1"]);
    }

    #[test]
    fn dot_repeat_replays_last_action_with_count_override() {
        let registry = registry();
        let mut engine = Helix::new(registry);
        let mut toy = Toy::default();

        assert!(matches!(
            engine.process_lookup(
                &mut toy,
                Mode::Normal,
                Key::char('m'),
                Lookup::Matched(CommandToken::Action(ActionId::new("mark"))),
            ),
            EngineResult::Executed
        ));
        assert!(matches!(
            engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('3')),
            Some(EngineResult::Pending)
        ));
        assert!(matches!(
            engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('.')),
            Some(EngineResult::Executed)
        ));
        assert_eq!(toy.marks, ["mark:1", "mark:3"]);
    }

    #[test]
    fn reset_clears_pending_operator_and_count() {
        let registry = registry();
        let mut engine = Vim::new(registry);
        let mut toy = Toy::default();

        assert!(matches!(
            engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('2')),
            Some(EngineResult::Pending)
        ));
        assert!(matches!(
            engine.process_lookup(
                &mut toy,
                Mode::Normal,
                Key::char('d'),
                Lookup::Matched(CommandToken::Operator(OperatorId::new("delete"))),
            ),
            EngineResult::Pending
        ));
        assert!(engine.is_pending());
        engine.reset();
        assert!(!engine.is_pending());
        assert_eq!(engine.input_state(), InputState::default());
        assert_eq!(engine.mode_name(), "NOR");
    }
}
