use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

use arc_swap::ArcSwap;
use helix_core::movement::{Direction, Movement as CoreMovement};
use helix_view::{
    document::Mode,
    editor::{Action, EditingEngineConfig},
    engine::{CharPendingId, CommandToken, ModalInputState},
    info::Info,
    input::KeyEvent,
    keymap::{
        ComponentIntentId, FrontendIntentId, ModalIntent, ModalIntentBinding, ModalIntentKeymaps,
        ModalIntentLookup, ModalIntentTrie, ModalIntentTrieNode,
    },
    modal_text::{ModalTextMotion as LabelMotion, ModalTextObject as LabelTextObject},
    Editor,
};

use crate::{alt, ctrl, key};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExplorerFileOperation {
    Copy,
    Move,
}

impl ExplorerFileOperation {
    pub(super) const fn status_verb(self) -> &'static str {
        match self {
            Self::Copy => "Yanked",
            Self::Move => "Cut",
        }
    }

    pub(super) const fn paste_verb(self) -> &'static str {
        match self {
            Self::Copy => "Copied",
            Self::Move => "Moved",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExplorerPastePlacement {
    After,
    Before,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExplorerOperator {
    Yank,
    Delete { yank: bool },
    Change { yank: bool },
}

impl ExplorerOperator {
    const fn doubled_key(self) -> char {
        match self {
            Self::Yank => 'y',
            Self::Delete { .. } => 'd',
            Self::Change { .. } => 'c',
        }
    }

    const fn selection_action(self) -> ExplorerAction {
        match self {
            Self::Yank => ExplorerAction::ClipboardOperation(ExplorerFileOperation::Copy),
            Self::Delete { yank } => ExplorerAction::DeleteLabelSelection { yank },
            Self::Change { yank } => ExplorerAction::ChangeLabelSelection { yank },
        }
    }

    const fn linewise_action(self) -> ExplorerAction {
        match self {
            Self::Yank => ExplorerAction::ClipboardOperation(ExplorerFileOperation::Copy),
            Self::Delete { yank } => ExplorerAction::DeleteSelectedItem { yank },
            Self::Change { .. } => {
                ExplorerAction::EnterLabelEdit(helix_view::edit_region::InsertEntry::AtCurrent)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingExplorerOperator {
    operator: ExplorerOperator,
    text_object_kind: Option<ExplorerTextObjectKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExplorerTextObjectKind {
    Inside,
    Around,
}

impl ExplorerTextObjectKind {
    const fn word(self, long: bool) -> LabelTextObject {
        match (self, long) {
            (Self::Inside, false) => LabelTextObject::InsideWord,
            (Self::Around, false) => LabelTextObject::AroundWord,
            (Self::Inside, true) => LabelTextObject::InsideLongWord,
            (Self::Around, true) => LabelTextObject::AroundLongWord,
        }
    }

    const fn paragraph(self) -> LabelTextObject {
        match self {
            Self::Inside => LabelTextObject::InsideParagraph,
            Self::Around => LabelTextObject::AroundParagraph,
        }
    }

    const fn surrounding_pair(self, ch: char) -> LabelTextObject {
        match self {
            Self::Inside => LabelTextObject::InsideSurroundingPair(ch),
            Self::Around => LabelTextObject::AroundSurroundingPair(ch),
        }
    }

    const fn closest_pair(self) -> LabelTextObject {
        match self {
            Self::Inside => LabelTextObject::InsideClosestPair,
            Self::Around => LabelTextObject::AroundClosestPair,
        }
    }
}

pub(super) struct ExplorerInputEngine {
    keymap_generation: Option<u64>,
    keymaps: ModalIntentKeymaps,
    editing_engine: EditingEngineConfig,
    pub(super) mode: Mode,
    pub(super) count: Option<NonZeroUsize>,
    pub(super) selected_register: Option<char>,
    pending_register: bool,
    pending_operator: Option<PendingExplorerOperator>,
}

impl Default for ExplorerInputEngine {
    fn default() -> Self {
        Self {
            keymap_generation: None,
            keymaps: ModalIntentKeymaps::default(),
            editing_engine: EditingEngineConfig::Helix,
            mode: Mode::Normal,
            count: None,
            selected_register: None,
            pending_register: false,
            pending_operator: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) enum ExplorerInput {
    Pending(Option<Info>),
    Execute(ExplorerAction),
}

impl PartialEq for ExplorerInput {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Pending(lhs), Self::Pending(rhs)) => lhs.is_some() == rhs.is_some(),
            (Self::Execute(lhs), Self::Execute(rhs)) => lhs == rhs,
            (Self::Pending(_), Self::Execute(_)) | (Self::Execute(_), Self::Pending(_)) => false,
        }
    }
}

impl Eq for ExplorerInput {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExplorerAction {
    Close,
    MoveSelection(isize),
    Page(isize),
    SelectFirst,
    SelectLast,
    Open(Action),
    ToggleDirectory,
    CollapseOrSelectParent,
    RootParent,
    GoWorkspaceRoot,
    UndoFileOperation,
    RedoFileOperation,
    Refresh,
    ShowHelp,
    SelectFirstDiagnostic,
    SelectLastDiagnostic,
    SelectNextDiagnostic,
    SelectPreviousDiagnostic,
    MoveLabelSelection(LabelMotion, CoreMovement),
    SelectLabelTextObject(LabelTextObject),
    SelectWholeLabel,
    CollapseLabelSelection,
    FlipLabelSelection,
    SetMode(Mode),
    ClipboardOperation(ExplorerFileOperation),
    PasteClipboard(ExplorerPastePlacement),
    ApplyOperatorTextObject(ExplorerOperator, LabelTextObject),
    ApplyOperatorMotion(ExplorerOperator, LabelMotion),
    BeginOperator(ExplorerOperator),
    DeleteLabelSelection {
        yank: bool,
    },
    ChangeLabelSelection {
        yank: bool,
    },
    DeleteSelectedItem {
        yank: bool,
    },
    /// Start an inline rename of the selected row, parameterized by the
    /// editor's Insert-mode entry semantics. `i` → `InsertEntry::AtCurrent`,
    /// `a` → `Append`, `I` → `AtLineStart`, `A` → `AtLineEnd`. The
    /// shared [`helix_view::edit_region::EditRegion`] does the cursor
    /// transform so the file explorer can't drift from the main editor's
    /// behavior.
    EnterLabelEdit(helix_view::edit_region::InsertEntry),
    /// Start an inline create — `o` from a directory row creates a new
    /// entry inside that directory, from a file row creates a sibling in
    /// the same parent. Buffer starts empty.
    EnterCreate,
    /// Begin a label-jump session over visible rows. Triggered by Helix's
    /// `goto_word` Frontend intent (`gw`). The panel renders two-letter
    /// labels over each visible row's label, then intercepts the user's
    /// next 1–2 keystrokes via [`helix_view::jump_labels::JumpSession`]
    /// to resolve which row to jump to.
    StartJumpSession,
    DelegateToEditor,
    Noop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExplorerCommand {
    Close,
    SelectPrevious,
    SelectNext,
    PageUp,
    PageDown,
    SelectFirst,
    SelectLast,
    Open,
    ToggleDirectory,
    CollapseOrSelectParent,
    RootParent,
    GoWorkspaceRoot,
    UndoFileOperation,
    RedoFileOperation,
    Refresh,
    ShowHelp,
}

impl ExplorerCommand {
    const ALL: [Self; 16] = [
        Self::Close,
        Self::SelectPrevious,
        Self::SelectNext,
        Self::PageUp,
        Self::PageDown,
        Self::SelectFirst,
        Self::SelectLast,
        Self::Open,
        Self::ToggleDirectory,
        Self::CollapseOrSelectParent,
        Self::RootParent,
        Self::GoWorkspaceRoot,
        Self::UndoFileOperation,
        Self::RedoFileOperation,
        Self::Refresh,
        Self::ShowHelp,
    ];

    const fn id(self) -> ComponentIntentId {
        ComponentIntentId::new(match self {
            Self::Close => "file_explorer.close",
            Self::SelectPrevious => "file_explorer.select_previous",
            Self::SelectNext => "file_explorer.select_next",
            Self::PageUp => "file_explorer.page_up",
            Self::PageDown => "file_explorer.page_down",
            Self::SelectFirst => "file_explorer.select_first",
            Self::SelectLast => "file_explorer.select_last",
            Self::Open => "file_explorer.open",
            Self::ToggleDirectory => "file_explorer.toggle_directory",
            Self::CollapseOrSelectParent => "file_explorer.collapse_or_select_parent",
            Self::RootParent => "file_explorer.root_parent",
            Self::GoWorkspaceRoot => "file_explorer.go_workspace_root",
            Self::UndoFileOperation => "file_explorer.undo_file_operation",
            Self::RedoFileOperation => "file_explorer.redo_file_operation",
            Self::Refresh => "file_explorer.refresh",
            Self::ShowHelp => "file_explorer.show_help",
        })
    }

    const fn intent(self) -> ModalIntent {
        ModalIntent::component(self.id())
    }

    const fn doc(self) -> &'static str {
        match self {
            Self::Close => "Close file explorer",
            Self::SelectPrevious => "Select previous item",
            Self::SelectNext => "Select next item",
            Self::PageUp => "Move one page up",
            Self::PageDown => "Move one page down",
            Self::SelectFirst => "Select first item",
            Self::SelectLast => "Select last item",
            Self::Open => "Open file or toggle directory",
            Self::ToggleDirectory => "Toggle directory",
            Self::CollapseOrSelectParent => "Collapse directory or select parent",
            Self::RootParent => "Open parent directory",
            Self::GoWorkspaceRoot => "Go to workspace root",
            Self::UndoFileOperation => "Undo file operation",
            Self::RedoFileOperation => "Redo file operation",
            Self::Refresh => "Refresh tree",
            Self::ShowHelp => "Show explorer key bindings",
        }
    }

    fn from_component_id(id: ComponentIntentId) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.id() == id)
    }

    const fn action(self) -> ExplorerAction {
        match self {
            Self::Close => ExplorerAction::Close,
            Self::SelectPrevious => ExplorerAction::MoveSelection(-1),
            Self::SelectNext => ExplorerAction::MoveSelection(1),
            Self::PageUp => ExplorerAction::Page(-1),
            Self::PageDown => ExplorerAction::Page(1),
            Self::SelectFirst => ExplorerAction::SelectFirst,
            Self::SelectLast => ExplorerAction::SelectLast,
            Self::Open => ExplorerAction::Open(Action::Replace),
            Self::ToggleDirectory => ExplorerAction::ToggleDirectory,
            Self::CollapseOrSelectParent => ExplorerAction::CollapseOrSelectParent,
            Self::RootParent => ExplorerAction::RootParent,
            Self::GoWorkspaceRoot => ExplorerAction::GoWorkspaceRoot,
            Self::UndoFileOperation => ExplorerAction::UndoFileOperation,
            Self::RedoFileOperation => ExplorerAction::RedoFileOperation,
            Self::Refresh => ExplorerAction::Refresh,
            Self::ShowHelp => ExplorerAction::ShowHelp,
        }
    }
}

struct ExplorerKeymapBuilder {
    name: &'static str,
    map: HashMap<KeyEvent, ModalIntentTrie>,
    order: Vec<KeyEvent>,
}

impl ExplorerKeymapBuilder {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            map: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn command(&mut self, key: KeyEvent, command: ExplorerCommand) {
        self.insert(key, ModalIntentTrie::Binding(command_binding(command)));
    }

    fn node(&mut self, key: KeyEvent, node: ExplorerKeymapBuilder) {
        self.insert(key, node.finish());
    }

    fn insert(&mut self, key: KeyEvent, trie: ModalIntentTrie) {
        if !self.map.contains_key(&key) {
            self.order.push(key);
        }
        self.map.insert(key, trie);
    }

    fn finish(self) -> ModalIntentTrie {
        ModalIntentTrie::Node(ModalIntentTrieNode::new(self.name, self.map, self.order))
    }
}

fn command_binding(command: ExplorerCommand) -> ModalIntentBinding {
    ModalIntentBinding::new(command.intent(), command.doc())
}

fn explorer_modal_keymaps(editor: &Editor) -> ModalIntentKeymaps {
    let source = editor.frontend().semantic_modal_keymaps.load();
    explorer_modal_keymaps_from_source(&source, editor.config().editing_engine)
}

fn explorer_modal_keymaps_from_source(
    source: &HashMap<Mode, ModalIntentTrie>,
    editing_engine: EditingEngineConfig,
) -> ModalIntentKeymaps {
    let mut modes = HashMap::new();
    for (&mode, trie) in source {
        if let Some(filtered) = filter_explorer_modal_trie(trie) {
            modes.insert(mode, filtered);
        }
    }

    let local = explorer_local_keymap(editing_engine);
    modes
        .entry(Mode::Normal)
        .and_modify(|trie| merge_modal_trie_missing(trie, local.clone()))
        .or_insert(local);

    ModalIntentKeymaps::from_shared(Arc::new(ArcSwap::from_pointee(modes)))
}

pub(super) fn explorer_local_keymap(editing_engine: EditingEngineConfig) -> ModalIntentTrie {
    let mut root = ExplorerKeymapBuilder::new("File Explorer");
    root.command(key!(Esc), ExplorerCommand::Close);
    root.command(ctrl!('c'), ExplorerCommand::Close);
    root.command(key!('q'), ExplorerCommand::Close);
    root.command(key!('?'), ExplorerCommand::ShowHelp);

    root.command(key!(Enter), ExplorerCommand::Open);
    root.command(alt!(Enter), ExplorerCommand::Open);
    root.command(key!(' '), ExplorerCommand::ToggleDirectory);
    root.command(key!(Left), ExplorerCommand::CollapseOrSelectParent);
    root.command(key!(Backspace), ExplorerCommand::RootParent);

    if editing_engine == EditingEngineConfig::Vim {
        root.command(ctrl!('r'), ExplorerCommand::RedoFileOperation);
    }

    let mut goto = ExplorerKeymapBuilder::new("Explorer goto");
    goto.command(key!('g'), ExplorerCommand::SelectFirst);
    // `gw` used to be bound to `GoWorkspaceRoot` (reset the explorer's
    // root to the workspace folder). That collided with Helix's
    // `goto_word` binding — pressing `gw` to "jump to a labelled word"
    // silently switched the explorer's root instead, which felt like
    // the cursor was "going to root". We leave `gw` unbound here so it
    // falls through to the filtered editor keymap. `goto_word` itself
    // is a Frontend command and currently no-ops in the explorer, but
    // that's quiet rather than surprising. If we want to restore the
    // workspace-root operation later, bind it to a fresh key (e.g.
    // `<space>r`) that doesn't compete with editor semantics.
    root.node(key!('g'), goto);

    root.finish()
}

fn filter_explorer_modal_trie(trie: &ModalIntentTrie) -> Option<ModalIntentTrie> {
    match trie {
        ModalIntentTrie::Binding(binding) => {
            explorer_binding_for_intent(binding.intent()).map(ModalIntentTrie::Binding)
        }
        ModalIntentTrie::Sequence(commands) => {
            let commands = commands
                .iter()
                .filter_map(|command| explorer_binding_for_intent(command.intent()))
                .collect::<Vec<_>>();
            (!commands.is_empty()).then(|| ModalIntentTrie::Sequence(commands.into_boxed_slice()))
        }
        ModalIntentTrie::Node(node) => {
            let mut map = HashMap::new();
            let mut order = Vec::new();
            for key in &node.order {
                let Some(child) = node.map.get(key).and_then(filter_explorer_modal_trie) else {
                    continue;
                };
                map.insert(*key, child);
                order.push(*key);
            }

            let fallback = node
                .fallback
                .filter(|fallback| explorer_char_pending_doc(fallback.id()).is_some());

            if map.is_empty() && fallback.is_none() {
                return None;
            }
            let mut filtered = ModalIntentTrieNode::new(&node.name, map, order);
            filtered.is_sticky = node.is_sticky;
            filtered.fallback = fallback;
            Some(ModalIntentTrie::Node(filtered))
        }
    }
}

fn explorer_binding_for_intent(intent: ModalIntent) -> Option<ModalIntentBinding> {
    let doc = explorer_doc_for_intent(intent)?;
    Some(ModalIntentBinding::new(intent, doc))
}

fn explorer_doc_for_intent(intent: ModalIntent) -> Option<&'static str> {
    match intent {
        ModalIntent::Component(id) => {
            ExplorerCommand::from_component_id(id).map(ExplorerCommand::doc)
        }
        ModalIntent::Engine(token) => explorer_engine_doc(token),
        ModalIntent::Frontend(id) => explorer_frontend_doc(id),
    }
}

fn explorer_engine_doc(token: CommandToken) -> Option<&'static str> {
    match token {
        CommandToken::Motion(id) => match id.as_str() {
            "move_line_up" | "move_visual_line_up" | "extend_line_up" | "extend_visual_line_up" => {
                Some("Select previous item")
            }
            "move_line_down"
            | "move_visual_line_down"
            | "extend_line_down"
            | "extend_visual_line_down" => Some("Select next item"),
            "move_char_left" | "extend_char_left" => Some("Move label cursor left"),
            "move_char_right" | "extend_char_right" => Some("Move label cursor right"),
            "goto_line_start" | "goto_first_nonwhitespace" => Some("Move label cursor to start"),
            "extend_to_line_start" | "extend_to_first_nonwhitespace" => {
                Some("Extend label selection to start")
            }
            "goto_line_end" | "goto_line_end_newline" => Some("Move label cursor to end"),
            "extend_to_line_end" | "extend_to_line_end_newline" => {
                Some("Extend label selection to end")
            }
            "move_next_word_start"
            | "move_next_long_word_start"
            | "move_next_sub_word_start"
            | "extend_next_word_start"
            | "extend_next_long_word_start"
            | "extend_next_sub_word_start" => Some("Move label cursor to next word"),
            "move_prev_word_start"
            | "move_prev_long_word_start"
            | "move_prev_sub_word_start"
            | "extend_prev_word_start"
            | "extend_prev_long_word_start"
            | "extend_prev_sub_word_start" => Some("Move label cursor to previous word"),
            "move_next_word_end"
            | "move_next_long_word_end"
            | "move_next_sub_word_end"
            | "extend_next_word_end"
            | "extend_next_long_word_end"
            | "extend_next_sub_word_end" => Some("Move label cursor to next word end"),
            "move_prev_word_end"
            | "move_prev_long_word_end"
            | "move_prev_sub_word_end"
            | "extend_prev_word_end"
            | "extend_prev_long_word_end"
            | "extend_prev_sub_word_end" => Some("Move label cursor to previous word end"),
            "goto_file_start" | "extend_to_file_start" => Some("Select first item"),
            "goto_file_end" | "goto_last_line" | "extend_to_file_end" | "extend_to_last_line" => {
                Some("Select last item")
            }
            "extend_line_below" | "extend_to_line_bounds" => Some("Select whole item label"),
            _ => None,
        },
        CommandToken::Action(id) => match id.as_str() {
            "undo" => Some("Undo file operation"),
            "redo" => Some("Redo file operation"),
            "select_mode" => Some("Enter label selection mode"),
            "normal_mode" | "exit_select_mode" => Some("Exit label selection mode"),
            "paste_after" => Some("Paste file operation after selection"),
            "paste_before" => Some("Paste file operation before selection"),
            "extend_line_below" | "extend_to_line_bounds" => Some("Select whole item label"),
            "select_all" => Some("Select whole item label"),
            "collapse_selection" => Some("Collapse label selection"),
            "flip_selections" => Some("Flip label selection direction"),
            "page_up" | "page_cursor_half_up" => Some("Move one page up"),
            "page_down" | "page_cursor_half_down" => Some("Move one page down"),
            _ => None,
        },
        CommandToken::Operator(id) => match id.as_str() {
            "yank" | "yank_joined" => Some("Yank selected path for file paste"),
            "delete_selection" => Some("Delete selected label text"),
            "delete_selection_noyank" => Some("Delete selected label text without yanking"),
            "change_selection" => Some("Change selected label text"),
            "change_selection_noyank" => Some("Change selected label text without yanking"),
            _ => None,
        },
        CommandToken::TextObject(id) => match id.as_str() {
            "textobject_word" => Some("Select label word"),
            "textobject_long_word" => Some("Select label WORD"),
            _ => None,
        },
        CommandToken::CharPending(_) => None,
    }
}

fn explorer_char_pending_doc(id: CharPendingId) -> Option<&'static str> {
    match id.as_str() {
        "find_next_char" | "extend_next_char" => Some("Move label cursor to next character"),
        "find_till_char" | "extend_till_char" => Some("Move label cursor until next character"),
        "find_prev_char" | "extend_prev_char" => Some("Move label cursor to previous character"),
        "till_prev_char" | "extend_till_prev_char" => {
            Some("Move label cursor until previous character")
        }
        "select_textobject_inside_surrounding_pair" => Some("Select inside label pair"),
        "select_textobject_around_surrounding_pair" => Some("Select around label pair"),
        "select_textobject_inside_prev_pair" => Some("Select inside previous label pair"),
        "select_textobject_inside_next_pair" => Some("Select inside next label pair"),
        _ => None,
    }
}

fn explorer_frontend_doc(id: FrontendIntentId) -> Option<&'static str> {
    match id.as_str() {
        "goto_first_diag" => Some("Select first diagnostic"),
        "goto_last_diag" => Some("Select last diagnostic"),
        "goto_next_diag" => Some("Select next diagnostic"),
        "goto_prev_diag" => Some("Select previous diagnostic"),
        "command_mode" => Some("Enter command mode"),
        // Inline rename / create entry — these must pass through the
        // explorer's keymap filter so the editor's `i` / `a` / `I` / `A` /
        // `o` / `O` bindings reach the action handler.
        "insert_mode" => Some("Edit name inline (insert)"),
        "append_mode" => Some("Edit name inline (append)"),
        "insert_at_line_start" => Some("Edit name inline (at start)"),
        "insert_at_line_end" => Some("Edit name inline (at end)"),
        "open_below" => Some("Create path below selection"),
        "open_above" => Some("Create path above selection"),
        "goto_word" => Some("Jump to row by two-letter label"),
        "select_textobject_inside_word" => Some("Select label word"),
        "select_textobject_around_word" => Some("Select label word"),
        "select_textobject_inside_WORD" => Some("Select label WORD"),
        "select_textobject_around_WORD" => Some("Select label WORD"),
        "select_textobject_inside_paragraph" | "select_textobject_around_paragraph" => {
            Some("Select whole item label")
        }
        "select_textobject_inside_closest_surrounding_pair" => {
            Some("Select inside closest label pair")
        }
        "select_textobject_around_closest_surrounding_pair" => {
            Some("Select around closest label pair")
        }
        "select_all" => Some("Select whole item label"),
        "collapse_selection" => Some("Collapse label selection"),
        "flip_selections" => Some("Flip label selection direction"),
        _ => None,
    }
}

fn merge_modal_trie_missing(target: &mut ModalIntentTrie, source: ModalIntentTrie) {
    let (ModalIntentTrie::Node(target), ModalIntentTrie::Node(source)) = (target, source) else {
        return;
    };

    for key in source.order {
        let Some(source_child) = source.map.get(&key).cloned() else {
            continue;
        };
        match target.map.get_mut(&key) {
            Some(target_child) => merge_modal_trie_missing(target_child, source_child),
            None => {
                target.map.insert(key, source_child);
                target.order.push(key);
            }
        }
    }
}

impl ExplorerInputEngine {
    pub(super) fn prepare_keymaps(&mut self, editor: &Editor) {
        self.editing_engine = editor.config().editing_engine;
        if self.keymap_generation == Some(editor.config_gen) {
            return;
        }
        self.keymap_generation = Some(editor.config_gen);
        self.keymaps = explorer_modal_keymaps(editor);
    }

    #[cfg(test)]
    pub(super) fn prepare_test_keymaps(&mut self, editing_engine: EditingEngineConfig) {
        let source = crate::keymap::to_semantic_modal_keymaps(&crate::keymap::default());
        self.keymaps = explorer_modal_keymaps_from_source(&source, editing_engine);
        self.editing_engine = editing_engine;
        self.keymap_generation = Some(0);
    }

    pub(super) fn translate(&mut self, key: KeyEvent) -> ExplorerInput {
        if self.pending_operator.is_some() {
            return self.translate_pending_operator(key);
        }

        if self.pending_register {
            self.pending_register = false;
            if let Some(register) = register_key(key) {
                self.selected_register = Some(register);
            }
            return ExplorerInput::Pending(None);
        }

        if is_register_prefix(key) {
            self.pending_register = true;
            return ExplorerInput::Pending(None);
        }

        if let Some(digit) = count_digit(key, self.count) {
            self.push_count_digit(digit);
            return ExplorerInput::Pending(None);
        }

        let lookup = self.keymaps.get(self.mode, key);
        self.translate_lookup(lookup)
    }

    fn translate_lookup(&mut self, lookup: ModalIntentLookup) -> ExplorerInput {
        match lookup {
            ModalIntentLookup::Matched(intent) => {
                let action = self.action_for_intent(intent);
                self.translate_action(action)
            }
            ModalIntentLookup::MatchedSequence(intents) => {
                let mut action = ExplorerAction::Noop;
                for intent in intents {
                    let next = self.action_for_intent(intent);
                    if !matches!(next, ExplorerAction::Noop) {
                        action = next;
                        break;
                    }
                }
                self.translate_action(action)
            }
            ModalIntentLookup::Pending(info) => ExplorerInput::Pending(info),
            ModalIntentLookup::Fallback(id, ch) => {
                let action = self
                    .action_for_char_pending(id, ch)
                    .unwrap_or(ExplorerAction::Noop);
                self.translate_action(action)
            }
            ModalIntentLookup::NotFound | ModalIntentLookup::Cancelled(_) => {
                ExplorerInput::Execute(ExplorerAction::Noop)
            }
        }
    }

    fn translate_action(&mut self, action: ExplorerAction) -> ExplorerInput {
        match action {
            ExplorerAction::BeginOperator(operator) => {
                self.pending_operator = Some(PendingExplorerOperator {
                    operator,
                    text_object_kind: None,
                });
                ExplorerInput::Pending(None)
            }
            action => ExplorerInput::Execute(action),
        }
    }

    fn translate_pending_operator(&mut self, key: KeyEvent) -> ExplorerInput {
        if key == key!(Esc) {
            self.pending_operator = None;
            return ExplorerInput::Execute(ExplorerAction::Noop);
        }

        if let Some(digit) = count_digit(key, self.count) {
            self.push_count_digit(digit);
            return ExplorerInput::Pending(None);
        }

        let Some(mut pending) = self.pending_operator else {
            return ExplorerInput::Execute(ExplorerAction::Noop);
        };

        if pending.text_object_kind.is_none() {
            if key == key!('i') {
                pending.text_object_kind = Some(ExplorerTextObjectKind::Inside);
                self.pending_operator = Some(pending);
                return ExplorerInput::Pending(None);
            }
            if key == key!('a') {
                pending.text_object_kind = Some(ExplorerTextObjectKind::Around);
                self.pending_operator = Some(pending);
                return ExplorerInput::Pending(None);
            }
        }

        if key.char() == Some(pending.operator.doubled_key()) {
            self.pending_operator = None;
            return ExplorerInput::Execute(pending.operator.linewise_action());
        }

        if let Some(kind) = pending.text_object_kind {
            let object = match key.char() {
                Some('w') => Some(kind.word(false)),
                Some('W') => Some(kind.word(true)),
                Some('p') => Some(kind.paragraph()),
                Some('m') => Some(kind.closest_pair()),
                Some(ch) if is_label_pair_char(ch) => Some(kind.surrounding_pair(ch)),
                _ => None,
            };
            if let Some(object) = object {
                self.pending_operator = None;
                return ExplorerInput::Execute(ExplorerAction::ApplyOperatorTextObject(
                    pending.operator,
                    object,
                ));
            }
        }

        match self.keymaps.get(Mode::Normal, key) {
            ModalIntentLookup::Matched(intent) => {
                self.translate_pending_operator_intent(pending.operator, intent)
            }
            ModalIntentLookup::Fallback(id, ch) => {
                let action = self
                    .action_for_char_pending(id, ch)
                    .map(|action| self.operator_target_action(pending.operator, action))
                    .unwrap_or(ExplorerAction::Noop);
                self.pending_operator = None;
                ExplorerInput::Execute(action)
            }
            ModalIntentLookup::Pending(info) => ExplorerInput::Pending(info),
            ModalIntentLookup::NotFound
            | ModalIntentLookup::Cancelled(_)
            | ModalIntentLookup::MatchedSequence(_) => {
                self.pending_operator = None;
                ExplorerInput::Execute(ExplorerAction::Noop)
            }
        }
    }

    fn translate_pending_operator_intent(
        &mut self,
        operator: ExplorerOperator,
        intent: ModalIntent,
    ) -> ExplorerInput {
        let action = self.action_for_intent(intent);
        let action = self.operator_target_action(operator, action);
        self.pending_operator = None;
        ExplorerInput::Execute(action)
    }

    fn operator_target_action(
        &self,
        operator: ExplorerOperator,
        action: ExplorerAction,
    ) -> ExplorerAction {
        match action {
            ExplorerAction::MoveLabelSelection(motion, _) => {
                ExplorerAction::ApplyOperatorMotion(operator, motion)
            }
            ExplorerAction::SelectLabelTextObject(object) => {
                ExplorerAction::ApplyOperatorTextObject(operator, object)
            }
            ExplorerAction::BeginOperator(next_operator)
                if next_operator.doubled_key() == operator.doubled_key() =>
            {
                operator.linewise_action()
            }
            _ => ExplorerAction::Noop,
        }
    }

    pub(super) fn modal_input_state(&self) -> ModalInputState {
        ModalInputState {
            count: self.count,
            selected_register: self.selected_register,
        }
    }

    pub(super) fn finish_command(&mut self) {
        self.count = None;
        self.selected_register = None;
        self.pending_register = false;
        self.pending_operator = None;
    }

    pub(super) fn root_infobox(&self) -> Option<Info> {
        self.keymaps
            .map()
            .get(&Mode::Normal)
            .and_then(ModalIntentTrie::node)
            .map(|node| {
                let mut info = explorer_keymap_infobox(node);
                info.title = "File Explorer".into();
                info
            })
    }

    fn action_for_intent(&mut self, intent: ModalIntent) -> ExplorerAction {
        match intent {
            ModalIntent::Component(id) => ExplorerCommand::from_component_id(id)
                .map(|command| self.action_for_command(command))
                .unwrap_or(ExplorerAction::Noop),
            ModalIntent::Engine(token) => self
                .action_for_engine_token(token)
                .unwrap_or(ExplorerAction::Noop),
            ModalIntent::Frontend(id) => self
                .action_for_frontend_intent(id)
                .unwrap_or(ExplorerAction::Noop),
        }
    }

    fn action_for_command(&mut self, command: ExplorerCommand) -> ExplorerAction {
        let count = self.count.map(NonZeroUsize::get).unwrap_or(1);
        match command {
            ExplorerCommand::SelectPrevious => ExplorerAction::MoveSelection(-(count as isize)),
            ExplorerCommand::SelectNext => ExplorerAction::MoveSelection(count as isize),
            ExplorerCommand::PageUp => ExplorerAction::Page(-(count as isize)),
            ExplorerCommand::PageDown => ExplorerAction::Page(count as isize),
            _ => command.action(),
        }
    }

    fn action_for_engine_token(&self, token: CommandToken) -> Option<ExplorerAction> {
        let count = self.count.map(NonZeroUsize::get).unwrap_or(1);
        let signed_count = isize::try_from(count).unwrap_or(isize::MAX);
        let word_movement = if self.mode == Mode::Select {
            CoreMovement::Extend
        } else {
            CoreMovement::Move
        };
        match token {
            CommandToken::Motion(id) => match id.as_str() {
                "move_line_up" | "move_visual_line_up" => {
                    Some(ExplorerAction::MoveSelection(-signed_count))
                }
                "move_line_down" | "move_visual_line_down" => {
                    Some(ExplorerAction::MoveSelection(signed_count))
                }
                "extend_line_up" | "extend_visual_line_up" => {
                    Some(ExplorerAction::MoveSelection(-signed_count))
                }
                "extend_line_down" | "extend_visual_line_down" => {
                    Some(ExplorerAction::MoveSelection(signed_count))
                }
                "move_char_left" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::Char(-signed_count),
                    word_movement,
                )),
                "move_char_right" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::Char(signed_count),
                    word_movement,
                )),
                "goto_line_start" | "goto_first_nonwhitespace" => Some(
                    ExplorerAction::MoveLabelSelection(LabelMotion::LineStart, word_movement),
                ),
                "goto_line_end" | "goto_line_end_newline" => Some(
                    ExplorerAction::MoveLabelSelection(LabelMotion::LineEnd, word_movement),
                ),
                "move_next_word_start"
                | "move_next_long_word_start"
                | "move_next_sub_word_start" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::NextWordStart(count),
                    word_movement,
                )),
                "move_prev_word_start"
                | "move_prev_long_word_start"
                | "move_prev_sub_word_start" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::PrevWordStart(count),
                    word_movement,
                )),
                "move_next_word_end" | "move_next_long_word_end" | "move_next_sub_word_end" => {
                    Some(ExplorerAction::MoveLabelSelection(
                        LabelMotion::NextWordEnd(count),
                        word_movement,
                    ))
                }
                "move_prev_word_end" | "move_prev_long_word_end" | "move_prev_sub_word_end" => {
                    Some(ExplorerAction::MoveLabelSelection(
                        LabelMotion::PrevWordEnd(count),
                        word_movement,
                    ))
                }
                "extend_char_left" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::Char(-signed_count),
                    CoreMovement::Extend,
                )),
                "extend_char_right" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::Char(signed_count),
                    CoreMovement::Extend,
                )),
                "extend_to_line_start" | "extend_to_first_nonwhitespace" => {
                    Some(ExplorerAction::MoveLabelSelection(
                        LabelMotion::LineStart,
                        CoreMovement::Extend,
                    ))
                }
                "extend_to_line_end" | "extend_to_line_end_newline" => Some(
                    ExplorerAction::MoveLabelSelection(LabelMotion::LineEnd, CoreMovement::Extend),
                ),
                "extend_next_word_start"
                | "extend_next_long_word_start"
                | "extend_next_sub_word_start" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::NextWordStart(count),
                    CoreMovement::Extend,
                )),
                "extend_prev_word_start"
                | "extend_prev_long_word_start"
                | "extend_prev_sub_word_start" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::PrevWordStart(count),
                    CoreMovement::Extend,
                )),
                "extend_next_word_end"
                | "extend_next_long_word_end"
                | "extend_next_sub_word_end" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::NextWordEnd(count),
                    CoreMovement::Extend,
                )),
                "extend_prev_word_end"
                | "extend_prev_long_word_end"
                | "extend_prev_sub_word_end" => Some(ExplorerAction::MoveLabelSelection(
                    LabelMotion::PrevWordEnd(count),
                    CoreMovement::Extend,
                )),
                "goto_file_start" | "extend_to_file_start" => Some(ExplorerAction::SelectFirst),
                "goto_file_end"
                | "goto_last_line"
                | "extend_to_file_end"
                | "extend_to_last_line" => Some(ExplorerAction::SelectLast),
                "extend_line_below" | "extend_to_line_bounds" => {
                    Some(ExplorerAction::SelectWholeLabel)
                }
                _ => None,
            },
            CommandToken::Action(id) => match id.as_str() {
                "undo" => Some(ExplorerAction::UndoFileOperation),
                "redo" => Some(ExplorerAction::RedoFileOperation),
                "select_mode" => Some(ExplorerAction::SetMode(Mode::Select)),
                "normal_mode" | "exit_select_mode" => Some(ExplorerAction::SetMode(Mode::Normal)),
                // Inline rename / create entry keys (`i` / `a` / `I` / `A` /
                // `o` / `O`) are Frontend intents in Helix's default keymap
                // — they're handled in `action_for_frontend_intent`.
                "paste_after" => Some(ExplorerAction::PasteClipboard(
                    ExplorerPastePlacement::After,
                )),
                "paste_before" => Some(ExplorerAction::PasteClipboard(
                    ExplorerPastePlacement::Before,
                )),
                "extend_line_below" | "extend_to_line_bounds" => {
                    Some(ExplorerAction::SelectWholeLabel)
                }
                "select_all" => Some(ExplorerAction::SelectWholeLabel),
                "collapse_selection" => Some(ExplorerAction::CollapseLabelSelection),
                "flip_selections" => Some(ExplorerAction::FlipLabelSelection),
                "page_up" | "page_cursor_half_up" => Some(ExplorerAction::Page(-signed_count)),
                "page_down" | "page_cursor_half_down" => Some(ExplorerAction::Page(signed_count)),
                "goto_first_diag" => Some(ExplorerAction::SelectFirstDiagnostic),
                "goto_last_diag" => Some(ExplorerAction::SelectLastDiagnostic),
                "goto_next_diag" => Some(ExplorerAction::SelectNextDiagnostic),
                "goto_prev_diag" => Some(ExplorerAction::SelectPreviousDiagnostic),
                _ => None,
            },
            CommandToken::Operator(id) => match id.as_str() {
                "yank" | "yank_joined" => Some(self.operator_action(ExplorerOperator::Yank)),
                "delete_selection" => {
                    Some(self.operator_action(ExplorerOperator::Delete { yank: true }))
                }
                "delete_selection_noyank" => {
                    Some(self.operator_action(ExplorerOperator::Delete { yank: false }))
                }
                "change_selection" => {
                    Some(self.operator_action(ExplorerOperator::Change { yank: true }))
                }
                "change_selection_noyank" => {
                    Some(self.operator_action(ExplorerOperator::Change { yank: false }))
                }
                _ => None,
            },
            CommandToken::TextObject(id) => match id.as_str() {
                "textobject_word" => Some(ExplorerAction::SelectLabelTextObject(
                    LabelTextObject::InsideWord,
                )),
                "textobject_long_word" => Some(ExplorerAction::SelectLabelTextObject(
                    LabelTextObject::InsideLongWord,
                )),
                _ => None,
            },
            CommandToken::CharPending(_) => None,
        }
    }

    fn action_for_char_pending(&self, id: CharPendingId, ch: char) -> Option<ExplorerAction> {
        let count = self.count.map(NonZeroUsize::get).unwrap_or(1);
        let mode_movement = if self.mode == Mode::Select {
            CoreMovement::Extend
        } else {
            CoreMovement::Move
        };
        let find = |direction, inclusive, movement| {
            ExplorerAction::MoveLabelSelection(
                LabelMotion::FindChar {
                    ch,
                    direction,
                    inclusive,
                    count,
                },
                movement,
            )
        };
        match id.as_str() {
            "find_next_char" => Some(find(Direction::Forward, true, mode_movement)),
            "find_till_char" => Some(find(Direction::Forward, false, mode_movement)),
            "find_prev_char" => Some(find(Direction::Backward, true, mode_movement)),
            "till_prev_char" => Some(find(Direction::Backward, false, mode_movement)),
            "extend_next_char" => Some(find(Direction::Forward, true, CoreMovement::Extend)),
            "extend_till_char" => Some(find(Direction::Forward, false, CoreMovement::Extend)),
            "extend_prev_char" => Some(find(Direction::Backward, true, CoreMovement::Extend)),
            "extend_till_prev_char" => Some(find(Direction::Backward, false, CoreMovement::Extend)),
            "select_textobject_inside_surrounding_pair" if is_label_pair_char(ch) => Some(
                ExplorerAction::SelectLabelTextObject(LabelTextObject::InsideSurroundingPair(ch)),
            ),
            "select_textobject_around_surrounding_pair" if is_label_pair_char(ch) => Some(
                ExplorerAction::SelectLabelTextObject(LabelTextObject::AroundSurroundingPair(ch)),
            ),
            "select_textobject_inside_prev_pair" if is_label_pair_char(ch) => Some(
                ExplorerAction::SelectLabelTextObject(LabelTextObject::InsidePreviousPair(ch)),
            ),
            "select_textobject_inside_next_pair" if is_label_pair_char(ch) => Some(
                ExplorerAction::SelectLabelTextObject(LabelTextObject::InsideNextPair(ch)),
            ),
            _ => None,
        }
    }

    fn operator_action(&self, operator: ExplorerOperator) -> ExplorerAction {
        if self.editing_engine == EditingEngineConfig::Vim && self.mode == Mode::Normal {
            ExplorerAction::BeginOperator(operator)
        } else {
            operator.selection_action()
        }
    }

    fn action_for_frontend_intent(&self, id: FrontendIntentId) -> Option<ExplorerAction> {
        match id.as_str() {
            "goto_first_diag" => Some(ExplorerAction::SelectFirstDiagnostic),
            "goto_last_diag" => Some(ExplorerAction::SelectLastDiagnostic),
            "goto_next_diag" => Some(ExplorerAction::SelectNextDiagnostic),
            "goto_prev_diag" => Some(ExplorerAction::SelectPreviousDiagnostic),
            "command_mode" => Some(ExplorerAction::DelegateToEditor),
            // Inline rename entry — Helix's Frontend commands (defined in
            // `commands.rs` `frontend_command_specs![]`) map straight onto
            // the shared `InsertEntry` enum. The region's
            // `enter_insert_at` then performs the same selection transform
            // the editor uses for the corresponding key in a normal buffer.
            "insert_mode" => Some(ExplorerAction::EnterLabelEdit(
                helix_view::edit_region::InsertEntry::AtCurrent,
            )),
            "append_mode" => Some(ExplorerAction::EnterLabelEdit(
                helix_view::edit_region::InsertEntry::Append,
            )),
            "insert_at_line_start" => Some(ExplorerAction::EnterLabelEdit(
                helix_view::edit_region::InsertEntry::AtLineStart,
            )),
            "insert_at_line_end" => Some(ExplorerAction::EnterLabelEdit(
                helix_view::edit_region::InsertEntry::AtLineEnd,
            )),
            "open_below" => Some(ExplorerAction::EnterCreate),
            "open_above" => Some(ExplorerAction::EnterCreate),
            // `gw` in the editor opens a two-letter label jump over
            // visible words. In the file explorer the analog is "jump
            // to any visible row by typing its label" — same algorithm,
            // different rendering surface. We route the Frontend intent
            // straight to a session-start action; the dispatch handler
            // builds the session and the panel intercepts the next
            // keystrokes through it.
            "goto_word" => Some(ExplorerAction::StartJumpSession),
            "select_textobject_inside_word" => Some(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::InsideWord,
            )),
            "select_textobject_around_word" => Some(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::AroundWord,
            )),
            "select_textobject_inside_WORD" => Some(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::InsideLongWord,
            )),
            "select_textobject_around_WORD" => Some(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::AroundLongWord,
            )),
            "select_textobject_inside_paragraph" => Some(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::InsideParagraph,
            )),
            "select_textobject_around_paragraph" => Some(ExplorerAction::SelectLabelTextObject(
                LabelTextObject::AroundParagraph,
            )),
            "select_textobject_inside_closest_surrounding_pair" => Some(
                ExplorerAction::SelectLabelTextObject(LabelTextObject::InsideClosestPair),
            ),
            "select_textobject_around_closest_surrounding_pair" => Some(
                ExplorerAction::SelectLabelTextObject(LabelTextObject::AroundClosestPair),
            ),
            "select_all" => Some(ExplorerAction::SelectWholeLabel),
            "collapse_selection" => Some(ExplorerAction::CollapseLabelSelection),
            "flip_selections" => Some(ExplorerAction::FlipLabelSelection),
            _ => None,
        }
    }

    fn push_count_digit(&mut self, digit: usize) {
        let count = self
            .count
            .map(NonZeroUsize::get)
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(digit)
            .max(1);
        self.count = NonZeroUsize::new(count);
    }
}

fn is_register_prefix(key: KeyEvent) -> bool {
    key.modifiers.is_empty() && key.char() == Some('"')
}

fn register_key(key: KeyEvent) -> Option<char> {
    key.modifiers.is_empty().then_some(())?;
    key.char().filter(|register| !register.is_control())
}

fn count_digit(key: KeyEvent, existing: Option<NonZeroUsize>) -> Option<usize> {
    if !key.modifiers.is_empty() {
        return None;
    }
    let digit = key.char()?.to_digit(10)? as usize;
    if digit == 0 && existing.is_none() {
        return None;
    }
    Some(digit)
}

const fn is_label_pair_char(ch: char) -> bool {
    matches!(
        ch,
        '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | '`'
    )
}

fn explorer_keymap_infobox(node: &ModalIntentTrieNode) -> Info {
    let mut body = Vec::with_capacity(node.len());
    collect_explorer_keymap("", node, &mut body);
    Info::new(node.name.clone(), &body)
}

fn collect_explorer_keymap(
    prefix: &str,
    node: &ModalIntentTrieNode,
    body: &mut Vec<(String, String)>,
) {
    for key in &node.order {
        let Some(trie) = node.map.get(key) else {
            continue;
        };
        let key = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix} {key}")
        };
        match trie {
            ModalIntentTrie::Binding(binding) => {
                body.push((key, binding.doc().to_string()));
            }
            ModalIntentTrie::Sequence(_) => {
                body.push((key, "[Multiple commands]".to_string()));
            }
            ModalIntentTrie::Node(child) => collect_explorer_keymap(&key, child, body),
        }
    }
    if let Some(fallback) = node.fallback {
        let key = if prefix.is_empty() {
            String::from("...")
        } else {
            format!("{prefix} ...")
        };
        body.push((key, fallback.doc().to_string()));
    }
}
