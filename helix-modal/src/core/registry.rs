use std::num::NonZeroUsize;

use super::*;

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
    pub(crate) make: Box<dyn Fn(Option<NonZeroUsize>) -> MotionFn<Ctx> + Send + Sync + 'static>,
}

/// A registered operator.
pub struct OperatorEntry<Ctx> {
    /// Command identifier.
    pub id: OperatorId,
    /// Operator implementation.
    pub execute: OperatorFn<Ctx>,
    pub(crate) pending_display: &'static str,
    pub(crate) doubled_key: Option<char>,
}

/// A registered text object.
pub struct TextObjectEntry<Ctx> {
    /// Command identifier.
    pub id: TextObjectId,
    pub(crate) make: Box<dyn Fn(usize) -> TextObjectFn<Ctx> + Send + Sync + 'static>,
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
    pub(crate) resolve: Box<dyn Fn(char, usize) -> CharPendingCommand<Ctx> + Send + Sync + 'static>,
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
