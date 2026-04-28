//! Editing engine abstraction.
//!
//! The engine owns the entire key→mutation pipeline: mode state machine, keymap
//! resolution, count/register accumulation, operator×motion composition, and
//! dot-repeat. Both Helix and Vim engines share the same command atoms (motions,
//! operators, text objects) but implement different composition rules.
//!
//! The trait lives in helix-view so any frontend can work with any engine.
//! Concrete implementations live in helix-modal.

use crate::document::Mode;
use crate::info::Info;
use crate::input::KeyEvent;
use crate::{DocumentId, Editor, ViewId};

use helix_core::movement::Movement;
use std::borrow::Cow;
use std::num::NonZeroUsize;

// ─── Keymap abstraction ─────────────────────────────────────────────

/// Result of looking up a key in the keymap.
///
/// This mirrors helix-term's `KeymapResult` but uses typed command IDs instead of
/// `MappableCommand`, making it frontend-independent and non-stringly.
#[derive(Debug, Clone)]
pub enum KeymapLookup {
    /// Key resolved to a single command.
    Matched(CommandToken),
    /// Key resolved to a sequence of commands.
    MatchedSequence(Box<[CommandToken]>),
    /// Need more keys to complete the sequence. Contains the infobox for display.
    Pending(Option<Info>),
    /// Key was not found in the keymap.
    NotFound,
    /// Multi-key sequence was cancelled. Contains the buffered keys.
    Cancelled(Box<[KeyEvent]>),
    /// Matched a fallback command (text object surround, etc.).
    Fallback(CharPendingId, char),
}

/// Read-only keymap queries used by the engine for count/register decisions.
///
/// Unlike the old `KeymapProvider`, this trait does NOT include `get()` — keymap
/// resolution is done by the frontend, and the result is passed to the engine
/// via `process_lookup()`.
pub trait KeymapQuery {
    /// Whether the keymap contains a binding for this key in the given mode.
    /// Used to decide if a digit starts count accumulation or is a keymap binding.
    fn contains_key(&self, mode: Mode, key: KeyEvent) -> bool;

    /// Keys buffered in an incomplete multi-key sequence.
    fn pending(&self) -> &[KeyEvent];

    /// Whether there's an active sticky keymap node.
    fn has_sticky(&self) -> bool;

    /// Get the infobox for the current sticky node (for autoinfo display).
    fn sticky_infobox(&self) -> Option<Info>;

    /// Clear sticky state.
    fn clear_sticky(&mut self);
}

// ─── Function types for command composition ──────────────────────────

/// A motion that can be applied to the editor.
///
/// Captures all parameters (direction, word-type, count, etc.) in the closure.
/// The `Movement` parameter lets the engine control Move vs Extend.
/// `ViewId` and `DocumentId` identify the editing context explicitly.
pub type MotionFn = Box<dyn Fn(&mut Editor, ViewId, DocumentId, Movement) + Send + Sync>;

/// An operator that acts on the current selection.
pub type OperatorFn =
    Box<dyn Fn(&mut Editor, ViewId, DocumentId, char /* register */) + Send + Sync>;

/// A text object that selects a range.
pub type TextObjectFn =
    Box<dyn Fn(&mut Editor, ViewId, DocumentId, helix_core::textobject::TextObject) + Send + Sync>;

/// A command identifier — `Id<K, &'static str>` specialized per command kind.
pub type CommandId<K> = crate::id::Id<K, &'static str>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MotionKind {}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatorKind {}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextObjectKind {}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CharPendingKind {}

pub type MotionId = CommandId<MotionKind>;
pub type OperatorId = CommandId<OperatorKind>;
pub type TextObjectId = CommandId<TextObjectKind>;
pub type ActionId = CommandId<ActionKind>;
pub type CharPendingId = CommandId<CharPendingKind>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CharPendingBinding {
    id: CharPendingId,
    doc: &'static str,
}

impl CharPendingBinding {
    pub const fn new(id: CharPendingId, doc: &'static str) -> Self {
        Self { id, doc }
    }

    pub const fn id(self) -> CharPendingId {
        self.id
    }

    pub const fn doc(self) -> &'static str {
        self.doc
    }

    pub const FIND_NEXT_CHAR: Self = Self::new(
        CharPendingId::new("find_next_char"),
        "Find next occurrence of char",
    );
    pub const FIND_TILL_CHAR: Self = Self::new(
        CharPendingId::new("find_till_char"),
        "Find till next occurrence of char",
    );
    pub const FIND_PREV_CHAR: Self = Self::new(
        CharPendingId::new("find_prev_char"),
        "Find previous occurrence of char",
    );
    pub const TILL_PREV_CHAR: Self = Self::new(
        CharPendingId::new("till_prev_char"),
        "Find till previous occurrence of char",
    );
    pub const SELECT_TEXTOBJECT_INSIDE_SURROUNDING_PAIR: Self = Self::new(
        CharPendingId::new("select_textobject_inside_surrounding_pair"),
        "Select inside any character pair",
    );
    pub const SELECT_TEXTOBJECT_AROUND_SURROUNDING_PAIR: Self = Self::new(
        CharPendingId::new("select_textobject_around_surrounding_pair"),
        "Select around any character pair",
    );
    pub const SELECT_TEXTOBJECT_INSIDE_PREV_PAIR: Self = Self::new(
        CharPendingId::new("select_textobject_inside_prev_pair"),
        "Select inside previous character pair",
    );
    pub const SELECT_TEXTOBJECT_INSIDE_NEXT_PAIR: Self = Self::new(
        CharPendingId::new("select_textobject_inside_next_pair"),
        "Select inside next character pair",
    );

    #[allow(non_upper_case_globals)]
    pub const find_next_char: Self = Self::FIND_NEXT_CHAR;
    #[allow(non_upper_case_globals)]
    pub const find_till_char: Self = Self::FIND_TILL_CHAR;
    #[allow(non_upper_case_globals)]
    pub const find_prev_char: Self = Self::FIND_PREV_CHAR;
    #[allow(non_upper_case_globals)]
    pub const till_prev_char: Self = Self::TILL_PREV_CHAR;
    #[allow(non_upper_case_globals)]
    pub const select_textobject_inside_surrounding_pair: Self =
        Self::SELECT_TEXTOBJECT_INSIDE_SURROUNDING_PAIR;
    #[allow(non_upper_case_globals)]
    pub const select_textobject_around_surrounding_pair: Self =
        Self::SELECT_TEXTOBJECT_AROUND_SURROUNDING_PAIR;
    #[allow(non_upper_case_globals)]
    pub const select_textobject_inside_prev_pair: Self = Self::SELECT_TEXTOBJECT_INSIDE_PREV_PAIR;
    #[allow(non_upper_case_globals)]
    pub const select_textobject_inside_next_pair: Self = Self::SELECT_TEXTOBJECT_INSIDE_NEXT_PAIR;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepeatableCommandId {
    Operator(OperatorId),
    Action(ActionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatorTargetId {
    Motion(MotionId),
    TextObject(TextObjectId),
    CharPending(CharPendingId, char),
    Linewise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandToken {
    Motion(MotionId),
    Operator(OperatorId),
    TextObject(TextObjectId),
    Action(ActionId),
    CharPending(CharPendingId),
}

impl CommandToken {
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

// ─── Engine result ───────────────────────────────────────────────────

/// What the engine returns to the frontend after processing a key.
#[derive(Debug)]
pub enum EngineResult {
    /// Editor state was mutated. Frontend should re-render.
    Executed,

    /// Engine consumed the key but needs more input before executing.
    /// Frontend should update statusline with `pending_display()`.
    Pending,

    /// Insert this character literally (insert mode, key not bound).
    InsertChar(char),

    /// Multi-key sequence was cancelled. These keys were buffered and
    /// should be replayed as individual insertions (insert mode only).
    CancelledInsert(Box<[KeyEvent]>),

    /// Key was not bound to anything.
    Unbound,

    /// Dot-repeat of an insert sequence. The frontend should:
    /// 1. Execute the entry command (e.g., "insert_mode", "append_mode") to enter insert mode
    /// 2. Feed each key through the engine while in insert mode
    /// 3. Exit insert mode (the last key should be Esc)
    ReplayInsert {
        entry_command: Cow<'static, str>,
        keys: Box<[KeyEvent]>,
    },
}

// ─── Recorded action for dot-repeat ──────────────────────────────────

/// A recorded action for dot-repeat.
#[derive(Debug, Clone)]
pub enum RecordedAction {
    /// An operator applied to a motion (Vim: `d3w`).
    OperatorMotion {
        operator: OperatorId,
        target: OperatorTargetId,
        motion_count: NonZeroUsize,
        operator_count: NonZeroUsize,
        register: Option<char>,
    },
    /// An insert sequence (both engines: enter insert, type text, exit).
    /// `entry_command` is the frontend command name (e.g., "insert_mode", "append_mode").
    InsertSequence {
        entry_command: Cow<'static, str>,
        keys: Box<[KeyEvent]>,
    },
    /// A simple action with count (e.g., `3x`, `5J`).
    CountedAction {
        command: RepeatableCommandId,
        count: NonZeroUsize,
        register: Option<char>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModalInputState {
    pub count: Option<NonZeroUsize>,
    pub selected_register: Option<char>,
}

// ─── Engine trait ────────────────────────────────────────────────────

/// The editing engine owns the full key→mutation pipeline.
///
/// The frontend feeds keys. The engine handles mode transitions, count/register
/// accumulation, keymap resolution, operator×motion composition, and dot-repeat.
/// It mutates `Editor` directly for modal editor commands.
pub trait EditingEngine: Send {
    /// Pre-resolve phase: handle count accumulation, register selection, and
    /// dot-repeat BEFORE the keymap is queried.
    ///
    /// Returns `Some(result)` if the engine consumed the key (count digit,
    /// register prefix, dot-repeat). Returns `None` if the frontend should
    /// resolve the keymap and call `process_lookup`.
    fn pre_resolve(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &dyn KeymapQuery,
        key: KeyEvent,
    ) -> Option<EngineResult>;

    /// Process a pre-resolved keymap lookup.
    ///
    /// Called by the frontend after resolving the keymap. The engine executes the
    /// command without querying the keymap itself. `keymaps` is borrowed immutably
    /// for read-only queries (pending state, sticky infobox).
    fn process_lookup(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        keymaps: &mut dyn KeymapQuery,
        key: KeyEvent,
        lookup: KeymapLookup,
    ) -> EngineResult;

    /// Get the current mode for display.
    /// Returns the engine's internal mode name (e.g., "NOR", "INS",
    /// "VIS", "VLN", "VBL", "OPR" for operator-pending).
    fn mode_name(&self) -> &str;

    /// Get the editor Mode (Normal/Insert/Select) for cursor shape, gutter, etc.
    fn editor_mode(&self) -> Mode;

    /// Pending keys display for statusline (e.g., "d" in Vim operator-pending,
    /// "g" in Helix multi-key sequence, "3" during count accumulation).
    fn pending_display(&self) -> &str;

    /// Whether the engine is in a state that expects more input.
    fn is_pending(&self) -> bool;

    /// Reset all pending state (Escape pressed).
    fn reset(&mut self);

    /// Engine name for config and display.
    fn name(&self) -> &str;

    /// Get the last recorded action for dot-repeat.
    fn last_action(&self) -> Option<&RecordedAction>;

    /// Replay the last recorded action (dot-repeat).
    fn repeat_last(
        &mut self,
        editor: &mut Editor,
        view_id: ViewId,
        doc_id: DocumentId,
        count: NonZeroUsize,
    ) -> EngineResult;

    /// Notify the engine that insert mode was entered by a frontend command.
    ///
    /// The engine begins recording keys for dot-repeat. The `entry_command` is the
    /// name of the frontend command that entered insert mode (e.g., "insert_mode",
    /// "append_mode", "open_below"). The engine does NOT execute this command — the
    /// frontend already did.
    fn begin_insert_recording(&mut self, entry_command: Cow<'static, str>);

    /// Notify the engine that insert mode was exited.
    ///
    /// The engine finalizes the insert recording into a `RecordedAction::InsertSequence`
    /// and stores it as `last_action` for dot-repeat.
    fn end_insert_recording(&mut self);

    /// Snapshot transient count/register state owned by the engine.
    fn input_state(&self) -> ModalInputState {
        ModalInputState::default()
    }

    /// Replace transient count/register state owned by the engine.
    fn set_input_state(&mut self, _state: ModalInputState) {}
}

/// Creates fresh engine instances for component-owned edit regions.
pub trait EditingEngineFactory: Send + Sync {
    fn create(&self, config: crate::editor::EditingEngineConfig) -> Box<dyn EditingEngine>;
}

#[derive(Debug, Default)]
pub struct HeadlessEditingEngineFactory;

impl EditingEngineFactory for HeadlessEditingEngineFactory {
    fn create(&self, _config: crate::editor::EditingEngineConfig) -> Box<dyn EditingEngine> {
        Box::new(HeadlessEditingEngine)
    }
}

#[derive(Debug)]
struct HeadlessEditingEngine;

impl EditingEngine for HeadlessEditingEngine {
    fn pre_resolve(
        &mut self,
        _editor: &mut Editor,
        _view_id: ViewId,
        _doc_id: DocumentId,
        _keymaps: &dyn KeymapQuery,
        _key: KeyEvent,
    ) -> Option<EngineResult> {
        None
    }

    fn process_lookup(
        &mut self,
        _editor: &mut Editor,
        _view_id: ViewId,
        _doc_id: DocumentId,
        _keymaps: &mut dyn KeymapQuery,
        _key: KeyEvent,
        _lookup: KeymapLookup,
    ) -> EngineResult {
        EngineResult::Unbound
    }

    fn mode_name(&self) -> &str {
        "HEADLESS"
    }

    fn editor_mode(&self) -> Mode {
        Mode::Normal
    }

    fn pending_display(&self) -> &str {
        ""
    }

    fn is_pending(&self) -> bool {
        false
    }

    fn reset(&mut self) {}

    fn name(&self) -> &str {
        "headless"
    }

    fn last_action(&self) -> Option<&RecordedAction> {
        None
    }

    fn repeat_last(
        &mut self,
        _editor: &mut Editor,
        _view_id: ViewId,
        _doc_id: DocumentId,
        _count: NonZeroUsize,
    ) -> EngineResult {
        EngineResult::Unbound
    }

    fn begin_insert_recording(&mut self, _entry_command: Cow<'static, str>) {}

    fn end_insert_recording(&mut self) {}
}
