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

    pub(crate) fn is_char(self, ch: char) -> bool {
        self.code == KeyCode::Char(ch) && self.modifiers.is_empty()
    }

    pub(crate) fn digit(self) -> Option<usize> {
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
    pub(crate) fn from_extend(extend: bool) -> Self {
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
