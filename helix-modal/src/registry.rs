//! Command registry — classifies commands for engine composition.
//!
//! The engine needs typed lookup by command kind so it can apply the correct
//! composition rules without falling back to string identifiers.
//!
//! Built via [`CommandRegistryBuilder`], then frozen into an immutable
//! [`CommandRegistry`] with sorted `Box<[T]>` slices for O(log n) lookups.

use helix_view::engine::{
    ActionId, CharPendingId, CommandToken, MotionFn, MotionId, OperatorId, TextObjectFn,
    TextObjectId,
};
use helix_view::{DocumentId, Editor, ViewId};
use std::num::NonZeroUsize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandScope {
    Viewport,
    Tree,
    Frontend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec<T> {
    name: &'static str,
    payload: T,
    doc: &'static str,
    scope: CommandScope,
}

impl<T> CommandSpec<T> {
    pub const fn new(
        name: &'static str,
        payload: T,
        doc: &'static str,
        scope: CommandScope,
    ) -> Self {
        Self {
            name,
            payload,
            doc,
            scope,
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub fn payload(&self) -> T
    where
        T: Copy,
    {
        self.payload
    }

    pub const fn doc(&self) -> &'static str {
        self.doc
    }

    pub const fn scope(&self) -> CommandScope {
        self.scope
    }
}

pub type EngineCommandSpec = CommandSpec<CommandToken>;

impl CommandSpec<CommandToken> {
    pub fn token(&self) -> CommandToken {
        self.payload()
    }

    pub const fn motion(name: &'static str, doc: &'static str, scope: CommandScope) -> Self {
        Self::new(name, CommandToken::Motion(MotionId::new(name)), doc, scope)
    }

    pub const fn operator(name: &'static str, doc: &'static str, scope: CommandScope) -> Self {
        Self::new(
            name,
            CommandToken::Operator(OperatorId::new(name)),
            doc,
            scope,
        )
    }

    pub const fn text_object(name: &'static str, doc: &'static str, scope: CommandScope) -> Self {
        Self::new(
            name,
            CommandToken::TextObject(TextObjectId::new(name)),
            doc,
            scope,
        )
    }

    pub const fn action(name: &'static str, doc: &'static str, scope: CommandScope) -> Self {
        Self::new(name, CommandToken::Action(ActionId::new(name)), doc, scope)
    }

    pub const fn char_pending(name: &'static str, doc: &'static str, scope: CommandScope) -> Self {
        Self::new(
            name,
            CommandToken::CharPending(CharPendingId::new(name)),
            doc,
            scope,
        )
    }
}

/// What kind of command a key binding resolves to.
pub enum CommandKind {
    /// Updates selection. Engine controls extend vs move.
    Motion(MotionEntry),
    /// Acts on current selection (delete, yank, indent, etc.).
    Operator(OperatorEntry),
    /// Selects a text object (word, paragraph, surround, etc.).
    TextObject(TextObjectEntry),
    /// Direct editor action (undo, mode switch, etc.).
    Action(ActionEntry),
    /// Wait for next character input (find_char, replace, surround_add).
    CharPending(CharPendingEntry),
}

pub struct MotionEntry {
    pub id: MotionId,
    /// Creates the motion closure, capturing count.
    pub make: MotionFactory,
}

pub enum MotionFactory {
    Counted(fn(count: usize) -> MotionFn),
    Optional(fn(count: Option<NonZeroUsize>) -> MotionFn),
}

impl MotionFactory {
    pub fn make(&self, count: Option<NonZeroUsize>) -> MotionFn {
        match self {
            Self::Counted(make) => make(count.map_or(1, NonZeroUsize::get)),
            Self::Optional(make) => make(count),
        }
    }
}

impl CommandKind {
    pub const fn id(&self) -> CommandToken {
        match self {
            Self::Motion(entry) => CommandToken::Motion(entry.id),
            Self::Operator(entry) => CommandToken::Operator(entry.id),
            Self::TextObject(entry) => CommandToken::TextObject(entry.id),
            Self::Action(entry) => CommandToken::Action(entry.id),
            Self::CharPending(entry) => CommandToken::CharPending(entry.id),
        }
    }
}

pub enum CommandRef<'a> {
    Motion(&'a MotionEntry),
    Operator(&'a OperatorEntry),
    TextObject(&'a TextObjectEntry),
    Action(&'a ActionEntry),
    CharPending(&'a CharPendingEntry),
}

pub struct OperatorEntry {
    pub id: OperatorId,
    pub execute:
        fn(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, register: Option<char>),
}

pub struct TextObjectEntry {
    pub id: TextObjectId,
    /// Creates the text object closure, capturing count.
    pub make: fn(count: usize) -> TextObjectFn,
}

pub struct ActionEntry {
    pub id: ActionId,
    pub execute: fn(
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        count: usize,
        register: Option<char>,
    ),
}

// TODO: When adding action-like char-pending commands (e.g., replace_char),
// widen `resolve` to return an enum { Motion(MotionFn), Action(...) }.
pub struct CharPendingEntry {
    pub id: CharPendingId,
    /// After receiving the character, produces a motion.
    pub resolve: fn(ch: char, count: usize) -> MotionFn,
}

/// Mutable builder for assembling a command registry at startup.
///
/// Call [`register`](Self::register) to add commands, then [`freeze`](Self::freeze)
/// to produce an immutable [`CommandRegistry`].
pub struct CommandRegistryBuilder {
    motions: Vec<MotionEntry>,
    operators: Vec<OperatorEntry>,
    text_objects: Vec<TextObjectEntry>,
    actions: Vec<ActionEntry>,
    char_pending: Vec<CharPendingEntry>,
}

impl Default for CommandRegistryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRegistryBuilder {
    pub fn new() -> Self {
        Self {
            motions: Vec::new(),
            operators: Vec::new(),
            text_objects: Vec::new(),
            actions: Vec::new(),
            char_pending: Vec::new(),
        }
    }

    /// Register a command.
    pub fn register(&mut self, kind: CommandKind) {
        match kind {
            CommandKind::Motion(entry) => self.motions.push(entry),
            CommandKind::Operator(entry) => self.operators.push(entry),
            CommandKind::TextObject(entry) => self.text_objects.push(entry),
            CommandKind::Action(entry) => self.actions.push(entry),
            CommandKind::CharPending(entry) => self.char_pending.push(entry),
        }
    }

    /// Sort and freeze into an immutable [`CommandRegistry`].
    pub fn freeze(mut self) -> CommandRegistry {
        self.motions.sort_unstable_by_key(|e| e.id);
        self.operators.sort_unstable_by_key(|e| e.id);
        self.text_objects.sort_unstable_by_key(|e| e.id);
        self.actions.sort_unstable_by_key(|e| e.id);
        self.char_pending.sort_unstable_by_key(|e| e.id);

        CommandRegistry {
            motions: self.motions.into_boxed_slice(),
            operators: self.operators.into_boxed_slice(),
            text_objects: self.text_objects.into_boxed_slice(),
            actions: self.actions.into_boxed_slice(),
            char_pending: self.char_pending.into_boxed_slice(),
        }
    }
}

/// Immutable registry of all commands, shared between engines.
///
/// Uses sorted `Box<[T]>` slices with binary search for O(log n) lookups.
/// Constructed via [`CommandRegistryBuilder::freeze`].
pub struct CommandRegistry {
    motions: Box<[MotionEntry]>,
    operators: Box<[OperatorEntry]>,
    text_objects: Box<[TextObjectEntry]>,
    actions: Box<[ActionEntry]>,
    char_pending: Box<[CharPendingEntry]>,
}

impl CommandRegistry {
    pub fn resolve(&self, token: CommandToken) -> Option<CommandRef<'_>> {
        match token {
            CommandToken::Motion(id) => self.motion(id).map(CommandRef::Motion),
            CommandToken::Operator(id) => self.operator(id).map(CommandRef::Operator),
            CommandToken::TextObject(id) => self.text_object(id).map(CommandRef::TextObject),
            CommandToken::Action(id) => self.action(id).map(CommandRef::Action),
            CommandToken::CharPending(id) => self.char_pending(id).map(CommandRef::CharPending),
        }
    }

    pub fn motion(&self, id: MotionId) -> Option<&MotionEntry> {
        self.motions
            .binary_search_by_key(&id.as_str(), |e| e.id.as_str())
            .ok()
            .map(|i| &self.motions[i])
    }

    pub fn operator(&self, id: OperatorId) -> Option<&OperatorEntry> {
        self.operators
            .binary_search_by_key(&id.as_str(), |e| e.id.as_str())
            .ok()
            .map(|i| &self.operators[i])
    }

    pub fn text_object(&self, id: TextObjectId) -> Option<&TextObjectEntry> {
        self.text_objects
            .binary_search_by_key(&id.as_str(), |e| e.id.as_str())
            .ok()
            .map(|i| &self.text_objects[i])
    }

    pub fn action(&self, id: ActionId) -> Option<&ActionEntry> {
        self.actions
            .binary_search_by_key(&id.as_str(), |e| e.id.as_str())
            .ok()
            .map(|i| &self.actions[i])
    }

    pub fn char_pending(&self, id: CharPendingId) -> Option<&CharPendingEntry> {
        self.char_pending
            .binary_search_by_key(&id.as_str(), |e| e.id.as_str())
            .ok()
            .map(|i| &self.char_pending[i])
    }

    pub fn contains(&self, token: CommandToken) -> bool {
        self.resolve(token).is_some()
    }

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

    /// Number of registered commands.
    pub fn len(&self) -> usize {
        self.motions.len()
            + self.operators.len()
            + self.text_objects.len()
            + self.actions.len()
            + self.char_pending.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
