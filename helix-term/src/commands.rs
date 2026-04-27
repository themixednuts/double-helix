pub(crate) mod dap;
pub(crate) mod lsp;
pub(crate) mod notification;
pub(crate) mod syntax;
pub(crate) mod typed;

pub use dap::*;
use helix_stdx::{
    path::{self, find_paths},
    rope::{self, RopeSliceExt},
};
use helix_vcs::{FileChange, Hunk};
use helix_view::vcs_state::LineBlameError;
pub use lsp::*;
pub use notification::*;
pub use syntax::*;
use tui::{
    text::{Span, Spans},
    widgets::Cell,
};
pub use typed::*;

use helix_core::{
    chars::char_is_word,
    command_line::{self, Args},
    comment,
    doc_formatter::TextFormat,
    encoding, find_workspace,
    graphemes::{self, next_grapheme_boundary, prev_grapheme_boundary},
    history::UndoKind,
    indent::IndentStyle,
    line_ending::{get_line_ending_of_str, line_end_char_index},
    match_brackets,
    movement::{self, move_vertically, Direction},
    pos_at_coords,
    regex::{self, Regex},
    selection,
    syntax::config::LanguageServerFeature,
    text_annotations::Overlay,
    text_folding::{self, RopeSliceFoldExt},
    textobject, LineEnding, Range, Rope, RopeReader, RopeSlice, Selection, SmallVec, Tendril,
    Transaction,
};
pub use helix_view::engine::CharPendingBinding;
use helix_view::{
    document::{DocumentFormatTask, FormatterError, Mode, SCRATCH_BUFFER_NAME},
    editor::Action,
    engine::CommandToken,
    expansion,
    icons::ICONS,
    info::Info,
    input::KeyEvent,
    keyboard::KeyCode,
    theme::Style,
    tree::{self, Dimension, Resize},
    view::View,
    Document, DocumentId, Editor, ViewId,
};

use anyhow::{anyhow, bail, ensure, Context as _};
use helix_modal::registry::{CommandScope, CommandSpec, EngineCommandSpec};
use insert::*;
use movement::Movement;

use crate::{
    compositor::{self, Component, Compositor},
    filter_picker_entry,
    runtime::ingress::{RuntimeEvent, RuntimeTaskEvent},
    runtime::{send_task_event_with, AssistantCommand, ExitTaskSet, UiCommand},
    ui::{self, overlay::overlaid, Picker, PickerColumn, Prompt, PromptEvent},
};
use helix_runtime::Sender as IngressSender;
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    future::Future,
    io::Read,
    num::NonZeroUsize,
    sync::OnceLock,
};

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use once_cell::sync::Lazy;
use serde::de::{self, Deserialize, Deserializer};
use url::Url;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{sinks, BinaryDetection, SearcherBuilder};
use ignore::{DirEntry, WalkBuilder, WalkState};

use helix_plugin::PluginManager;

pub type OnKeyCallback = Box<dyn FnOnce(&mut Context, KeyEvent) + Send>;
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum OnKeyCallbackKind {
    PseudoPending,
    Fallback,
}

pub struct Context<'a> {
    pub register: Option<char>,
    pub count: Option<NonZeroUsize>,
    pub editor: &'a mut Editor,
    pub registry: std::sync::Arc<helix_modal::registry::CommandRegistry>,
    pub notifier: crate::handlers::local::Notifier,

    pub callback: Vec<crate::compositor::PostAction>,
    pub on_next_key_callback: Option<(OnKeyCallback, OnKeyCallbackKind)>,
    /// Exit-bound task sink for commands that must complete typed task work before shutdown.
    pub exit_tasks: &'a mut ExitTaskSet,
    pub exit_task_work: helix_runtime::Work,
    /// Mirrors [`compositor::Context::ingress`] when built from the live app (Phase 3).
    pub ingress: IngressSender<RuntimeEvent>,
    pub idle_reset_tx: helix_runtime::Sender<()>,
    pub plugin_manager: Option<std::sync::Arc<PluginManager>>,
}

impl Context<'_> {
    /// Push a new component onto the compositor.
    pub fn push_layer(&mut self, component: Box<dyn Component>) {
        self.callback
            .push(crate::compositor::PostAction::PushLayer(component));
    }

    /// Call `replace_or_push` on the Compositor
    pub fn replace_or_push_layer<T: Component>(&mut self, id: &'static str, component: T) {
        self.callback
            .push(crate::compositor::PostAction::ReplaceOrPushLayer {
                id,
                layer: Box::new(component),
            });
    }

    #[inline]
    pub fn on_next_key(
        &mut self,
        on_next_key_callback: impl FnOnce(&mut Context, KeyEvent) + Send + 'static,
    ) {
        self.on_next_key_callback = Some((
            Box::new(on_next_key_callback),
            OnKeyCallbackKind::PseudoPending,
        ));
    }

    #[inline]
    pub fn on_next_key_fallback(
        &mut self,
        on_next_key_callback: impl FnOnce(&mut Context, KeyEvent) + Send + 'static,
    ) {
        self.on_next_key_callback =
            Some((Box::new(on_next_key_callback), OnKeyCallbackKind::Fallback));
    }

    #[inline]
    pub fn spawn_ui(
        &mut self,
        future: impl Future<Output = anyhow::Result<UiCommand>> + Send + 'static,
    ) {
        crate::runtime::ingress::spawn_ui_command_with_future(
            self.editor.work(),
            future,
            self.ingress.clone(),
        );
    }

    #[inline]
    pub fn spawn_task_event(
        &mut self,
        future: impl Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
    ) {
        crate::runtime::ingress::spawn_task_event_with_future(
            self.editor.work(),
            future,
            self.ingress.clone(),
        );
    }

    pub fn reset_idle_timer(&self) {
        helix_runtime::send_blocking(&self.idle_reset_tx, ());
    }

    #[inline]
    pub fn exit_task_event(
        &mut self,
        future: impl Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
    ) {
        crate::runtime::schedule_exit_task(self.exit_tasks, &self.exit_task_work, future);
    }

    /// Returns 1 if no explicit count was provided
    #[inline]
    pub fn count(&self) -> usize {
        self.count.map_or(1, |v| v.get())
    }

    /// Execute an engine command through the registry.
    fn execute_engine_command(&mut self, token: CommandToken) {
        use helix_modal::registry::CommandRef;

        let count = self.count();
        let register = self.register.take();

        // Resolve editing context from the focused view.
        let focus = self.editor.focused_view_id();
        let focused_view = self.editor.tree.get(focus);
        let view_id = focused_view.id;
        let doc_id = focused_view.doc;

        let Some(kind) = self.registry.resolve(token) else {
            log::warn!("engine command missing from registry: {}", token.as_str());
            return;
        };

        match kind {
            CommandRef::Motion(m) => {
                let movement = if self.editor.mode() == Mode::Select {
                    Movement::Extend
                } else {
                    Movement::Move
                };
                let motion = m
                    .make
                    .make(Some(NonZeroUsize::new(count).unwrap_or(NonZeroUsize::MIN)));
                motion(self.editor, view_id, doc_id, movement);
            }
            CommandRef::Operator(op) => {
                (op.execute)(self.editor, view_id, doc_id, register);
            }
            CommandRef::Action(a) => {
                (a.execute)(self.editor, view_id, doc_id, count, register);
            }
            CommandRef::TextObject(to) => {
                let obj_fn = (to.make)(count);
                obj_fn(
                    self.editor,
                    view_id,
                    doc_id,
                    helix_core::textobject::TextObject::Around,
                );
            }
            CommandRef::CharPending(cp) => {
                let resolve = cp.resolve;
                let movement = if self.editor.mode() == Mode::Select {
                    Movement::Extend
                } else {
                    Movement::Move
                };
                self.on_next_key(move |cx, event| {
                    if let Some(ch) = event.char() {
                        let motion = resolve(ch, count);
                        cx.editor
                            .apply_motion(move |ed| motion(ed, view_id, doc_id, movement));
                    }
                });
            }
        }
    }

    /// Waits on all pending async UI work, then tries to flush all pending write
    /// operations for all documents.
    pub fn block_try_flush_writes(&mut self) -> anyhow::Result<()> {
        compositor::Context {
            editor: self.editor,
            exit_tasks: self.exit_tasks,
            exit_task_work: self.exit_task_work.clone(),
            scroll: None,
            notifier: self.notifier.clone(),
            ingress: self.ingress.clone(),
            idle_reset_tx: self.idle_reset_tx.clone(),
            plugin_manager: self.plugin_manager.clone(),
        }
        .block_try_flush_writes()
    }
}

use helix_view::{align_view, Align};

type FrontendCommandSpec = CommandSpec<fn(&mut Context)>;

macro_rules! frontend_command_specs {
    [ $( $name:ident => $doc:literal ),* $(,)? ] => {
        pub const FRONTEND_COMMAND_SPECS: &'static [FrontendCommandSpec] = &[
            $(
                FrontendCommandSpec::new(
                    stringify!($name),
                    $name,
                    $doc,
                    CommandScope::Frontend,
                ),
            )*
        ];
    };
}

/// MappableCommands are commands that can be bound to keys, executable in
/// normal, insert or select mode.
///
/// There are three kinds:
///
/// * Static: commands usually bound to keys and used for editing, movement,
///   etc., for example `move_char_left`.
/// * Typable: commands executable from command mode, prefixed with a `:`,
///   for example `:write!`.
/// * Macro: a sequence of keys to execute, for example `@miw`.
#[derive(Clone)]
pub enum MappableCommand {
    Typable {
        name: String,
        args: String,
        doc: String,
    },
    /// Command whose logic lives in CommandRegistry — pure Editor mutation.
    /// `execute()` delegates to the registry via Context; no function pointer needed.
    Engine {
        spec: &'static EngineCommandSpec,
    },
    /// Command that requires frontend Context (pickers, LSP, compositor, on_next_key).
    Frontend {
        spec: &'static FrontendCommandSpec,
    },
    Macro {
        name: String,
        keys: Vec<KeyEvent>,
    },
}

impl MappableCommand {
    pub fn execute(&self, cx: &mut Context) {
        match &self {
            Self::Typable { name, args, doc: _ } => {
                if let Some(command) = typed::TYPABLE_COMMAND_MAP.get(name.as_str()) {
                    let mut cx = compositor::Context {
                        editor: cx.editor,
                        exit_tasks: cx.exit_tasks,
                        exit_task_work: cx.exit_task_work.clone(),
                        scroll: None,
                        notifier: cx.notifier.clone(),
                        ingress: cx.ingress.clone(),
                        idle_reset_tx: cx.idle_reset_tx.clone(),
                        plugin_manager: cx.plugin_manager.clone(),
                    };
                    if let Err(e) =
                        typed::execute_command(&mut cx, command, args, PromptEvent::Validate)
                    {
                        cx.editor.set_error(format!("{}", e));
                    }
                } else if let Some(plugin_manager) = &cx.plugin_manager {
                    let args: Vec<String> =
                        args.split_whitespace().map(|s| s.to_string()).collect();
                    if let Err(e) = plugin_manager.execute_command(cx.editor, name, args) {
                        cx.editor.set_error(format!("{}", e));
                    }
                } else {
                    cx.editor.set_error(format!("no such command: '{name}'"));
                }
            }
            Self::Engine { spec } => {
                cx.execute_engine_command(spec.token());
            }
            Self::Frontend { spec } => (spec.payload())(cx),
            Self::Macro { keys, .. } => {
                // Protect against recursive macros.
                if cx.editor.macro_replaying.contains(&'@') {
                    cx.editor.set_error(
                        "Cannot execute macro because the [@] register is already playing a macro",
                    );
                    return;
                }
                cx.editor.macro_replaying.push('@');
                let keys = keys.clone();
                cx.callback.push(crate::compositor::PostAction::ReplayKeys {
                    keys,
                    count: 1,
                    pop_macro_replaying: true,
                });
            }
        }
    }

    pub fn name(&self) -> &str {
        match &self {
            Self::Typable { name, .. } => name,
            Self::Engine { spec } => spec.name(),
            Self::Frontend { spec } => spec.name(),
            Self::Macro { name, .. } => name,
        }
    }

    /// Returns the name as `&'static str` for Engine and Frontend variants,
    /// `None` for Typable and Macro (which have owned names).
    pub fn static_name(&self) -> Option<&'static str> {
        match self {
            Self::Engine { spec } => Some(spec.name()),
            Self::Frontend { spec } => Some(spec.name()),
            Self::Typable { .. } | Self::Macro { .. } => None,
        }
    }

    pub fn doc(&self) -> &str {
        match &self {
            Self::Typable { doc, .. } => doc,
            Self::Engine { spec } => spec.doc(),
            Self::Frontend { spec } => spec.doc(),
            Self::Macro { name, .. } => name,
        }
    }

    pub fn modal_command(&self) -> Option<CommandToken> {
        match self {
            Self::Engine { spec } => Some(spec.token()),
            Self::Frontend { .. } | Self::Typable { .. } | Self::Macro { .. } => None,
        }
    }

    pub fn scope(&self) -> CommandScope {
        match self {
            Self::Engine { spec } => spec.scope(),
            Self::Frontend { spec } => spec.scope(),
            Self::Typable { .. } | Self::Macro { .. } => CommandScope::Frontend,
        }
    }

    pub fn named(name: &str) -> Option<Self> {
        helix_modal::populate::engine_command_specs()
            .iter()
            .find(|spec| spec.name() == name)
            .map(|spec| Self::Engine { spec })
            .or_else(|| {
                Self::FRONTEND_COMMAND_SPECS
                    .iter()
                    .find(|spec| spec.name() == name)
                    .map(|spec| Self::Frontend { spec })
            })
    }

    pub fn builtin_named(name: &'static str) -> Self {
        Self::named(name).unwrap_or_else(|| panic!("builtin command `{name}` must exist"))
    }

    pub fn builtin_commands() -> &'static [Self] {
        static STATIC_COMMANDS: OnceLock<Box<[MappableCommand]>> = OnceLock::new();
        STATIC_COMMANDS.get_or_init(|| {
            let mut commands = Vec::with_capacity(
                Self::FRONTEND_COMMAND_SPECS.len()
                    + helix_modal::populate::engine_command_specs().len(),
            );
            commands.extend(
                Self::FRONTEND_COMMAND_SPECS
                    .iter()
                    .map(|spec| Self::Frontend { spec }),
            );
            commands.extend(
                helix_modal::populate::engine_command_specs()
                    .iter()
                    .map(|spec| Self::Engine { spec }),
            );
            commands.into_boxed_slice()
        })
    }

    /// Whether this command can execute against a component-owned `EditRegion`
    /// without requiring a tree-backed `View`.
    pub fn supports_component_region(&self) -> bool {
        self.scope() == CommandScope::Viewport
    }

    #[rustfmt::skip]
    frontend_command_specs![
        no_op => "Do nothing",
        repeat_last_motion => "Repeat last motion",
        replace => "Replace with new char",
        half_page_up => "Move half page up",
        half_page_down => "Move half page down",
        page_cursor_up => "Move page and cursor up",
        page_cursor_down => "Move page and cursor down",
        select_regex => "Select all regex matches inside selections",
        split_selection => "Split selections on regex matches",
        split_selection_on_newline => "Split selection on newlines",
        merge_selections => "Merge selections",
        merge_consecutive_selections => "Merge consecutive selections",
        search => "Search for regex pattern",
        rsearch => "Reverse search for regex pattern",
        search_next => "Select next search match",
        search_prev => "Select previous search match",
        extend_search_next => "Add next search match to selection",
        extend_search_prev => "Add previous search match to selection",
        search_selection => "Use current selection as search pattern",
        search_selection_detect_word_boundaries => "Use current selection as the search pattern, automatically wrapping with `\\b` on word boundaries",
        make_search_word_bounded => "Modify current search to make it word bounded",
        global_search => "Global search in workspace folder",
        local_search_grep => "Local search in buffer",
        local_search_fuzzy => "Fuzzy local search in buffer",
        insert_mode => "Insert before selection",
        append_mode => "Append after selection",
        command_mode => "Enter command mode",
        file_picker => "Open file picker",
        file_picker_in_current_buffer_directory => "Open file picker at current buffer's directory",
        file_picker_in_current_directory => "Open file picker at current working directory",
        file_explorer => "Open file explorer in workspace root",
        file_explorer_in_current_buffer_directory => "Open file explorer at current buffer's directory",
        file_explorer_in_current_directory => "Open file explorer at current working directory",
        code_action => "Perform code action",
        code_action_picker => "Perform code action in a picker",
        buffer_picker => "Open buffer picker",
        jumplist_picker => "Open jumplist picker",
        symbol_picker => "Open symbol picker",
        syntax_symbol_picker => "Open symbol picker from syntax information",
        lsp_or_syntax_symbol_picker => "Open symbol picker from LSP or syntax information",
        changed_file_picker => "Open changed file picker",
        select_references_to_symbol_under_cursor => "Select symbol references",
        workspace_symbol_picker => "Open workspace symbol picker",
        syntax_workspace_symbol_picker => "Open workspace symbol picker from syntax information",
        lsp_or_syntax_workspace_symbol_picker => "Open workspace symbol picker from LSP or syntax information",
        diagnostics_picker => "Open diagnostic picker",
        workspace_diagnostics_picker => "Open workspace diagnostic picker",
        last_picker => "Open last picker",
        insert_at_line_start => "Insert at start of line",
        insert_at_line_end => "Insert at end of line",
        open_below => "Open new line below selection",
        open_above => "Open new line above selection",
        normal_mode => "Enter normal mode",
        goto_definition => "Goto definition",
        goto_declaration => "Goto declaration",
        goto_type_definition => "Goto type definition",
        goto_implementation => "Goto implementation",
        goto_file => "Goto files/URLs in selections",
        goto_file_hsplit => "Goto files in selections (hsplit)",
        goto_file_vsplit => "Goto files in selections (vsplit)",
        goto_reference => "Goto references",
        goto_window_top => "Goto window top",
        goto_window_center => "Goto window center",
        goto_window_bottom => "Goto window bottom",
        goto_last_accessed_file => "Goto last accessed file",
        goto_last_modified_file => "Goto last modified file",
        goto_last_modification => "Goto last modification",
        goto_first_diag => "Goto first diagnostic",
        goto_last_diag => "Goto last diagnostic",
        goto_next_diag => "Goto next diagnostic",
        goto_prev_diag => "Goto previous diagnostic",
        goto_next_change => "Goto next change",
        goto_prev_change => "Goto previous change",
        goto_first_change => "Goto first change",
        goto_last_change => "Goto last change",
        grow_buffer_width => "Grow focused container width",
        shrink_buffer_width => "Shrink focused container width",
        grow_buffer_height => "Grow focused container height",
        shrink_buffer_height => "Shrink focused container height",
        toggle_focus_window => "Toggle focus mode on buffer",
        assistant_panel => "Toggle assistant panel",
        assistant_close_panel => "Close assistant panel",
        assistant_focus_input => "Focus assistant panel and activate input",
        assistant_focus_entries => "Focus assistant panel entry list",
        assistant_open_entry_scratch => "Open selected assistant entry details in current view",
        assistant_cycle_thinking => "Cycle assistant thinking options",
        assistant_cycle_model => "Cycle assistant model options",
        assistant_cycle_mode => "Cycle assistant mode options",
        assistant_toggle_follow => "Toggle following for the active assistant thread",
        goto_next_buffer => "Goto next buffer",
        goto_previous_buffer => "Goto previous buffer",
        signature_help => "Show signature help",
        smart_tab => "Insert tab if all cursors have all whitespace to their left; otherwise, run a separate command.",
        insert_char_interactive => "Insert an interactively-chosen char",
        append_char_interactive => "Append an interactively-chosen char",
        yank_to_clipboard => "Yank selections to clipboard",
        yank_to_primary_clipboard => "Yank selections to primary clipboard",
        yank_joined_to_clipboard => "Join and yank selections to clipboard",
        yank_main_selection_to_clipboard => "Yank main selection to clipboard",
        yank_joined_to_primary_clipboard => "Join and yank selections to primary clipboard",
        yank_main_selection_to_primary_clipboard => "Yank main selection to primary clipboard",
        replace_selections_with_clipboard => "Replace selections by clipboard content",
        replace_selections_with_primary_clipboard => "Replace selections by primary clipboard",
        format_selections => "Format selection",
        keep_selections => "Keep selections matching regex",
        remove_selections => "Remove selections matching regex",
        completion => "Invoke completion popup",
        hover => "Show docs for item under cursor",
        goto_hover => "Show docs for item under cursor in a new buffer",
        jump_view_right => "Jump to right split",
        jump_view_left => "Jump to left split",
        jump_view_up => "Jump to split above",
        jump_view_down => "Jump to split below",
        swap_view_right => "Swap with right split",
        swap_view_left => "Swap with left split",
        swap_view_up => "Swap with split above",
        swap_view_down => "Swap with split below",
        transpose_view => "Transpose splits",
        rotate_view => "Goto next window",
        rotate_view_reverse => "Goto previous window",
        hsplit => "Horizontal bottom split",
        hsplit_new => "Horizontal bottom split scratch buffer",
        vsplit => "Vertical right split",
        vsplit_new => "Vertical right split scratch buffer",
        wclose => "Close window",
        wonly => "Close windows except current",
        select_register => "Select register",
        insert_register => "Insert register",
        copy_between_registers => "Copy between two registers",
        surround_add => "Surround add",
        surround_replace => "Surround replace",
        surround_delete => "Surround delete",
        select_textobject_inside_type => "Select inside type definition (tree-sitter)",
        select_textobject_around_type => "Select around type definition (tree-sitter)",
        select_textobject_inside_function => "Select inside function (tree-sitter)",
        select_textobject_around_function => "Select around function (tree-sitter)",
        select_textobject_inside_parameter => "Select inside argument/parameter (tree-sitter)",
        select_textobject_around_parameter => "Select around argument/parameter (tree-sitter)",
        select_textobject_inside_comment => "Select inside comment (tree-sitter)",
        select_textobject_around_comment => "Select around comment (tree-sitter)",
        select_textobject_inside_test => "Select inside test (tree-sitter)",
        select_textobject_around_test => "Select around test (tree-sitter)",
        select_textobject_inside_entry => "Select inside data structure entry (tree-sitter)",
        select_textobject_around_entry => "Select around data structure entry (tree-sitter)",
        select_textobject_inside_paragraph => "Select inside paragraph",
        select_textobject_around_paragraph => "Select around paragraph",
        select_textobject_inside_closest_surrounding_pair => "Select inside closest surrounding pair (tree-sitter)",
        select_textobject_around_closest_surrounding_pair => "Select around closest surrounding pair (tree-sitter)",
        select_textobject_inside_word => "Select inside word",
        select_textobject_around_word => "Select around word",
        select_textobject_inside_WORD => "Select inside WORD",
        select_textobject_around_WORD => "Select around WORD",
        select_textobject_inside_change => "Select inside VCS change",
        select_textobject_around_change => "Select around VCS change",
        goto_next_xml_element => "Goto next (X)HTML element",
        goto_prev_xml_element => "Goto previous (X)HTML element",
        dap_launch => "Launch debug target",
        dap_restart => "Restart debugging session",
        dap_toggle_breakpoint => "Toggle breakpoint",
        dap_continue => "Continue program execution",
        dap_pause => "Pause program execution",
        dap_step_in => "Step in",
        dap_step_out => "Step out",
        dap_next => "Step to next",
        dap_variables => "List variables",
        dap_terminate => "End debug session",
        dap_edit_condition => "Edit breakpoint condition on current line",
        dap_edit_log => "Edit breakpoint log message on current line",
        dap_switch_thread => "Switch current thread",
        dap_switch_stack_frame => "Switch stack frame",
        dap_enable_exceptions => "Enable exception breakpoints",
        dap_disable_exceptions => "Disable exception breakpoints",
        shell_pipe => "Pipe selections through shell command",
        shell_pipe_to => "Pipe selections into shell command ignoring output",
        shell_insert_output => "Insert shell command output before selections",
        shell_append_output => "Append shell command output after selections",
        shell_keep_pipe => "Filter selections with shell predicate",
        suspend => "Suspend and return to shell",
        rename_symbol => "Rename symbol",
        record_macro => "Record macro",
        replay_macro => "Replay macro",
        command_palette => "Open command palette",
        goto_word => "Jump to a two-character label",
        extend_to_word => "Extend to a two-character label",
        goto_next_tabstop => "Goto next snippet placeholder",
        goto_prev_tabstop => "Goto next snippet placeholder",
        blame_line => "Show blame for the current line",
        fold => "Fold text objects",
        unfold => "Unfold text objects",
        toggle_fold => "Toggle fold for the text object at the primary cursor",
    ];
}

impl fmt::Debug for MappableCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MappableCommand::Engine { spec } => f
                .debug_tuple("MappableCommand")
                .field(&spec.name())
                .finish(),
            MappableCommand::Frontend { spec } => f
                .debug_tuple("MappableCommand")
                .field(&spec.name())
                .finish(),
            MappableCommand::Typable { name, args, .. } => f
                .debug_tuple("MappableCommand")
                .field(name)
                .field(args)
                .finish(),
            MappableCommand::Macro { name, keys, .. } => f
                .debug_tuple("MappableCommand")
                .field(name)
                .field(keys)
                .finish(),
        }
    }
}

impl fmt::Display for MappableCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for MappableCommand {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(suffix) = s.strip_prefix(':') {
            let (name, args, _) = command_line::split(suffix);
            ensure!(!name.is_empty(), "Expected typable command name");
            let cmd = typed::TYPABLE_COMMAND_MAP.get(name);

            let doc = if let Some(cmd) = cmd {
                if args.is_empty() {
                    cmd.doc.to_string()
                } else {
                    format!(":{} {:?}", cmd.name, args)
                }
            } else {
                format!(":{} {:?}", name, args)
            };

            Ok(MappableCommand::Typable {
                name: name.to_owned(),
                doc,
                args: args.to_string(),
            })
        } else if let Some(suffix) = s.strip_prefix('@') {
            helix_view::input::parse_macro(suffix).map(|keys| Self::Macro {
                name: s.to_string(),
                keys,
            })
        } else {
            MappableCommand::named(s).ok_or_else(|| anyhow!("No command named '{}'", s))
        }
    }
}

impl<'de> Deserialize<'de> for MappableCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

impl PartialEq for MappableCommand {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                MappableCommand::Typable {
                    name: first_name,
                    args: first_args,
                    ..
                },
                MappableCommand::Typable {
                    name: second_name,
                    args: second_args,
                    ..
                },
            ) => first_name == second_name && first_args == second_args,
            (
                MappableCommand::Engine { spec: first_spec },
                MappableCommand::Engine { spec: second_spec },
            ) => first_spec.name() == second_spec.name(),
            (
                MappableCommand::Frontend { spec: first_spec },
                MappableCommand::Frontend { spec: second_spec },
            ) => first_spec.name() == second_spec.name(),
            _ => false,
        }
    }
}

fn no_op(_cx: &mut Context) {}

fn grow_buffer_width(cx: &mut Context) {
    cx.editor.resize_buffer(Resize::Grow, Dimension::Width);
}

fn shrink_buffer_width(cx: &mut Context) {
    cx.editor.resize_buffer(Resize::Shrink, Dimension::Width);
}
fn grow_buffer_height(cx: &mut Context) {
    cx.editor.resize_buffer(Resize::Grow, Dimension::Height);
}

fn shrink_buffer_height(cx: &mut Context) {
    cx.editor.resize_buffer(Resize::Shrink, Dimension::Height);
}

fn toggle_focus_window(cx: &mut Context) {
    cx.editor.toggle_focus_window();
}

fn assistant_panel(cx: &mut Context) {
    cx.spawn_ui(async { Ok(UiCommand::Assistant(AssistantCommand::TogglePanelFocus)) });
}

fn assistant_close_panel(cx: &mut Context) {
    cx.spawn_ui(async { Ok(UiCommand::Assistant(AssistantCommand::ClosePanel)) });
}

fn assistant_focus_input(cx: &mut Context) {
    cx.spawn_ui(async { Ok(UiCommand::Assistant(AssistantCommand::FocusPanelInput)) });
}

fn assistant_focus_entries(cx: &mut Context) {
    cx.spawn_ui(async { Ok(UiCommand::Assistant(AssistantCommand::FocusPanelEntries)) });
}

fn assistant_open_entry_scratch(cx: &mut Context) {
    let Some(effects) = cx
        .editor
        .open_selected_assistant_entry_scratch(Action::Replace)
    else {
        cx.editor.set_status("No assistant entry selected");
        return;
    };
    cx.editor.apply_assistant_effects(effects);
}

fn assistant_cycle_thinking(cx: &mut Context) {
    match cx.editor.cycle_active_assistant_config("thinking") {
        Ok(effects) => {
            cx.editor.apply_assistant_effects(effects);
            cx.editor.set_status("Cycled thinking")
        }
        Err(err) => cx.editor.set_status(err.to_string()),
    }
}

fn assistant_cycle_model(cx: &mut Context) {
    match cx.editor.cycle_active_assistant_config("model") {
        Ok(effects) => {
            cx.editor.apply_assistant_effects(effects);
            cx.editor.set_status("Cycled model")
        }
        Err(err) => cx.editor.set_status(err.to_string()),
    }
}

fn assistant_cycle_mode(cx: &mut Context) {
    match cx.editor.cycle_active_assistant_mode() {
        Ok(effects) => {
            cx.editor.apply_assistant_effects(effects);
            cx.editor.set_status("Cycled mode")
        }
        Err(err) => cx.editor.set_status(err.to_string()),
    }
}

fn assistant_toggle_follow(cx: &mut Context) {
    match cx.editor.toggle_active_assistant_follow() {
        Ok((status, effects)) => {
            cx.editor.apply_assistant_effects(effects);
            cx.editor.set_status(status)
        }
        Err(err) => cx.editor.set_status(err.to_string()),
    }
}

fn goto_next_buffer(cx: &mut Context) {
    goto_buffer(cx.editor, Direction::Forward, cx.count());
}

fn goto_previous_buffer(cx: &mut Context) {
    goto_buffer(cx.editor, Direction::Backward, cx.count());
}

fn goto_buffer(editor: &mut Editor, direction: Direction, count: usize) {
    let current = view!(editor).doc;

    let id = match direction {
        Direction::Forward => {
            let iter = editor.documents.keys();
            // skip 'count' times past current buffer
            iter.cycle().skip_while(|id| *id != &current).nth(count)
        }
        Direction::Backward => {
            let iter = editor.documents.keys();
            // skip 'count' times past current buffer
            iter.rev()
                .cycle()
                .skip_while(|id| *id != &current)
                .nth(count)
        }
    }
    .unwrap();

    let id = *id;

    editor.switch(id, Action::Replace);
}

fn goto_window(cx: &mut Context, align: Align) {
    let count = cx.count() - 1;
    let config = cx.editor.config();
    let (view_id, doc) = focused!(cx.editor);
    let view = view!(cx.editor, view_id);
    let view_offset = doc.view_offset(view_id);

    let height = view.inner_height();

    // respect user given count if any
    // - 1 so we have at least one gap in the middle.
    // a height of 6 with padding of 3 on each side will keep shifting the view back and forth
    // as we type
    let scrolloff = config.scrolloff.min(height.saturating_sub(1) / 2);

    let last_visual_line = view.last_visual_line(doc);

    let visual_line = match align {
        Align::Top => view_offset.vertical_offset + scrolloff + count,
        Align::Center => view_offset.vertical_offset + (last_visual_line / 2),
        Align::Bottom => {
            view_offset.vertical_offset + last_visual_line.saturating_sub(scrolloff + count)
        }
    };
    let visual_line = visual_line
        .max(view_offset.vertical_offset + scrolloff)
        .min(view_offset.vertical_offset + last_visual_line.saturating_sub(scrolloff));

    let pos = view
        .pos_at_visual_coords(doc, visual_line as u16, 0, false)
        .expect("visual_line was constrained to the view area");

    let text = doc.text().slice(..);
    let selection = doc
        .selection(view_id)
        .clone()
        .transform(|range| range.put_cursor(text, pos, cx.editor.mode == Mode::Select));
    doc.set_selection(view_id, selection);
}

fn goto_window_top(cx: &mut Context) {
    goto_window(cx, Align::Top)
}

fn goto_window_center(cx: &mut Context) {
    goto_window(cx, Align::Center)
}

fn goto_window_bottom(cx: &mut Context) {
    goto_window(cx, Align::Bottom)
}

fn goto_file(cx: &mut Context) {
    goto_file_impl(cx, Action::Replace);
}

fn goto_file_hsplit(cx: &mut Context) {
    goto_file_impl(cx, Action::HorizontalSplit);
}

fn goto_file_vsplit(cx: &mut Context) {
    goto_file_impl(cx, Action::VerticalSplit);
}

/// Goto files in selection.
fn goto_file_impl(cx: &mut Context, action: Action) {
    let (view_id, doc) = focused_ref!(cx.editor);
    let text = doc.text().slice(..);
    let selections = doc.selection(view_id);
    let primary = selections.primary();
    let rel_path = doc
        .relative_path()
        .map(|path| path.parent().unwrap().to_path_buf())
        .unwrap_or_default();

    let paths: Vec<_> = if selections.len() == 1 && primary.len() == 1 {
        // Cap the search at roughly 1k bytes around the cursor.
        let lookaround = 1000;
        let pos = text.char_to_byte(primary.cursor(text));
        let search_start = text
            .line_to_byte(text.byte_to_line(pos))
            .max(text.floor_char_boundary(pos.saturating_sub(lookaround)));
        let search_end = text
            .line_to_byte(text.byte_to_line(pos) + 1)
            .min(text.ceil_char_boundary(pos + lookaround));
        let search_range = text.byte_slice(search_start..search_end);
        // we also allow paths that are next to the cursor (can be ambiguous but
        // rarely so in practice) so that gf on quoted/braced path works (not sure about this
        // but apparently that is how gf has worked historically in helix)
        let path = find_paths(search_range, true)
            .take_while(|range| search_start + range.start <= pos + 1)
            .find(|range| pos <= search_start + range.end)
            .map(|range| Cow::from(search_range.byte_slice(range)));
        log::debug!("goto_file auto-detected path: {path:?}");
        let path = path.unwrap_or_else(|| primary.fragment(text));
        vec![path.into_owned()]
    } else {
        // Otherwise use each selection, trimmed.
        selections
            .fragments(text)
            .map(|sel| sel.trim().to_owned())
            .filter(|sel| !sel.is_empty())
            .collect()
    };

    for sel in paths {
        if let Ok(url) = Url::parse(&sel) {
            open_url(cx, url, action);
            continue;
        }

        let path = path::expand(&sel);
        let path = &rel_path.join(path);
        if path.is_dir() {
            let picker = ui::file_picker(cx.editor, path.into(), cx.ingress.clone());
            cx.push_layer(Box::new(overlaid(picker)));
        } else if let Err(e) = cx.editor.open(path, action) {
            cx.editor.set_error(format!("Open file failed: {:?}", e));
        }
    }
}

/// Opens the given url. If the URL points to a valid textual file it is open in helix.
//  Otherwise, the file is open using external program.
fn open_url(cx: &mut Context, url: Url, action: Action) {
    let (_, doc) = focused_ref!(cx.editor);
    let rel_path = doc
        .relative_path()
        .map(|path| path.parent().unwrap().to_path_buf())
        .unwrap_or_default();

    if url.scheme() != "file" {
        cx.spawn_task_event(crate::open_external_url_task_event(url));
        return;
    }

    let content_type = std::fs::File::open(url.path()).and_then(|file| {
        // Read up to 1kb to detect the content type
        let mut read_buffer = Vec::new();
        let n = file.take(1024).read_to_end(&mut read_buffer)?;
        Ok(content_inspector::inspect(&read_buffer[..n]))
    });

    // we attempt to open binary files - files that can't be open in helix - using external
    // program as well, e.g. pdf files or images
    match content_type {
        Ok(content_inspector::ContentType::BINARY) => {
            cx.spawn_task_event(crate::open_external_url_task_event(url))
        }
        Ok(_) | Err(_) => {
            let path = &rel_path.join(url.path());
            if path.is_dir() {
                let picker = ui::file_picker(cx.editor, path.into(), cx.ingress.clone());
                cx.push_layer(Box::new(overlaid(picker)));
            } else if let Err(e) = cx.editor.open(path, action) {
                cx.editor.set_error(format!("Open file failed: {:?}", e));
            }
        }
    }
}

fn repeat_last_motion(cx: &mut Context) {
    cx.editor.repeat_last_motion(cx.count())
}

fn replace(cx: &mut Context) {
    let mut buf = [0u8; 4]; // To hold utf8 encoded char.

    // need to wait for next key
    cx.on_next_key(move |cx, event| {
        let (view_id, doc) = focused!(cx.editor);
        let ch: Option<&str> = match event {
            KeyEvent {
                code: KeyCode::Char(ch),
                ..
            } => Some(ch.encode_utf8(&mut buf[..])),
            KeyEvent {
                code: KeyCode::Enter,
                ..
            } => Some(doc.line_ending().as_str()),
            KeyEvent {
                code: KeyCode::Tab, ..
            } => Some("\t"),
            _ => None,
        };

        let selection = doc.selection(view_id);

        if let Some(ch) = ch {
            let transaction = Transaction::change_by_selection(doc.text(), selection, |range| {
                if !range.is_empty() {
                    let text: Tendril = doc
                        .text()
                        .slice(range.from()..range.to())
                        .graphemes()
                        .map(|_g| ch)
                        .collect();
                    (range.from(), range.to(), Some(text))
                } else {
                    // No change.
                    (range.from(), range.to(), None)
                }
            });

            doc.apply(&transaction, view_id);
            exit_select_mode(cx);
        }
    })
}

pub fn scroll(cx: &mut Context, offset: usize, direction: Direction, sync_cursor: bool) {
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    helix_view::commands::movement::scroll(
        cx.editor,
        view_id,
        doc_id,
        offset,
        direction,
        sync_cursor,
    );
}

fn half_page_up(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height() / 2;
    scroll(cx, offset, Direction::Backward, false);
}

fn half_page_down(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height() / 2;
    scroll(cx, offset, Direction::Forward, false);
}

fn page_cursor_up(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height();
    scroll(cx, offset, Direction::Backward, true);
}

fn page_cursor_down(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height();
    scroll(cx, offset, Direction::Forward, true);
}

fn select_regex(cx: &mut Context) {
    let reg = cx.register.unwrap_or('/');
    ui::regex_prompt(
        cx,
        "select:".into(),
        Some(reg),
        ui::completers::none,
        move |cx, regex, event| {
            let (view_id, doc) = focused!(cx.editor);
            if !matches!(event, PromptEvent::Update | PromptEvent::Validate) {
                return;
            }
            let text = doc.text().slice(..);
            if let Some(selection) =
                selection::select_on_matches(text, doc.selection(view_id), &regex)
            {
                doc.set_selection(view_id, selection);
            } else if event == PromptEvent::Validate {
                cx.editor.set_error("nothing selected");
            }
        },
    );
}

fn split_selection(cx: &mut Context) {
    let reg = cx.register.unwrap_or('/');
    ui::regex_prompt(
        cx,
        "split:".into(),
        Some(reg),
        ui::completers::none,
        move |cx, regex, event| {
            let (view_id, doc) = focused!(cx.editor);
            if !matches!(event, PromptEvent::Update | PromptEvent::Validate) {
                return;
            }
            let text = doc.text().slice(..);
            let selection = selection::split_on_matches(text, doc.selection(view_id), &regex);
            doc.set_selection(view_id, selection);
        },
    );
}

fn split_selection_on_newline(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let text = doc.text().slice(..);
    let selection = selection::split_on_newline(text, doc.selection(view_id));
    doc.set_selection(view_id, selection);
}

fn merge_selections(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let selection = doc.selection(view_id).clone().merge_ranges();
    doc.set_selection(view_id, selection);
}

fn merge_consecutive_selections(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let selection = doc.selection(view_id).clone().merge_consecutive_ranges();
    doc.set_selection(view_id, selection);
}

#[allow(clippy::too_many_arguments)]
fn search_impl(
    editor: &mut Editor,
    regex: &rope::Regex,
    movement: Movement,
    direction: Direction,
    scrolloff: usize,
    wrap_around: bool,
    show_warnings: bool,
) {
    let (view_id, doc) = focused!(editor);
    let text = doc.text().slice(..);
    let selection = doc.selection(view_id);

    // Get the right side of the primary block cursor for forward search, or the
    // grapheme before the start of the selection for reverse search.
    let start = match direction {
        Direction::Forward => text.char_to_byte(graphemes::ensure_grapheme_boundary_next(
            text,
            selection.primary().to(),
        )),
        Direction::Backward => text.char_to_byte(graphemes::ensure_grapheme_boundary_prev(
            text,
            selection.primary().from(),
        )),
    };

    // A regex::Match returns byte-positions in the str. In the case where we
    // do a reverse search and wraparound to the end, we don't need to search
    // the text before the current cursor position for matches, but by slicing
    // it out, we need to add it back to the position of the selection.
    let doc = focused_ref!(editor).1.text().slice(..);

    // use find_at to find the next match after the cursor, loop around the end
    // Careful, `Regex` uses `bytes` as offsets, not character indices!
    let mut mat = match direction {
        Direction::Forward => regex.find(doc.regex_input_at_bytes(start..)),
        Direction::Backward => regex.find_iter(doc.regex_input_at_bytes(..start)).last(),
    };

    if mat.is_none() {
        if wrap_around {
            mat = match direction {
                Direction::Forward => regex.find(doc.regex_input()),
                Direction::Backward => regex.find_iter(doc.regex_input_at_bytes(start..)).last(),
            };
        }
        if show_warnings {
            if wrap_around && mat.is_some() {
                editor.set_status("Wrapped around document");
            } else {
                editor.set_error("No more matches");
            }
        }
    }

    let (view_id, doc) = focused!(editor);
    let text = doc.text().slice(..);
    let selection = doc.selection(view_id);

    if let Some(mat) = mat {
        let start = text.byte_to_char(mat.start());
        let end = text.byte_to_char(mat.end());

        if end == 0 {
            // skip empty matches that don't make sense
            return;
        }

        // Determine range direction based on the primary range
        let primary = selection.primary();
        let range = Range::new(start, end).with_direction(primary.direction());

        let selection = match movement {
            Movement::Extend => selection.clone().push(range),
            Movement::Move => selection.clone().replace(selection.primary_index(), range),
        };

        doc.set_selection(view_id, selection);
        let view = view_mut!(editor, view_id);
        view.ensure_cursor_in_view_center(doc, scrolloff);
    };
}

fn search_completions(cx: &mut Context, reg: Option<char>) -> Vec<String> {
    let mut items = reg
        .and_then(|reg| cx.editor.registers.read(reg, cx.editor))
        .map_or(Vec::new(), |reg| reg.take(200).collect());
    items.sort_unstable();
    items.dedup();
    items.into_iter().map(|value| value.to_string()).collect()
}

fn search(cx: &mut Context) {
    searcher(cx, Direction::Forward)
}

fn rsearch(cx: &mut Context) {
    searcher(cx, Direction::Backward)
}

fn searcher(cx: &mut Context, direction: Direction) {
    let reg = cx.register.unwrap_or('/');
    let config = cx.editor.config();
    let scrolloff = config.scrolloff;
    let wrap_around = config.search.wrap_around;
    let movement = if cx.editor.mode() == Mode::Select {
        Movement::Extend
    } else {
        Movement::Move
    };

    // TODO: could probably share with select_on_matches?
    let completions = search_completions(cx, Some(reg));

    ui::regex_prompt(
        cx,
        "Search".into(),
        Some(reg),
        move |_editor: &Editor, input: &str| {
            completions
                .iter()
                .filter(|comp| comp.starts_with(input))
                .map(|comp| (0.., comp.clone().into()))
                .collect()
        },
        move |cx, regex, event| {
            if event == PromptEvent::Validate {
                cx.editor.registers.last_search_register = reg;
            } else if event != PromptEvent::Update {
                return;
            }
            search_impl(
                cx.editor,
                &regex,
                movement,
                direction,
                scrolloff,
                wrap_around,
                false,
            );
        },
    );
}

fn search_next_or_prev_impl(cx: &mut Context, movement: Movement, direction: Direction) {
    let count = cx.count();
    let register = cx
        .register
        .unwrap_or(cx.editor.registers.last_search_register);
    let config = cx.editor.config();
    let scrolloff = config.scrolloff;
    if let Some(query) = cx.editor.registers.first(register, cx.editor) {
        let search_config = &config.search;
        let case_insensitive = if search_config.smart_case {
            !query.chars().any(char::is_uppercase)
        } else {
            false
        };
        let wrap_around = search_config.wrap_around;
        if let Ok(regex) = rope::RegexBuilder::new()
            .syntax(
                rope::Config::new()
                    .case_insensitive(case_insensitive)
                    .multi_line(true),
            )
            .build(&query)
        {
            for _ in 0..count {
                search_impl(
                    cx.editor,
                    &regex,
                    movement,
                    direction,
                    scrolloff,
                    wrap_around,
                    true,
                );
            }
        } else {
            let error = format!("Invalid regex: {}", query);
            cx.editor.set_error(error);
        }
    }
}

fn search_next(cx: &mut Context) {
    search_next_or_prev_impl(cx, Movement::Move, Direction::Forward);
}

fn search_prev(cx: &mut Context) {
    search_next_or_prev_impl(cx, Movement::Move, Direction::Backward);
}
fn extend_search_next(cx: &mut Context) {
    search_next_or_prev_impl(cx, Movement::Extend, Direction::Forward);
}

fn extend_search_prev(cx: &mut Context) {
    search_next_or_prev_impl(cx, Movement::Extend, Direction::Backward);
}

fn search_selection(cx: &mut Context) {
    search_selection_impl(cx, false)
}

fn search_selection_detect_word_boundaries(cx: &mut Context) {
    search_selection_impl(cx, true)
}

fn search_selection_impl(cx: &mut Context, detect_word_boundaries: bool) {
    fn is_at_word_start(text: RopeSlice, index: usize) -> bool {
        // This can happen when the cursor is at the last character in
        // the document +1 (ge + j), in this case text.char(index) will panic as
        // it will index out of bounds. See https://github.com/helix-editor/helix/issues/12609
        if index == text.len_chars() {
            return false;
        }
        let ch = text.char(index);
        if index == 0 {
            return char_is_word(ch);
        }
        let prev_ch = text.char(index - 1);

        !char_is_word(prev_ch) && char_is_word(ch)
    }

    fn is_at_word_end(text: RopeSlice, index: usize) -> bool {
        if index == 0 || index == text.len_chars() {
            return false;
        }
        let ch = text.char(index);
        let prev_ch = text.char(index - 1);

        char_is_word(prev_ch) && !char_is_word(ch)
    }

    let register = cx.register.unwrap_or('/');
    let (view_id, doc) = focused!(cx.editor);
    let text = doc.text().slice(..);

    let regex = doc
        .selection(view_id)
        .iter()
        .map(|selection| {
            let add_boundary_prefix =
                detect_word_boundaries && is_at_word_start(text, selection.from());
            let add_boundary_suffix =
                detect_word_boundaries && is_at_word_end(text, selection.to());

            let prefix = if add_boundary_prefix { "\\b" } else { "" };
            let suffix = if add_boundary_suffix { "\\b" } else { "" };

            let word = regex::escape(&selection.fragment(text));
            format!("{}{}{}", prefix, word, suffix)
        })
        .collect::<HashSet<_>>() // Collect into hashset to deduplicate identical regexes
        .into_iter()
        .collect::<Vec<_>>()
        .join("|");

    let msg = format!("register '{}' set to '{}'", register, &regex);
    match cx.editor.registers.push(register, regex) {
        Ok(_) => {
            cx.editor.registers.last_search_register = register;
            cx.editor.set_status(msg)
        }
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

fn make_search_word_bounded(cx: &mut Context) {
    // Defaults to the active search register instead `/` to be more ergonomic assuming most people
    // would use this command following `search_selection`. This avoids selecting the register
    // twice.
    let register = cx
        .register
        .unwrap_or(cx.editor.registers.last_search_register);
    let regex = match cx.editor.registers.first(register, cx.editor) {
        Some(regex) => regex,
        None => return,
    };
    let start_anchored = regex.starts_with("\\b");
    let end_anchored = regex.ends_with("\\b");

    if start_anchored && end_anchored {
        return;
    }

    let mut new_regex = String::with_capacity(
        regex.len() + if start_anchored { 0 } else { 2 } + if end_anchored { 0 } else { 2 },
    );

    if !start_anchored {
        new_regex.push_str("\\b");
    }
    new_regex.push_str(&regex);
    if !end_anchored {
        new_regex.push_str("\\b");
    }

    let msg = format!("register '{}' set to '{}'", register, &new_regex);
    match cx.editor.registers.push(register, new_regex) {
        Ok(_) => {
            cx.editor.registers.last_search_register = register;
            cx.editor.set_status(msg)
        }
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

fn global_search(cx: &mut Context) {
    #[derive(Debug)]
    struct FileResult {
        path: PathBuf,
        /// 0 indexed lines
        line_num: usize,
    }

    impl FileResult {
        fn new(path: &Path, line_num: usize) -> Self {
            Self {
                path: path.to_path_buf(),
                line_num,
            }
        }
    }

    struct GlobalSearchConfig {
        smart_case: bool,
        file_picker_config: helix_view::editor::FilePickerConfig,
        directory_style: Style,
        number_style: Style,
        colon_style: Style,
    }

    let config = cx.editor.config();
    let config = GlobalSearchConfig {
        smart_case: config.search.smart_case,
        file_picker_config: config.file_picker.clone(),
        directory_style: cx.editor.theme.get("ui.text.directory"),
        number_style: cx.editor.theme.get("constant.numeric.integer"),
        colon_style: cx.editor.theme.get("punctuation"),
    };

    let columns = [
        PickerColumn::new("path", |item: &FileResult, config: &GlobalSearchConfig| {
            let path = helix_stdx::path::get_relative_path(&item.path);

            let directories = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| format!("{}{}", p.display(), std::path::MAIN_SEPARATOR))
                .unwrap_or_default();

            let filename = item
                .path
                .file_name()
                .expect("global search paths are normalized (can't end in `..`)")
                .to_string_lossy();

            Cell::from(Spans::from(vec![
                Span::styled(directories, config.directory_style),
                Span::raw(filename),
                Span::styled(":", config.colon_style),
                Span::styled((item.line_num + 1).to_string(), config.number_style),
            ]))
        }),
        PickerColumn::hidden("contents"),
    ];

    let get_files = |query: &str,
                     editor: &mut Editor,
                     config: std::sync::Arc<GlobalSearchConfig>,
                     injector: &ui::picker::Injector<_, _>,
                     work: helix_runtime::Work| {
        if query.is_empty() {
            return work.spawn(async { Ok(()) });
        }

        let search_root = helix_stdx::env::current_working_dir();
        if !search_root.exists() {
            return work
                .spawn(async { Err(anyhow::anyhow!("Current working directory does not exist")) });
        }

        let documents: Vec<_> = editor
            .documents()
            .map(|doc| (doc.path().cloned(), doc.text().to_owned()))
            .collect();

        let matcher = match RegexMatcherBuilder::new()
            .case_smart(config.smart_case)
            .build(query)
        {
            Ok(matcher) => {
                // Clear any "Failed to compile regex" errors out of the statusline.
                editor.clear_status();
                matcher
            }
            Err(err) => {
                log::info!("Failed to compile search pattern in global search: {}", err);
                return work.spawn(async { Err(anyhow::anyhow!("Failed to compile regex")) });
            }
        };

        let dedup_symlinks = config.file_picker_config.deduplicate_links;
        let absolute_root = search_root
            .canonicalize()
            .unwrap_or_else(|_| search_root.clone());

        let injector = injector.clone();
        work.spawn(async move {
            let searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .build();
            WalkBuilder::new(search_root)
                .hidden(config.file_picker_config.hidden)
                .parents(config.file_picker_config.parents)
                .ignore(config.file_picker_config.ignore)
                .follow_links(config.file_picker_config.follow_symlinks)
                .git_ignore(config.file_picker_config.git_ignore)
                .git_global(config.file_picker_config.git_global)
                .git_exclude(config.file_picker_config.git_exclude)
                .max_depth(config.file_picker_config.max_depth)
                .filter_entry(move |entry| {
                    filter_picker_entry(entry, &absolute_root, dedup_symlinks)
                })
                .add_custom_ignore_filename(helix_loader::config_dir().join("ignore"))
                .add_custom_ignore_filename(".helix/ignore")
                .build_parallel()
                .run(|| {
                    let mut searcher = searcher.clone();
                    let matcher = matcher.clone();
                    let injector = injector.clone();
                    let documents = &documents;
                    Box::new(move |entry: Result<DirEntry, ignore::Error>| -> WalkState {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(_) => return WalkState::Continue,
                        };

                        if !entry.path().is_file() {
                            return WalkState::Continue;
                        }

                        let mut stop = false;
                        let sink = sinks::UTF8(|line_num, _line_content| {
                            stop = injector
                                .push(FileResult::new(entry.path(), line_num as usize - 1))
                                .is_err();

                            Ok(!stop)
                        });
                        let doc = documents.iter().find(|&(doc_path, _)| {
                            doc_path
                                .as_ref()
                                .is_some_and(|doc_path| doc_path == entry.path())
                        });

                        let result = if let Some((_, doc)) = doc {
                            // there is already a buffer for this file
                            // search the buffer instead of the file because it's faster
                            // and captures new edits without requiring a save
                            if searcher.multi_line_with_matcher(&matcher) {
                                // in this case a continuous buffer is required
                                // convert the rope to a string
                                let text = doc.to_string();
                                searcher.search_slice(&matcher, text.as_bytes(), sink)
                            } else {
                                searcher.search_reader(
                                    &matcher,
                                    RopeReader::new(doc.slice(..)),
                                    sink,
                                )
                            }
                        } else {
                            searcher.search_path(&matcher, entry.path(), sink)
                        };

                        if let Err(err) = result {
                            log::error!("Global search error: {}, {}", entry.path().display(), err);
                        }
                        if stop {
                            WalkState::Quit
                        } else {
                            WalkState::Continue
                        }
                    })
                });
            Ok(())
        })
    };

    let reg = cx.register.unwrap_or('/');
    cx.editor.registers.last_search_register = reg;

    let picker = Picker::new(
        columns,
        1, // contents
        [],
        config,
        crate::ui::PickerRuntime::new(cx.editor.runtime()),
        cx.ingress.clone(),
        move |cx, FileResult { path, line_num, .. }, action| {
            let doc = match cx.editor.open(path, action) {
                Ok(id) => doc_mut!(cx.editor, &id),
                Err(e) => {
                    cx.editor
                        .set_error(format!("Failed to open file '{}': {}", path.display(), e));
                    return;
                }
            };

            let line_num = *line_num;
            let view = view_mut!(cx.editor);
            let text = doc.text();
            if line_num >= text.len_lines() {
                cx.editor.set_error(
                    "The line you jumped to does not exist anymore because the file has changed.",
                );
                return;
            }
            let start = text.line_to_char(line_num);
            let end = text.line_to_char((line_num + 1).min(text.len_lines()));

            doc.set_selection(view.id, Selection::single(start, end));
            if action.align_view(view, doc.id()) {
                align_view(doc, view, Align::Center);
            }
        },
    )
    .with_preview(|_editor, FileResult { path, line_num, .. }| {
        Some((path.as_path().into(), Some((*line_num, *line_num))))
    })
    .with_history_register(Some(reg))
    .with_dynamic_query(get_files, Some(275));

    cx.push_layer(Box::new(overlaid(picker)));
}

/// Local grep search in buffer
fn local_search_grep(cx: &mut Context) {
    #[derive(Debug)]
    struct FileResult {
        path: PathBuf,
        line_num: usize,
        line_content: String,
    }

    impl FileResult {
        fn new(path: &Path, line_num: usize, line_content: String) -> Self {
            Self {
                path: path.to_path_buf(),
                line_num,
                line_content,
            }
        }
    }

    struct LocalSearchConfig {
        smart_case: bool,
        file_picker_config: helix_view::editor::FilePickerConfig,
        number_style: Style,
    }

    let editor_config = cx.editor.config();
    let config = LocalSearchConfig {
        smart_case: editor_config.search.smart_case,
        file_picker_config: editor_config.file_picker.clone(),
        number_style: cx.editor.theme.get("constant.numeric.integer"),
    };

    let columns = [
        PickerColumn::new("line", |item: &FileResult, config: &LocalSearchConfig| {
            let line_num = (item.line_num + 1).to_string();
            // files can never contain more than 99_999_999 lines
            // thus using maximum line length to be 8 for this formatter is valid
            let max_line_num_length = 8;
            // whitespace padding to align results after the line number
            let padding_length = max_line_num_length - line_num.len();
            let padding = " ".repeat(padding_length);
            // create column value to be displayed in the picker
            Cell::from(Spans::from(vec![
                Span::styled(line_num, config.number_style),
                Span::raw(padding),
            ]))
        }),
        PickerColumn::new("", |item: &FileResult, _config: &LocalSearchConfig| {
            // extract line content to be displayed in the picker
            // create column value to be displayed in the picker
            Cell::from(Spans::from(vec![Span::raw(&item.line_content)]))
        }),
    ];

    let get_files = |query: &str,
                     editor: &mut Editor,
                     config: std::sync::Arc<LocalSearchConfig>,
                     injector: &ui::picker::Injector<_, _>,
                     work: helix_runtime::Work| {
        if query.is_empty() {
            return work.spawn(async { Ok(()) });
        }

        let search_root = helix_stdx::env::current_working_dir();
        if !search_root.exists() {
            return work
                .spawn(async { Err(anyhow::anyhow!("Current working directory does not exist")) });
        }

        // Only read the current document (not other documents opened in the buffer)
        let (_, doc) = focused_ref!(editor);
        let documents = vec![(doc.path().cloned(), doc.text().to_owned())];

        let matcher = match RegexMatcherBuilder::new()
            .case_smart(config.smart_case)
            .build(query)
        {
            Ok(matcher) => {
                // Clear any "Failed to compile regex" errors out of the statusline.
                editor.clear_status();
                matcher
            }
            Err(err) => {
                log::info!("Failed to compile search pattern in global search: {}", err);
                return work.spawn(async { Err(anyhow::anyhow!("Failed to compile regex")) });
            }
        };

        let dedup_symlinks = config.file_picker_config.deduplicate_links;
        let absolute_root = search_root
            .canonicalize()
            .unwrap_or_else(|_| search_root.clone());

        let injector = injector.clone();
        work.spawn(async move {
            let searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .build();
            WalkBuilder::new(search_root)
                .hidden(config.file_picker_config.hidden)
                .parents(config.file_picker_config.parents)
                .ignore(config.file_picker_config.ignore)
                .follow_links(config.file_picker_config.follow_symlinks)
                .git_ignore(config.file_picker_config.git_ignore)
                .git_global(config.file_picker_config.git_global)
                .git_exclude(config.file_picker_config.git_exclude)
                .max_depth(config.file_picker_config.max_depth)
                .filter_entry(move |entry| {
                    filter_picker_entry(entry, &absolute_root, dedup_symlinks)
                })
                .add_custom_ignore_filename(helix_loader::config_dir().join("ignore"))
                .add_custom_ignore_filename(".helix/ignore")
                .build_parallel()
                .run(|| {
                    let mut searcher = searcher.clone();
                    let matcher = matcher.clone();
                    let injector = injector.clone();
                    let documents = &documents;
                    Box::new(move |entry: Result<DirEntry, ignore::Error>| -> WalkState {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(_) => return WalkState::Continue,
                        };

                        match entry.file_type() {
                            Some(entry) if entry.is_file() => {}
                            // skip everything else
                            _ => return WalkState::Continue,
                        };

                        let mut stop = false;

                        // Maximum line length of the content displayed within the result picker.
                        // User should be allowed to control this to accomodate their monitor width.
                        // TODO: Expose this setting to the user so they can control it.
                        let local_search_result_line_length = 80;

                        let sink = sinks::UTF8(|line_num, line_content| {
                            stop = injector
                                .push(FileResult::new(
                                    entry.path(),
                                    line_num as usize - 1,
                                    line_content[0..std::cmp::min(
                                        local_search_result_line_length,
                                        line_content.len(),
                                    )]
                                        .to_string(),
                                ))
                                .is_err();

                            Ok(!stop)
                        });
                        let doc = documents.iter().find(|&(doc_path, _)| {
                            doc_path
                                .as_ref()
                                .is_some_and(|doc_path| doc_path == entry.path())
                        });

                        // search in current document
                        let result = if let Some((_, doc)) = doc {
                            // there is already a buffer for this file
                            // search the buffer instead of the file because it's faster
                            // and captures new edits without requiring a save
                            if searcher.multi_line_with_matcher(&matcher) {
                                // in this case a continuous buffer is required
                                // convert the rope to a string
                                let text = doc.to_string();
                                searcher.search_slice(&matcher, text.as_bytes(), sink)
                            } else {
                                searcher.search_reader(
                                    &matcher,
                                    RopeReader::new(doc.slice(..)),
                                    sink,
                                )
                            }
                        } else {
                            // Note: This is a hack!
                            // We ignore all other files.
                            // We only search an empty string (to satisfy rust's return type).
                            searcher.search_slice(&matcher, "".to_owned().as_bytes(), sink)
                        };

                        if let Err(err) = result {
                            log::error!("Local search error: {}, {}", entry.path().display(), err);
                        }
                        if stop {
                            WalkState::Quit
                        } else {
                            WalkState::Continue
                        }
                    })
                });
            Ok(())
        })
    };

    let reg = cx.register.unwrap_or('/');
    cx.editor.registers.last_search_register = reg;

    let picker = Picker::new(
        columns,
        1, // contents
        [],
        config,
        crate::ui::PickerRuntime::new(cx.editor.runtime()),
        cx.ingress.clone(),
        move |cx, FileResult { path, line_num, .. }, action| {
            let doc = match cx.editor.open(path, action) {
                Ok(id) => doc_mut!(cx.editor, &id),
                Err(e) => {
                    cx.editor
                        .set_error(format!("Failed to open file '{}': {}", path.display(), e));
                    return;
                }
            };

            let line_num = *line_num;
            let view = view_mut!(cx.editor);
            let text = doc.text();
            if line_num >= text.len_lines() {
                cx.editor.set_error(
                    "The line you jumped to does not exist anymore because the file has changed.",
                );
                return;
            }
            let start = text.line_to_char(line_num);
            let end = text.line_to_char((line_num + 1).min(text.len_lines()));

            doc.set_selection(view.id, Selection::single(start, end));
            if action.align_view(view, doc.id()) {
                align_view(doc, view, Align::Center);
            }
        },
    )
    .with_preview(|_editor, FileResult { path, line_num, .. }| {
        Some((path.as_path().into(), Some((*line_num, *line_num))))
    })
    .with_history_register(Some(reg))
    .with_dynamic_query(get_files, Some(275));
    cx.push_layer(Box::new(overlaid(picker)));
}

fn local_search_fuzzy(cx: &mut Context) {
    #[derive(Debug)]
    struct FileResult {
        path: std::sync::Arc<PathBuf>,
        line_num: usize,
        file_contents_byte_start: usize,
        file_contents_byte_end: usize,
    }

    struct LocalSearchData {
        file_contents: String,
        number_style: Style,
    }

    let (_, current_document) = focused_ref!(cx.editor);
    let Some(current_document_path) = current_document.path() else {
        cx.editor.set_error("Failed to get current document path");
        return;
    };

    let file_contents = std::fs::read_to_string(current_document_path).unwrap();

    let current_document_path = std::sync::Arc::new(current_document_path.clone());

    let file_results: Vec<FileResult> = file_contents
        .lines()
        .enumerate()
        .filter_map(|(line_num, line)| {
            if !line.trim().is_empty() {
                // SAFETY: The offsets will be used to index back into the original `file_contents` String
                // as a byte slice. Since the `file_contents` will be moved into the `Picker` as part of
                // `editor_data`, we know that the `Picker` will take ownership of the underlying String,
                // so it will be valid for displaying the `Span` as long as the user uses the `Picker`
                // (the `Picker` gets dropped only when a new `Picker` is created). Furthermore, the
                // process of reconstructing a `&str` back requires that we have access to the original
                // `String` anyways so we can index into it, as is the case when we construct the `Span`
                // when creating the `PickerColumn`s, so we know that we are returning the correct
                // substring from the original `file_contents`.
                // In fact, since we only store offsets, and accessing them from safe rust, there is
                // no risk of memory safety (like our &str not living long enough). The only real
                // bug would be moving out the original underlying `String` (which we obviously
                // don't do). This would lead to an out of bounds crash in the `PickerColumn` function
                // call, or a crash when we recreate back the &str if the new underlying `String`
                // makes it so that our byte offsets index into the middle of a Unicode grapheme cluster.
                // Last but not least, it could make it so that we do display the lines correctly,
                // but these are from a different underlying `String` than the original, which would be
                // different from the lines in the current buffer.
                let beg =
                    unsafe { line.as_ptr().byte_offset_from(file_contents.as_ptr()) } as usize;
                let end = beg + line.len();
                let result = FileResult {
                    path: current_document_path.clone(),
                    line_num,
                    file_contents_byte_start: beg,
                    file_contents_byte_end: end,
                };
                Some(result)
            } else {
                None
            }
        })
        .collect();

    let config = LocalSearchData {
        number_style: cx.editor.theme.get("constant.numeric.integer"),
        file_contents,
    };

    let columns = [
        PickerColumn::new("line", |item: &FileResult, config: &LocalSearchData| {
            let line_num = (item.line_num + 1).to_string();
            // files can never contain more than 99_999_999 lines
            // thus using maximum line length to be 8 for this formatter is valid
            let max_line_num_length = 8;
            // whitespace padding to align results after the line number
            let padding_length = max_line_num_length - line_num.len();
            let padding = " ".repeat(padding_length);
            // create column value to be displayed in the picker
            Cell::from(Spans::from(vec![
                Span::styled(line_num, config.number_style),
                Span::raw(padding),
            ]))
        }),
        PickerColumn::new("", |item: &FileResult, config: &LocalSearchData| {
            // extract line content to be displayed in the picker
            let slice = &config.file_contents.as_bytes()
                [item.file_contents_byte_start..item.file_contents_byte_end];
            let content = std::str::from_utf8(slice).unwrap();
            // create column value to be displayed in the picker
            Cell::from(Spans::from(vec![Span::raw(content)]))
        }),
    ];

    let reg = cx.register.unwrap_or('/');
    cx.editor.registers.last_search_register = reg;

    let picker = Picker::new(
        columns,
        1, // contents
        [],
        config,
        crate::ui::PickerRuntime::new(cx.editor.runtime()),
        cx.ingress.clone(),
        move |cx, FileResult { path, line_num, .. }, action| {
            let doc = match cx.editor.open(path, action) {
                Ok(id) => doc_mut!(cx.editor, &id),
                Err(e) => {
                    cx.editor
                        .set_error(format!("Failed to open file '{}': {}", path.display(), e));
                    return;
                }
            };

            let line_num = *line_num;
            let view = view_mut!(cx.editor);
            let text = doc.text();
            if line_num >= text.len_lines() {
                cx.editor.set_error(
                    "The line you jumped to does not exist anymore because the file has changed.",
                );
                return;
            }
            let start = text.line_to_char(line_num);
            let end = text.line_to_char((line_num + 1).min(text.len_lines()));

            doc.set_selection(view.id, Selection::single(start, end));
            if action.align_view(view, doc.id()) {
                align_view(doc, view, Align::Center);
            }
        },
    )
    .with_preview(|_editor, FileResult { path, line_num, .. }| {
        Some((path.as_path().into(), Some((*line_num, *line_num))))
    })
    .with_history_register(Some(reg));

    let injector = picker.injector();
    let timeout = std::time::Instant::now() + std::time::Duration::from_millis(30);
    for file_result in file_results {
        if injector.push(file_result).is_err() {
            break;
        }
        if std::time::Instant::now() >= timeout {
            break;
        }
    }

    cx.push_layer(Box::new(overlaid(picker)));
}

fn enter_insert_mode(cx: &mut Context) {
    cx.editor.mode = Mode::Insert;
}

// inserts at the start of each selection
fn insert_mode(cx: &mut Context) {
    enter_insert_mode(cx);
    let (view_id, doc) = focused!(cx.editor);

    log::trace!(
        "entering insert mode with sel: {:?}, text: {:?}",
        doc.selection(view_id),
        doc.text().to_string()
    );

    let selection = doc
        .selection(view_id)
        .clone()
        .transform(|range| Range::new(range.to(), range.from()));

    doc.set_selection(view_id, selection);
}

// inserts at the end of each selection
fn append_mode(cx: &mut Context) {
    enter_insert_mode(cx);
    let (view_id, doc) = focused!(cx.editor);
    doc.mark_restore_cursor();
    let text = doc.text().slice(..);

    // Make sure there's room at the end of the document if the last
    // selection butts up against it.
    let end = text.len_chars();
    let last_range = doc
        .selection(view_id)
        .iter()
        .last()
        .expect("selection should always have at least one range");
    if !last_range.is_empty() && last_range.to() == end {
        let transaction = Transaction::change(
            doc.text(),
            [(end, end, Some(doc.line_ending().as_str().into()))].into_iter(),
        );
        doc.apply(&transaction, view_id);
    }

    let selection = doc.selection(view_id).clone().transform(|range| {
        Range::new(
            range.from(),
            graphemes::next_grapheme_boundary(doc.text().slice(..), range.to()),
        )
    });
    doc.set_selection(view_id, selection);
}

fn file_picker(cx: &mut Context) {
    let root = find_workspace().0;
    if !root.exists() {
        cx.editor.set_error("Workspace directory does not exist");
        return;
    }
    let picker = ui::file_picker(cx.editor, root, cx.ingress.clone());
    if cx.editor.config().file_picker.hide_preview {
        let overlay = ui::overlay::Overlay {
            content: picker,
            calc_child_size: Box::new(|rect| {
                ui::overlay::clip_rect_relative(rect.clip_bottom(2), 75, 90)
            }),
        };
        cx.push_layer(Box::new(overlay));
    } else {
        cx.push_layer(Box::new(overlaid(picker)));
    }
}

fn file_picker_in_current_buffer_directory(cx: &mut Context) {
    let doc_dir = focused_ref!(cx.editor)
        .1
        .path()
        .and_then(|path| path.parent().map(|path| path.to_path_buf()));

    let path = match doc_dir {
        Some(path) => path,
        None => {
            let cwd = helix_stdx::env::current_working_dir();
            if !cwd.exists() {
                cx.editor.set_error(
                    "Current buffer has no parent and current working directory does not exist",
                );
                return;
            }
            cx.editor.set_error(
                "Current buffer has no parent, opening file picker in current working directory",
            );
            cwd
        }
    };

    let picker = ui::file_picker(cx.editor, path, cx.ingress.clone());
    cx.push_layer(Box::new(overlaid(picker)));
}

fn file_picker_in_current_directory(cx: &mut Context) {
    let cwd = helix_stdx::env::current_working_dir();
    if !cwd.exists() {
        cx.editor
            .set_error("Current working directory does not exist");
        return;
    }
    let picker = ui::file_picker(cx.editor, cwd, cx.ingress.clone());
    cx.push_layer(Box::new(overlaid(picker)));
}

fn file_explorer(cx: &mut Context) {
    let root = find_workspace().0;
    if !root.exists() {
        cx.editor.set_error("Workspace directory does not exist");
        return;
    }

    if let Ok(picker) = ui::file_explorer(None, root, cx.editor, cx.ingress.clone()) {
        cx.push_layer(Box::new(overlaid(picker)));
    }
}

fn file_explorer_in_current_buffer_directory(cx: &mut Context) {
    let doc_dir = focused_ref!(cx.editor)
        .1
        .path()
        .and_then(|path| path.parent().map(|path| path.to_path_buf()));

    let path = match doc_dir {
        Some(path) => path,
        None => {
            let cwd = helix_stdx::env::current_working_dir();
            if !cwd.exists() {
                cx.editor.set_error(
                    "Current buffer has no parent and current working directory does not exist",
                );
                return;
            }
            cx.editor.set_error(
                "Current buffer has no parent, opening file explorer in current working directory",
            );
            cwd
        }
    };

    if let Ok(picker) = ui::file_explorer(None, path, cx.editor, cx.ingress.clone()) {
        cx.push_layer(Box::new(overlaid(picker)));
    }
}

fn file_explorer_in_current_directory(cx: &mut Context) {
    let cwd = helix_stdx::env::current_working_dir();
    if !cwd.exists() {
        cx.editor
            .set_error("Current working directory does not exist");
        return;
    }

    if let Ok(picker) = ui::file_explorer(None, cwd, cx.editor, cx.ingress.clone()) {
        cx.push_layer(Box::new(overlaid(picker)));
    }
}

fn buffer_picker(cx: &mut Context) {
    let current = view!(cx.editor).doc;

    struct BufferMeta {
        id: DocumentId,
        path: Option<PathBuf>,
        is_modified: bool,
        is_current: bool,
        focused_at: std::time::Instant,
    }

    let new_meta = |doc: &Document| BufferMeta {
        id: doc.id(),
        path: doc.path().cloned(),
        is_modified: doc.is_modified(),
        is_current: doc.id() == current,
        focused_at: doc.focused_at(),
    };

    let mut items = cx
        .editor
        .documents
        .values()
        .map(new_meta)
        .collect::<Vec<BufferMeta>>();

    // mru
    items.sort_unstable_by_key(|item| std::cmp::Reverse(item.focused_at));

    let columns = [
        PickerColumn::new("id", |meta: &BufferMeta, _| meta.id.to_string().into()),
        PickerColumn::new("flags", |meta: &BufferMeta, _| {
            let mut flags = String::new();
            if meta.is_modified {
                flags.push('+');
            }
            if meta.is_current {
                flags.push('*');
            }
            flags.into()
        }),
        PickerColumn::new("path", |meta: &BufferMeta, _| {
            let path = meta
                .path
                .as_deref()
                .map(helix_stdx::path::get_relative_path);

            let name = path
                .as_deref()
                .and_then(Path::to_str)
                .unwrap_or(SCRATCH_BUFFER_NAME);
            let icons = ICONS.load();

            let mut spans = Vec::with_capacity(2);

            if let Some(icon) = icons
                .mime()
                .get(path.as_ref().map(|path| path.to_path_buf()).as_ref(), None)
            {
                if let Some(color) = icon.color() {
                    spans.push(Span::styled(
                        format!("{}  ", icon.glyph()),
                        Style::default().fg(color),
                    ));
                } else {
                    spans.push(Span::raw(format!("{}  ", icon.glyph())));
                }
            }

            spans.push(Span::raw(name.to_string()));

            Spans::from(spans).into()
        }),
    ];

    let initial_cursor = if cx
        .editor
        .config()
        .buffer_picker
        .start_position
        .is_previous()
        && !items.is_empty()
    {
        1
    } else {
        0
    };

    let picker = Picker::new(
        columns,
        2,
        items,
        (),
        crate::ui::PickerRuntime::new(cx.editor.runtime()),
        cx.ingress.clone(),
        |cx, meta, action| {
            cx.editor.switch(meta.id, action);
        },
    )
    .with_cursor(initial_cursor)
    .with_preview(|editor, meta| {
        let doc = &editor.documents.get(&meta.id)?;
        let lines = doc.selections().values().next().map(|selection| {
            let cursor_line = selection.primary().cursor_line(doc.text().slice(..));
            (cursor_line, cursor_line)
        });
        Some((meta.id.into(), lines))
    });
    cx.push_layer(Box::new(overlaid(picker)));
}

fn jumplist_picker(cx: &mut Context) {
    struct JumpMeta {
        id: DocumentId,
        path: Option<PathBuf>,
        selection: Selection,
        text: String,
        is_current: bool,
    }

    for (view, _) in cx.editor.tree.views_mut() {
        for doc_id in view
            .history
            .jumps
            .iter()
            .map(|e| e.0)
            .collect::<Vec<_>>()
            .iter()
        {
            let doc = doc_mut!(cx.editor, doc_id);
            view.sync_changes(doc);
        }
    }

    let new_meta = |view: &View, doc_id: DocumentId, selection: Selection| {
        let doc = &cx.editor.documents.get(&doc_id);
        let text = doc.map_or("".into(), |d| {
            selection
                .fragments(d.text().slice(..))
                .map(Cow::into_owned)
                .collect::<Vec<_>>()
                .join(" ")
        });

        JumpMeta {
            id: doc_id,
            path: doc.and_then(|d| d.path().cloned()),
            selection,
            text,
            is_current: view.doc == doc_id,
        }
    };

    let columns = [
        ui::PickerColumn::new("id", |item: &JumpMeta, _| item.id.to_string().into()),
        ui::PickerColumn::new("path", |item: &JumpMeta, _| {
            let path = item
                .path
                .as_deref()
                .map(helix_stdx::path::get_relative_path);

            let name = path
                .as_deref()
                .and_then(Path::to_str)
                .unwrap_or(SCRATCH_BUFFER_NAME);
            let icons = ICONS.load();

            let mut spans = Vec::with_capacity(2);

            if let Some(icon) = icons
                .mime()
                .get(path.as_ref().map(|path| path.to_path_buf()).as_ref(), None)
            {
                if let Some(color) = icon.color() {
                    spans.push(Span::styled(
                        format!("{}  ", icon.glyph()),
                        Style::default().fg(color),
                    ));
                } else {
                    spans.push(Span::raw(format!("{}  ", icon.glyph())));
                }
            }

            spans.push(Span::raw(name.to_string()));

            Spans::from(spans).into()
        }),
        ui::PickerColumn::new("flags", |item: &JumpMeta, _| {
            let mut flags = Vec::new();
            if item.is_current {
                flags.push("*");
            }

            if flags.is_empty() {
                "".into()
            } else {
                format!(" ({})", flags.join("")).into()
            }
        }),
        ui::PickerColumn::new("contents", |item: &JumpMeta, _| item.text.as_str().into()),
    ];

    let picker = Picker::new(
        columns,
        1, // path
        cx.editor.tree.views().flat_map(|(view, _)| {
            view.history
                .jumps
                .iter()
                .rev()
                .map(|(doc_id, selection)| new_meta(view, *doc_id, selection.clone()))
        }),
        (),
        crate::ui::PickerRuntime::new(cx.editor.runtime()),
        cx.ingress.clone(),
        |cx, meta, action| {
            cx.editor.switch(meta.id, action);
            let config = cx.editor.config();
            let (view, doc) = (view_mut!(cx.editor), doc_mut!(cx.editor, &meta.id));
            doc.set_selection(view.id, meta.selection.clone());
            if action.align_view(view, doc.id()) {
                view.ensure_cursor_in_view_center(doc, config.scrolloff);
            }
        },
    )
    .with_preview(|editor, meta| {
        let doc = &editor.documents.get(&meta.id)?;
        let line = meta.selection.primary().cursor_line(doc.text().slice(..));
        Some((meta.id.into(), Some((line, line))))
    });
    cx.push_layer(Box::new(overlaid(picker)));
}

fn changed_file_picker(cx: &mut Context) {
    pub struct FileChangeData {
        cwd: PathBuf,
        style_untracked: Style,
        style_modified: Style,
        style_conflict: Style,
        style_deleted: Style,
        style_renamed: Style,
    }

    let cwd = helix_stdx::env::current_working_dir();
    if !cwd.exists() {
        cx.editor
            .set_error("Current working directory does not exist");
        return;
    }

    let added = cx.editor.theme.get("diff.plus");
    let modified = cx.editor.theme.get("diff.delta");
    let conflict = cx.editor.theme.get("diff.delta.conflict");
    let deleted = cx.editor.theme.get("diff.minus");
    let renamed = cx.editor.theme.get("diff.delta.moved");

    let columns = [
        PickerColumn::new("change", |change: &FileChange, data: &FileChangeData| {
            let icons = ICONS.load();
            match change {
                FileChange::Untracked { .. } => Span::styled(
                    format!("{}  untracked", icons.vcs().added()),
                    data.style_untracked,
                ),
                FileChange::Modified { .. } => Span::styled(
                    format!("{}  modified", icons.vcs().modified()),
                    data.style_modified,
                ),
                FileChange::Conflict { .. } => Span::styled(
                    format!("{}  conflict", icons.vcs().conflict()),
                    data.style_conflict,
                ),
                FileChange::Deleted { .. } => Span::styled(
                    format!("{}  deleted", icons.vcs().removed()),
                    data.style_deleted,
                ),
                FileChange::Renamed { .. } => Span::styled(
                    format!("{}  renamed", icons.vcs().renamed()),
                    data.style_renamed,
                ),
            }
            .into()
        }),
        PickerColumn::new("path", |change: &FileChange, data: &FileChangeData| {
            let display_path = |path: &PathBuf| {
                path.strip_prefix(&data.cwd)
                    .unwrap_or(path)
                    .display()
                    .to_string()
            };
            match change {
                FileChange::Untracked { path } => display_path(path),
                FileChange::Modified { path } => display_path(path),
                FileChange::Conflict { path } => display_path(path),
                FileChange::Deleted { path } => display_path(path),
                FileChange::Renamed { from_path, to_path } => {
                    format!("{} -> {}", display_path(from_path), display_path(to_path))
                }
            }
            .into()
        }),
    ];

    let picker = Picker::new(
        columns,
        1, // path
        [],
        FileChangeData {
            cwd: cwd.clone(),
            style_untracked: added,
            style_modified: modified,
            style_conflict: conflict,
            style_deleted: deleted,
            style_renamed: renamed,
        },
        crate::ui::PickerRuntime::new(cx.editor.runtime()),
        cx.ingress.clone(),
        |cx, meta: &FileChange, action| {
            let path_to_open = meta.path();
            if let Err(e) = cx.editor.open(path_to_open, action) {
                let err = if let Some(err) = e.source() {
                    format!("{}", err)
                } else {
                    format!("unable to open \"{}\"", path_to_open.display())
                };
                cx.editor.set_error(err);
            }
        },
    )
    .with_preview(|_editor, meta| Some((meta.path().into(), None)));
    let injector = picker.injector();
    let ingress = cx.ingress.clone();

    cx.editor
        .diff_providers
        .clone()
        .for_each_changed_file(cwd, move |change| match change {
            Ok(change) => injector.push(change).is_ok(),
            Err(err) => {
                let message = crate::runtime::ingress::StatusMessage::from(err);
                helix_runtime::send_blocking(
                    &ingress,
                    RuntimeEvent::Status {
                        message: message.message.into_owned(),
                        severity: message.severity,
                    },
                );
                true
            }
        });
    cx.push_layer(Box::new(overlaid(picker)));
}

pub fn command_palette(cx: &mut Context) {
    let register = cx.register;
    let count = cx.count;
    cx.callback
        .push(crate::compositor::PostAction::ShowCommandPalette { register, count });
}

fn last_picker(cx: &mut Context) {
    // TODO: last picker does not seem to work well with buffer_picker
    cx.callback
        .push(crate::compositor::PostAction::RestoreLastPicker);
}

pub(crate) fn show_command_palette(
    compositor: &mut Compositor,
    cx: &mut compositor::Context,
    register: Option<char>,
    count: Option<NonZeroUsize>,
) {
    let keymap =
        compositor.find::<ui::EditorView>().unwrap().keymaps.map()[&cx.editor.mode].reverse_map();

    let mut commands: Vec<MappableCommand> = MappableCommand::builtin_commands().to_vec();
    commands.extend(
        typed::TYPABLE_COMMAND_MAP
            .values()
            .map(|cmd| MappableCommand::Typable {
                name: cmd.name.to_owned(),
                args: String::new(),
                doc: cmd.doc.to_owned(),
            }),
    );

    if let Some(pm) = &cx.plugin_manager {
        commands.extend(
            pm.get_commands()
                .into_iter()
                .map(|meta| MappableCommand::Typable {
                    name: meta.name,
                    args: String::new(),
                    doc: meta.doc,
                }),
        );
    }

    let columns = [
        ui::PickerColumn::new("name", |item, _| match item {
            MappableCommand::Typable { name, .. } => format!(":{name}").into(),
            MappableCommand::Engine { spec } => spec.name().into(),
            MappableCommand::Frontend { spec } => spec.name().into(),
            MappableCommand::Macro { .. } => {
                unreachable!("macros aren't included in the command palette")
            }
        }),
        ui::PickerColumn::new(
            "bindings",
            |item: &MappableCommand, keymap: &crate::keymap::ReverseKeymap| {
                keymap
                    .get(item.name())
                    .map(|bindings| {
                        bindings.iter().fold(String::new(), |mut acc, bind| {
                            if !acc.is_empty() {
                                acc.push(' ');
                            }
                            for key in bind {
                                acc.push_str(&key.key_sequence_format());
                            }
                            acc
                        })
                    })
                    .unwrap_or_default()
                    .into()
            },
        ),
        ui::PickerColumn::new("doc", |item: &MappableCommand, _| item.doc().into()),
    ];

    let registry = compositor
        .find::<ui::EditorView>()
        .unwrap()
        .registry
        .clone();
    let picker = Picker::new(
        columns,
        0,
        commands,
        keymap,
        crate::ui::PickerRuntime::new(cx.editor.runtime()),
        cx.ingress.clone(),
        move |cx, command, _action| {
            let mut ctx = Context {
                register,
                count,
                editor: cx.editor,
                registry: registry.clone(),
                notifier: cx.notifier.clone(),
                callback: Vec::new(),
                on_next_key_callback: None,
                exit_tasks: cx.exit_tasks,
                exit_task_work: cx.exit_task_work.clone(),
                ingress: cx.ingress.clone(),
                idle_reset_tx: cx.idle_reset_tx.clone(),
                plugin_manager: cx.plugin_manager.clone(),
            };
            let focus = view!(ctx.editor).id;

            command.execute(&mut ctx);

            if ctx.editor.contains_view(focus) {
                let config = ctx.editor.config();
                let mode = ctx.editor.mode();
                let view = view_mut!(ctx.editor, focus);
                let doc = doc_mut!(ctx.editor, &view.doc);

                view.ensure_cursor_in_view(doc, config.scrolloff);

                if mode != Mode::Insert {
                    doc.append_changes_to_history(view);
                }
            }
        },
    );
    compositor.push(Box::new(overlaid(picker)));
}

/// Fallback position to use for [`insert_with_indent`].
enum IndentFallbackPos {
    LineStart,
    LineEnd,
}

// `I` inserts at the first nonwhitespace character of each line with a selection.
// If the line is empty, automatically indent.
fn insert_at_line_start(cx: &mut Context) {
    insert_with_indent(cx, IndentFallbackPos::LineStart);
}

pub(crate) fn blame_line_impl(editor: &mut Editor, doc_id: DocumentId, cursor_line: u32) {
    let inline_blame_config = &editor.config().inline_blame;
    let Some(doc) = editor.document(doc_id) else {
        return;
    };
    let line_blame = match doc.line_blame(cursor_line, &inline_blame_config.format) {
        result
            if (result.is_ok() && doc.is_blame_outdated())
                || matches!(result, Err(LineBlameError::NotReadyYet) if !inline_blame_config.auto_fetch) =>
        {
            if let Some(path) = doc.path() {
                editor.request_blame(helix_view::handlers::BlameEvent {
                    path: path.to_path_buf(),
                    doc_id: doc.id(),
                    line: Some(cursor_line),
                });
                editor.set_status(format!("Requested blame for {}...", path.display()));
                let doc = editor
                    .document_mut(doc_id)
                    .expect("exists since we return from the function earlier if it does not");
                doc.clear_blame_outdated();
            } else {
                editor.set_error("Could not get path of document");
            };
            return;
        }
        Ok(line_blame) => line_blame,
        Err(err @ (LineBlameError::NotCommittedYet | LineBlameError::NotReadyYet)) => {
            editor.set_status(err.to_string());
            return;
        }
        Err(err @ LineBlameError::NoFileBlame(_, _)) => {
            editor.set_error(err.to_string());
            return;
        }
    };

    editor.set_status(line_blame);
}

fn blame_line(cx: &mut Context) {
    let (view_id, doc) = focused_ref!(cx.editor);
    blame_line_impl(cx.editor, doc.id(), doc.cursor_line(view_id) as u32);
}

#[cfg(test)]
mod tests {
    use super::{CommandScope, MappableCommand};
    use helix_modal::populate::build_registry;

    #[test]
    fn engine_commands_are_registered_in_modal_registry() {
        let registry = build_registry();

        for command in MappableCommand::builtin_commands() {
            match command {
                MappableCommand::Engine { spec } => {
                    assert_eq!(command.modal_command(), Some(spec.token()));
                    assert!(
                        registry.contains(spec.token()),
                        "engine command `{}` is missing from modal registry",
                        command.name()
                    );
                }
                MappableCommand::Frontend { .. } => {
                    assert_eq!(command.modal_command(), None);
                }
                MappableCommand::Typable { .. } | MappableCommand::Macro { .. } => {
                    panic!("static command list should not contain dynamic commands");
                }
            }
        }
    }

    #[test]
    fn modal_registry_does_not_contain_undeclared_engine_commands() {
        let registry = build_registry();

        let mut declared = MappableCommand::builtin_commands()
            .iter()
            .filter_map(|command| match command {
                MappableCommand::Engine { spec } => Some(spec.token()),
                _ => None,
            })
            .collect::<Vec<_>>();
        declared.sort_by_key(|token| token.as_str());

        let mut registered = registry.tokens().collect::<Vec<_>>();
        registered.sort_by_key(|token| token.as_str());

        assert_eq!(registered, declared);
    }

    #[test]
    fn only_jumplist_traversal_commands_are_tree_only() {
        let mut actual = MappableCommand::builtin_commands()
            .iter()
            .filter_map(|command| match command {
                MappableCommand::Engine { spec } if spec.scope() == CommandScope::Tree => {
                    Some(spec.token())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        actual.sort_by_key(|token| token.as_str());

        let mut expected = vec![
            MappableCommand::named("jump_backward")
                .expect("jump_backward exists")
                .modal_command()
                .expect("jump_backward is an engine command"),
            MappableCommand::named("jump_forward")
                .expect("jump_forward exists")
                .modal_command()
                .expect("jump_forward is an engine command"),
        ];
        expected.sort_by_key(|token| token.as_str());

        assert_eq!(actual, expected);
    }
}

// `A` inserts at the end of each line with a selection.
// If the line is empty, automatically indent.
fn insert_at_line_end(cx: &mut Context) {
    insert_with_indent(cx, IndentFallbackPos::LineEnd);
}

// Enter insert mode and auto-indent the current line if it is empty.
// If the line is not empty, move the cursor to the specified fallback position.
fn insert_with_indent(cx: &mut Context, cursor_fallback: IndentFallbackPos) {
    enter_insert_mode(cx);

    let (view_id, doc) = focused!(cx.editor);
    let loader = cx.editor.syn_loader.load();

    let text = doc.text().slice(..);
    let contents = doc.text();
    let selection = doc.selection(view_id);

    let mut ranges = SmallVec::with_capacity(selection.len());
    let mut offs = 0;

    let mut transaction = Transaction::change_by_selection(contents, selection, |range| {
        let cursor_line = range.cursor_line(text);
        let cursor_line_start = text.line_to_char(cursor_line);

        if line_end_char_index(&text, cursor_line) == cursor_line_start {
            // line is empty => auto indent
            let line_end_index = cursor_line_start;

            let indent = doc.indent_for_newline(
                &loader,
                &doc.config.load().indent_heuristic,
                text,
                cursor_line,
                line_end_index,
                cursor_line,
            );

            // calculate new selection ranges
            let pos = offs + cursor_line_start;
            let indent_width = indent.chars().count();
            ranges.push(Range::point(pos + indent_width));
            offs += indent_width;

            (line_end_index, line_end_index, Some(indent.into()))
        } else {
            // move cursor to the fallback position
            let pos = match cursor_fallback {
                IndentFallbackPos::LineStart => text
                    .line(cursor_line)
                    .first_non_whitespace_char()
                    .map(|ws_offset| ws_offset + cursor_line_start)
                    .unwrap_or(cursor_line_start),
                IndentFallbackPos::LineEnd => line_end_char_index(&text, cursor_line),
            };

            ranges.push(range.put_cursor(text, pos + offs, cx.editor.mode == Mode::Select));

            (cursor_line_start, cursor_line_start, None)
        }
    });

    transaction = transaction.with_selection(Selection::new(ranges, selection.primary_index()));
    doc.apply(&transaction, view_id);
}

/// Waits for formatting changes, then returns a typed [`RuntimeTaskEvent`] for the main loop.
///
/// TODO: provide some way to cancel this, probably as part of a more general job cancellation
/// scheme
pub(crate) async fn make_format_task_event(
    doc_id: DocumentId,
    doc_version: i32,
    view_id: ViewId,
    format: DocumentFormatTask,
    write: Option<crate::runtime::PendingFormatWrite>,
) -> anyhow::Result<RuntimeTaskEvent> {
    let format_result = match format.await {
        Ok(result) => result,
        Err(err) => Err(FormatterError::TaskFailed(err)),
    };
    Ok(RuntimeTaskEvent::ApplyFormattingResult {
        doc_id,
        view_id,
        expected_version: doc_version,
        format_result,
        write,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Open {
    Below,
    Above,
}

impl Open {
    pub fn from_signature_help_position(pos: &helix_view::editor::SignatureHelpPosition) -> Self {
        match pos {
            helix_view::editor::SignatureHelpPosition::Above => Self::Above,
            helix_view::editor::SignatureHelpPosition::Below => Self::Below,
        }
    }
}

#[derive(PartialEq)]
pub enum CommentContinuation {
    Enabled,
    Disabled,
}

fn open(cx: &mut Context, open: Open, comment_continuation: CommentContinuation) {
    let count = cx.count();
    enter_insert_mode(cx);
    let config = cx.editor.config();
    let (view_id, doc) = focused!(cx.editor);
    let view = view!(cx.editor, view_id);
    let loader = cx.editor.syn_loader.load();
    let mut annotations = view.text_annotations(doc, None);

    let text = doc.text().slice(..);
    let contents = doc.text();
    let selection = doc.selection(view_id);
    let mut offs = 0;

    let mut ranges = SmallVec::with_capacity(selection.len());

    let continue_comment_tokens =
        if comment_continuation == CommentContinuation::Enabled && config.continue_comments {
            doc.language_config()
                .and_then(|config| config.comment_tokens.as_ref())
        } else {
            None
        };

    let mut transaction = Transaction::change_by_selection(contents, selection, |range| {
        // if open is Below and next line is folded,
        // move the range to the next visible line, and open Above
        let (range, open) = (open == Open::Below)
            .then(|| {
                let next_line_is_folded = {
                    let next_line = text.char_to_line(prev_grapheme_boundary(text, range.to())) + 1;
                    annotations
                        .folds
                        .superest_fold_containing(next_line, |fold| fold.start.line..=fold.end.line)
                        .is_some()
                };

                next_line_is_folded.then(|| {
                    move_vertically(
                        text,
                        *range,
                        Direction::Forward,
                        1,
                        Movement::Move,
                        &TextFormat::default(),
                        &mut annotations,
                    )
                })
            })
            .flatten()
            .map_or((*range, open), |range| (range, Open::Above));

        // the line number, where the cursor is currently
        let curr_line_num = text.char_to_line(match open {
            Open::Below => graphemes::prev_grapheme_boundary(text, range.to()),
            Open::Above => range.from(),
        });

        // the next line number, where the cursor will be, after finishing the transaction
        let next_new_line_num = match open {
            Open::Below => curr_line_num + 1,
            Open::Above => curr_line_num,
        };

        let above_next_new_line_num = next_new_line_num.saturating_sub(1);

        let continue_comment_token = continue_comment_tokens
            .and_then(|tokens| comment::get_comment_token(text, tokens, curr_line_num));

        // Index to insert newlines after, as well as the char width
        // to use to compensate for those inserted newlines.
        let (above_next_line_end_index, above_next_line_end_width) = if next_new_line_num == 0 {
            (0, 0)
        } else {
            (
                line_end_char_index(&text, above_next_new_line_num),
                doc.line_ending().len_chars(),
            )
        };

        let line = text.line(curr_line_num);
        let indent = match line.first_non_whitespace_char() {
            Some(pos) if continue_comment_token.is_some() => line.slice(..pos).to_string(),
            _ => doc.indent_for_newline(
                &loader,
                &config.indent_heuristic,
                text,
                above_next_new_line_num,
                above_next_line_end_index,
                curr_line_num,
            ),
        };

        let indent_len = indent.len();
        let mut text = String::with_capacity(1 + indent_len);

        if open == Open::Above && next_new_line_num == 0 {
            text.push_str(&indent);
            if let Some(token) = continue_comment_token {
                text.push_str(token);
                text.push(' ');
            }
            text.push_str(doc.line_ending().as_str());
        } else {
            text.push_str(doc.line_ending().as_str());
            text.push_str(&indent);

            if let Some(token) = continue_comment_token {
                text.push_str(token);
                text.push(' ');
            }
        }

        let text = text.repeat(count);

        // calculate new selection ranges
        let pos = offs + above_next_line_end_index + above_next_line_end_width;
        let comment_len = continue_comment_token
            .map(|token| token.len() + 1) // `+ 1` for the extra space added
            .unwrap_or_default();
        for i in 0..count {
            // pos                     -> beginning of reference line,
            // + (i * (line_ending_len + indent_len + comment_len)) -> beginning of i'th line from pos (possibly including comment token)
            // + indent_len + comment_len ->        -> indent for i'th line
            ranges.push(Range::point(
                pos + (i * (doc.line_ending().len_chars() + indent_len + comment_len))
                    + indent_len
                    + comment_len,
            ));
        }

        // update the offset for the next range
        offs += text.chars().count();

        (
            above_next_line_end_index,
            above_next_line_end_index,
            Some(text.into()),
        )
    });
    drop(annotations);

    transaction = transaction.with_selection(Selection::new(ranges, selection.primary_index()));

    doc.apply(&transaction, view_id);
}

// o inserts a new line after each line with a selection
fn open_below(cx: &mut Context) {
    open(cx, Open::Below, CommentContinuation::Enabled)
}

// O inserts a new line before each line with a selection
fn open_above(cx: &mut Context) {
    open(cx, Open::Above, CommentContinuation::Enabled)
}

fn normal_mode(cx: &mut Context) {
    cx.editor.enter_normal_mode();
}

fn goto_line_without_jumplist(
    editor: &mut Editor,
    count: Option<NonZeroUsize>,
    movement: Movement,
) {
    if let Some(count) = count {
        let (view_id, doc) = focused!(editor);
        let doc_id = doc.id();
        helix_view::commands::movement::goto_line_without_jumplist(
            editor,
            view_id,
            doc_id,
            count.get(),
            movement,
        );
    }
}

fn goto_last_accessed_file(cx: &mut Context) {
    let view = view_mut!(cx.editor);
    if let Some(alt) = view.docs_access_history.pop() {
        cx.editor.switch(alt, Action::Replace);
    } else {
        cx.editor.set_error("no last accessed buffer")
    }
}

fn goto_last_modification(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let pos = doc.with_history_mut(|history| history.last_edit_pos());
    let text = doc.text().slice(..);
    if let Some(pos) = pos {
        let selection = doc
            .selection(view_id)
            .clone()
            .transform(|range| range.put_cursor(text, pos, cx.editor.mode == Mode::Select));
        let view = view_mut!(cx.editor, view_id);
        helix_view::view::push_jump(view, doc);
        doc.set_selection(view_id, selection);
    }
}

fn goto_last_modified_file(cx: &mut Context) {
    let view = view!(cx.editor);
    let alternate_file = view
        .last_modified_docs
        .into_iter()
        .flatten()
        .find(|&id| id != view.doc);
    if let Some(alt) = alternate_file {
        cx.editor.switch(alt, Action::Replace);
    } else {
        cx.editor.set_error("no last modified buffer")
    }
}

fn exit_select_mode(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    helix_view::commands::editing::exit_select_mode(cx.editor, view_id, doc_id);
}

fn goto_first_diag(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let selection = match doc.diagnostics().first() {
        Some(diag) => Selection::single(diag.range.start, diag.range.end),
        None => return,
    };
    let view = view_mut!(cx.editor, view_id);
    helix_view::view::push_jump(view, doc);
    doc.set_selection(view_id, selection);
    view.diagnostics_handler
        .immediately_show_diagnostic(doc, view_id);
}

fn goto_last_diag(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let selection = match doc.diagnostics().last() {
        Some(diag) => Selection::single(diag.range.start, diag.range.end),
        None => return,
    };
    let view = view_mut!(cx.editor, view_id);
    helix_view::view::push_jump(view, doc);
    doc.set_selection(view_id, selection);
    view.diagnostics_handler
        .immediately_show_diagnostic(doc, view_id);
}

fn goto_next_diag(cx: &mut Context) {
    let motion = move |editor: &mut Editor| {
        let (view_id, doc) = focused!(editor);

        let cursor_pos = doc
            .selection(view_id)
            .primary()
            .cursor(doc.text().slice(..));

        let diag = doc
            .diagnostics()
            .iter()
            .find(|diag| diag.range.start > cursor_pos);

        let selection = match diag {
            Some(diag) => Selection::single(diag.range.start, diag.range.end),
            None => return,
        };
        let view = view_mut!(editor, view_id);
        helix_view::view::push_jump(view, doc);
        doc.set_selection(view_id, selection);
        view.diagnostics_handler
            .immediately_show_diagnostic(doc, view_id);
    };

    cx.editor.apply_motion(motion);
}

fn goto_prev_diag(cx: &mut Context) {
    let motion = move |editor: &mut Editor| {
        let (view_id, doc) = focused!(editor);

        let cursor_pos = doc
            .selection(view_id)
            .primary()
            .cursor(doc.text().slice(..));

        let diag = doc
            .diagnostics()
            .iter()
            .rev()
            .find(|diag| diag.range.start < cursor_pos);

        let selection = match diag {
            // NOTE: the selection is reversed because we're jumping to the
            // previous diagnostic.
            Some(diag) => Selection::single(diag.range.end, diag.range.start),
            None => return,
        };
        let view = view_mut!(editor, view_id);
        helix_view::view::push_jump(view, doc);
        doc.set_selection(view_id, selection);
        view.diagnostics_handler
            .immediately_show_diagnostic(doc, view_id);
    };
    cx.editor.apply_motion(motion)
}

fn goto_first_change(cx: &mut Context) {
    goto_first_change_impl(cx, false);
}

fn goto_last_change(cx: &mut Context) {
    goto_first_change_impl(cx, true);
}

fn goto_first_change_impl(cx: &mut Context, reverse: bool) {
    let editor = &mut cx.editor;
    let (view_id, doc) = focused!(editor);
    if let Some(handle) = doc.diff_handle() {
        let hunk = {
            let diff = handle.load();
            let idx = if reverse {
                diff.len().saturating_sub(1)
            } else {
                0
            };
            diff.nth_hunk(idx)
        };
        if hunk != Hunk::NONE {
            let range = hunk_range(hunk, doc.text().slice(..));
            let view = view_mut!(editor, view_id);
            helix_view::view::push_jump(view, doc);
            doc.set_selection(view_id, Selection::single(range.anchor, range.head));
        }
    }
}

fn goto_next_change(cx: &mut Context) {
    goto_next_change_impl(cx, Direction::Forward)
}

fn goto_prev_change(cx: &mut Context) {
    goto_next_change_impl(cx, Direction::Backward)
}

fn goto_next_change_impl(cx: &mut Context, direction: Direction) {
    let count = cx.count() as u32 - 1;
    let motion = move |editor: &mut Editor| {
        let (view_id, doc) = focused!(editor);
        let doc_text = doc.text().slice(..);
        let diff_handle = if let Some(diff_handle) = doc.diff_handle() {
            diff_handle
        } else {
            editor.set_status("Diff is not available in current buffer");
            return;
        };

        let selection = doc.selection(view_id).clone().transform(|range| {
            let cursor_line = range.cursor_line(doc_text) as u32;

            let diff = diff_handle.load();
            let hunk_idx = match direction {
                Direction::Forward => diff
                    .next_hunk(cursor_line)
                    .map(|idx| (idx + count).min(diff.len() - 1)),
                Direction::Backward => diff
                    .prev_hunk(cursor_line)
                    .map(|idx| idx.saturating_sub(count)),
            };
            let Some(hunk_idx) = hunk_idx else {
                return range;
            };
            let hunk = diff.nth_hunk(hunk_idx);
            let new_range = hunk_range(hunk, doc_text);
            if editor.mode == Mode::Select {
                let head = if new_range.head < range.anchor {
                    new_range.anchor
                } else {
                    new_range.head
                };

                Range::new(range.anchor, head)
            } else {
                new_range.with_direction(direction)
            }
        });

        let view = view_mut!(editor, view_id);
        helix_view::view::push_jump(view, doc);
        doc.set_selection(view_id, selection)
    };
    cx.editor.apply_motion(motion);
}

/// Returns the [Range] for a [Hunk] in the given text.
/// Additions and modifications cover the added and modified ranges.
/// Deletions are represented as the point at the start of the deletion hunk.
fn hunk_range(hunk: Hunk, text: RopeSlice) -> Range {
    let anchor = text.line_to_char(hunk.after.start as usize);
    let head = if hunk.after.is_empty() {
        anchor + 1
    } else {
        text.line_to_char(hunk.after.end as usize)
    };

    Range::new(anchor, head)
}

pub mod insert {
    use crate::{handlers::local, key};

    use super::*;
    pub type Hook = fn(&Rope, &Selection, char) -> Option<Transaction>;

    use helix_view::editor::SmartTabConfig;

    pub fn insert_char(cx: &mut Context, c: char) {
        let (view_id, doc) = focused!(cx.editor);
        let doc_id = doc.id();
        if let Some(t) =
            helix_view::commands::editing::insert_char_transaction(cx.editor, view_id, doc_id, c)
        {
            let doc = doc_mut!(cx.editor, &doc_id);
            doc.apply(&t, view_id);
        }
        local::post_insert_char(c, cx);
    }

    pub fn smart_tab(cx: &mut Context) {
        let (view_id, doc) = focused_ref!(cx.editor);

        if matches!(
            cx.editor.config().smart_tab,
            Some(SmartTabConfig { enable: true, .. })
        ) {
            let cursors_after_whitespace = doc.selection(view_id).ranges().iter().all(|range| {
                let cursor = range.cursor(doc.text().slice(..));
                let current_line_num = doc.text().char_to_line(cursor);
                let current_line_start = doc.text().line_to_char(current_line_num);
                let left = doc.text().slice(current_line_start..cursor);
                left.chars().all(|c| c.is_whitespace())
            });

            if !cursors_after_whitespace {
                if doc.has_active_snippet() {
                    goto_next_tabstop(cx);
                } else {
                    let (view_id, doc) = focused!(cx.editor);
                    let doc_id = doc.id();
                    helix_view::commands::editing::move_node_bound(
                        cx.editor,
                        view_id,
                        doc_id,
                        Direction::Forward,
                        Movement::Move,
                    );
                }
                return;
            }
        }

        insert_tab(cx);
    }

    pub fn insert_tab(cx: &mut Context) {
        let (view_id, doc) = focused!(cx.editor);
        let doc_id = doc.id();
        helix_view::commands::editing::insert_tab(cx.editor, view_id, doc_id, 1);
    }

    fn insert_tab_impl(cx: &mut Context, count: usize) {
        let (view_id, doc) = focused!(cx.editor);
        let doc_id = doc.id();
        helix_view::commands::editing::insert_tab(cx.editor, view_id, doc_id, count);
    }

    pub fn append_char_interactive(cx: &mut Context) {
        // Save the current mode, so we can restore it later.
        let mode = cx.editor.mode;
        append_mode(cx);
        insert_selection_interactive(cx, mode);
    }

    pub fn insert_char_interactive(cx: &mut Context) {
        let mode = cx.editor.mode;
        insert_mode(cx);
        insert_selection_interactive(cx, mode);
    }

    fn insert_selection_interactive(cx: &mut Context, old_mode: Mode) {
        let count = cx.count();

        // need to wait for next key
        cx.on_next_key(move |cx, event| {
            match event {
                KeyEvent {
                    code: KeyCode::Char(ch),
                    ..
                } => {
                    for _ in 0..count {
                        insert::insert_char(cx, ch)
                    }
                }
                key!(Enter) => {
                    if count != 1 {
                        cx.editor
                            .set_error("inserting multiple newlines not yet supported");
                        return;
                    }
                    insert_newline(cx)
                }
                key!(Tab) => insert_tab_impl(cx, count),
                _ => (),
            };
            // Restore the old mode.
            cx.editor.mode = old_mode;
        });
    }

    pub fn insert_newline(cx: &mut Context) {
        let (view_id, doc) = focused!(cx.editor);
        let doc_id = doc.id();
        helix_view::commands::editing::insert_newline(cx.editor, view_id, doc_id);
    }

    pub fn delete_char_backward(cx: &mut Context) {
        let (view_id, doc) = focused!(cx.editor);
        let doc_id = doc.id();
        helix_view::commands::editing::delete_char_backward(cx.editor, view_id, doc_id, cx.count());
    }

    pub fn delete_char_forward(cx: &mut Context) {
        let (view_id, doc) = focused!(cx.editor);
        let doc_id = doc.id();
        helix_view::commands::editing::delete_char_forward(cx.editor, view_id, doc_id, cx.count());
    }

    pub fn delete_word_backward(cx: &mut Context) {
        let (view_id, doc) = focused!(cx.editor);
        let doc_id = doc.id();
        helix_view::commands::editing::delete_word_backward(cx.editor, view_id, doc_id, cx.count());
    }

    pub fn delete_word_forward(cx: &mut Context) {
        let (view_id, doc) = focused!(cx.editor);
        let doc_id = doc.id();
        helix_view::commands::editing::delete_word_forward(cx.editor, view_id, doc_id, cx.count());
    }
}

// Undo / Redo

// Yank / Paste

fn yank_to_clipboard(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    helix_view::commands::editing::yank(cx.editor, view_id, doc_id, '+');
    exit_select_mode(cx);
}

fn yank_to_primary_clipboard(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    helix_view::commands::editing::yank(cx.editor, view_id, doc_id, '*');
    exit_select_mode(cx);
}

fn yank_joined_impl(editor: &mut Editor, separator: &str, register: char) {
    let (view_id, doc) = focused!(editor);
    let text = doc.text().slice(..);

    let selection = doc.selection(view_id);
    let selections = selection.len();
    let joined = selection
        .fragments(text)
        .fold(String::new(), |mut acc, fragment| {
            if !acc.is_empty() {
                acc.push_str(separator);
            }
            acc.push_str(&fragment);
            acc
        });

    match editor.registers.write(register, vec![joined]) {
        Ok(_) => editor.set_status(format!(
            "joined and yanked {selections} selection{} to register {register}",
            if selections == 1 { "" } else { "s" }
        )),
        Err(err) => editor.set_error(err.to_string()),
    }
}

fn yank_joined_to_clipboard(cx: &mut Context) {
    let separator = focused_ref!(cx.editor).1.line_ending().as_str();
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    helix_view::commands::editing::yank_joined(cx.editor, view_id, doc_id, '+', separator);
    exit_select_mode(cx);
}

fn yank_joined_to_primary_clipboard(cx: &mut Context) {
    let separator = focused_ref!(cx.editor).1.line_ending().as_str();
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    helix_view::commands::editing::yank_joined(cx.editor, view_id, doc_id, '*', separator);
    exit_select_mode(cx);
}

fn yank_primary_selection_impl(editor: &mut Editor, register: char) {
    let (view_id, doc) = focused!(editor);
    let text = doc.text().slice(..);

    let selection = doc.selection(view_id).primary().fragment(text).to_string();

    match editor.registers.write(register, vec![selection]) {
        Ok(_) => editor.set_status(format!("yanked primary selection to register {register}",)),
        Err(err) => editor.set_error(err.to_string()),
    }
}

fn yank_main_selection_to_clipboard(cx: &mut Context) {
    yank_primary_selection_impl(cx.editor, '+');
    exit_select_mode(cx);
}

fn yank_main_selection_to_primary_clipboard(cx: &mut Context) {
    yank_primary_selection_impl(cx.editor, '*');
    exit_select_mode(cx);
}

#[derive(Copy, Clone)]
enum Paste {
    Before,
    After,
    Cursor,
}

static LINE_ENDING_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\r\n|\r|\n").unwrap());

fn paste_impl(
    values: &[String],
    doc: &mut Document,
    view: &mut View,
    action: Paste,
    count: usize,
    mode: Mode,
) {
    if values.is_empty() {
        return;
    }

    if mode == Mode::Insert {
        doc.append_changes_to_history(view);
    }

    // if any of values ends with a line ending, it's linewise paste
    let linewise = values
        .iter()
        .any(|value| get_line_ending_of_str(value).is_some());

    let map_value = |value| {
        let value = LINE_ENDING_REGEX.replace_all(value, doc.line_ending().as_str());
        let mut out = Tendril::from(value.as_ref());
        for _ in 1..count {
            out.push_str(&value);
        }
        out
    };

    let repeat = std::iter::repeat(
        // `values` is asserted to have at least one entry above.
        map_value(values.last().unwrap()),
    );

    let mut values = values.iter().map(|value| map_value(value)).chain(repeat);

    let text = doc.text();
    let selection = doc.selection(view.id);
    let annotations = view.fold_annotations(doc);

    let mut offset = 0;
    let mut ranges = SmallVec::with_capacity(selection.len());

    let mut transaction = Transaction::change_by_selection(text, selection, |range| {
        let pos = match (action, linewise) {
            // paste linewise before
            (Paste::Before, true) => text.line_to_char(text.char_to_line(range.from())),
            // paste linewise after
            (Paste::After, true) => {
                let line = range.line_range(text.slice(..)).1;
                text.line_to_char(text.slice(..).next_folded_line(&annotations, line))
            }
            // paste insert
            (Paste::Before, false) => range.from(),
            // paste append
            (Paste::After, false) => text.slice(..).next_folded_char(
                &annotations,
                prev_grapheme_boundary(text.slice(..), range.to()),
            ),
            // paste at cursor
            (Paste::Cursor, _) => range.cursor(text.slice(..)),
        };

        let value = values.next();

        let value_len = value
            .as_ref()
            .map(|content| content.chars().count())
            .unwrap_or_default();
        let anchor = offset + pos;

        let new_range = Range::new(anchor, anchor + value_len).with_direction(range.direction());
        ranges.push(new_range);
        offset += value_len;

        (pos, pos, value)
    });

    if mode == Mode::Normal {
        transaction = transaction.with_selection(Selection::new(ranges, selection.primary_index()));
    }

    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
}

pub(crate) fn paste_bracketed_value(cx: &mut Context, contents: String) {
    let count = cx.count();
    let paste = match cx.editor.mode {
        Mode::Insert | Mode::Select => Paste::Cursor,
        Mode::Normal => Paste::Before,
    };
    let (view_id, doc) = focused!(cx.editor);
    let view = view_mut!(cx.editor, view_id);
    paste_impl(&[contents], doc, view, paste, count, cx.editor.mode);
    exit_select_mode(cx);
}

fn replace_with_yanked_impl(editor: &mut Editor, register: char, count: usize) {
    let Some(values) = editor
        .registers
        .read(register, editor)
        .filter(|values| values.len() > 0)
    else {
        return;
    };
    let scrolloff = editor.config().scrolloff;
    let (view_id, doc) = focused_ref!(editor);

    let map_value = |value: &Cow<str>| {
        let value = LINE_ENDING_REGEX.replace_all(value, doc.line_ending().as_str());
        let mut out = Tendril::from(value.as_ref());
        for _ in 1..count {
            out.push_str(&value);
        }
        out
    };
    let mut values_rev = values.rev().peekable();
    // `values` is asserted to have at least one entry above.
    let last = values_rev.peek().unwrap();
    let repeat = std::iter::repeat(map_value(last));
    let mut values = values_rev
        .rev()
        .map(|value| map_value(&value))
        .chain(repeat);
    let selection = doc.selection(view_id);
    let transaction = Transaction::change_by_selection(doc.text(), selection, |range| {
        if !range.is_empty() {
            (range.from(), range.to(), Some(values.next().unwrap()))
        } else {
            (range.from(), range.to(), None)
        }
    });
    drop(values);

    let (view_id, doc) = focused!(editor);
    let view = view_mut!(editor, view_id);
    doc.apply(&transaction, view_id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);
}

fn replace_selections_with_clipboard(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    helix_view::commands::editing::replace_with_yanked(cx.editor, view_id, doc_id, '+', cx.count());
    exit_select_mode(cx);
}

fn replace_selections_with_primary_clipboard(cx: &mut Context) {
    replace_with_yanked_impl(cx.editor, '*', cx.count());
    exit_select_mode(cx);
}

fn paste(editor: &mut Editor, register: char, pos: Paste, count: usize) {
    let Some(values) = editor.registers.read(register, editor) else {
        return;
    };
    let values: Vec<_> = values.map(|value| value.to_string()).collect();

    let (view_id, doc) = focused!(editor);
    let view = view_mut!(editor, view_id);
    paste_impl(&values, doc, view, pos, count, editor.mode);
}

fn format_selections(cx: &mut Context) {
    use helix_lsp::{lsp, util::range_to_lsp_range};

    let (view_id, doc) = focused!(cx.editor);

    // via lsp if available
    // TODO: else via tree-sitter indentation calculations

    if doc.selection(view_id).len() != 1 {
        cx.editor
            .set_error("format_selections only supports a single selection for now");
        return;
    }

    // TODO extra LanguageServerFeature::FormatSelections?
    // maybe such that LanguageServerFeature::Format contains it as well
    let Some(language_server) = doc
        .language_servers_with_feature(LanguageServerFeature::Format)
        .find(|ls| {
            matches!(
                ls.capabilities().document_range_formatting_provider,
                Some(lsp::OneOf::Left(true) | lsp::OneOf::Right(_))
            )
        })
    else {
        cx.editor
            .set_error("No configured language server supports range formatting");
        return;
    };

    let offset_encoding = language_server.offset_encoding();
    let ranges: Vec<lsp::Range> = doc
        .selection(view_id)
        .iter()
        .map(|range| range_to_lsp_range(doc.text(), *range, offset_encoding))
        .collect();

    // TODO: handle fails
    // TODO: concurrent map over all ranges

    let range = ranges[0];

    let future = language_server
        .text_document_range_formatting(
            doc.identifier(),
            range,
            lsp::FormattingOptions {
                tab_size: doc.tab_width() as u32,
                insert_spaces: matches!(doc.indent_style(), IndentStyle::Spaces(_)),
                ..Default::default()
            },
            None,
        )
        .unwrap();

    let text = doc.text().clone();
    let doc_id = doc.id();
    let doc_version = doc.version();
    let ingress = cx.ingress.clone();

    cx.editor
        .work()
        .spawn(async move {
            match future.await {
                Ok(Some(res)) => {
                    let transaction = helix_lsp::util::generate_transaction_from_edits(
                        &text,
                        res,
                        offset_encoding,
                    );
                    send_task_event_with(
                        RuntimeTaskEvent::ApplyTransactionIfCurrent {
                            doc_id,
                            view_id,
                            expected_version: doc_version,
                            transaction,
                        },
                        ingress,
                    )
                    .await
                }
                Err(err) => log::error!("format sections failed: {err}"),
                Ok(None) => (),
            }
        })
        .detach();
}

fn keep_or_remove_selections_impl(cx: &mut Context, remove: bool) {
    // keep or remove selections matching regex
    let reg = cx.register.unwrap_or('/');
    ui::regex_prompt(
        cx,
        if remove { "remove:" } else { "keep:" }.into(),
        Some(reg),
        ui::completers::none,
        move |cx, regex, event| {
            let (view_id, doc) = focused!(cx.editor);
            if !matches!(event, PromptEvent::Update | PromptEvent::Validate) {
                return;
            }
            let text = doc.text().slice(..);

            if let Some(selection) =
                selection::keep_or_remove_matches(text, doc.selection(view_id), &regex, remove)
            {
                doc.set_selection(view_id, selection);
            } else if event == PromptEvent::Validate {
                cx.editor.set_error("no selections remaining");
            }
        },
    )
}

fn keep_selections(cx: &mut Context) {
    keep_or_remove_selections_impl(cx, false)
}

fn remove_selections(cx: &mut Context) {
    keep_or_remove_selections_impl(cx, true)
}

pub fn completion(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let range = doc.selection(view_id).primary();
    let text = doc.text().slice(..);
    let cursor = range.cursor(text);

    cx.editor
        .handlers
        .trigger_completions(cursor, doc.id(), view_id);
}

fn save_selection(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    helix_view::commands::editing::save_selection(cx.editor, view_id, doc_id);
}

fn rotate_view(cx: &mut Context) {
    cx.editor.focus_next()
}

fn rotate_view_reverse(cx: &mut Context) {
    cx.editor.focus_prev()
}

fn jump_view_right(cx: &mut Context) {
    cx.editor.focus_direction(tree::Direction::Right)
}

fn jump_view_left(cx: &mut Context) {
    cx.editor.focus_direction(tree::Direction::Left)
}

fn jump_view_up(cx: &mut Context) {
    cx.editor.focus_direction(tree::Direction::Up)
}

fn jump_view_down(cx: &mut Context) {
    cx.editor.focus_direction(tree::Direction::Down)
}

fn swap_view_right(cx: &mut Context) {
    cx.editor.swap_split_in_direction(tree::Direction::Right)
}

fn swap_view_left(cx: &mut Context) {
    cx.editor.swap_split_in_direction(tree::Direction::Left)
}

fn swap_view_up(cx: &mut Context) {
    cx.editor.swap_split_in_direction(tree::Direction::Up)
}

fn swap_view_down(cx: &mut Context) {
    cx.editor.swap_split_in_direction(tree::Direction::Down)
}

fn transpose_view(cx: &mut Context) {
    cx.editor.transpose_view()
}

/// Open a new split in the given direction specified by the action.
///
/// Maintain the current view (both the cursor's position and view in document).
fn split(editor: &mut Editor, action: Action) {
    let (view_id, doc) = focused!(editor);
    let id = doc.id();
    let selection = doc.selection(view_id).clone();
    let offset = doc.view_offset(view_id);
    let container = doc.fold_container(view_id).cloned();

    editor.switch(id, action);

    // match the selection in the previous view
    let (view_id, doc) = focused!(editor);
    if let Some(container) = container {
        doc.insert_fold_container(view_id, container);
    }
    doc.set_selection(view_id, selection);
    // match the view scroll offset (switch doesn't handle this fully
    // since the selection is only matched after the split)
    doc.set_view_offset(view_id, offset);
}

fn hsplit(cx: &mut Context) {
    split(cx.editor, Action::HorizontalSplit);
}

fn hsplit_new(cx: &mut Context) {
    cx.editor.new_file(Action::HorizontalSplit);
}

fn vsplit(cx: &mut Context) {
    split(cx.editor, Action::VerticalSplit);
}

fn vsplit_new(cx: &mut Context) {
    cx.editor.new_file(Action::VerticalSplit);
}

fn wclose(cx: &mut Context) {
    if cx.editor.has_single_view() {
        if let Err(err) = typed::buffers_remaining_impl(cx.editor) {
            cx.editor.set_error(err.to_string());
            return;
        }
    }
    let view_id = view!(cx.editor).id;
    // close current split
    cx.editor.close(view_id);
}

fn wonly(cx: &mut Context) {
    let views = cx
        .editor
        .tree
        .views()
        .map(|(v, focus)| (v.id, focus))
        .collect::<Vec<_>>();
    for (view_id, focus) in views {
        if !focus {
            cx.editor.close(view_id);
        }
    }
}

fn select_register(cx: &mut Context) {
    cx.editor.autoinfo = Some(Info::from_registers(
        "Select register",
        &cx.editor.registers,
    ));
    cx.on_next_key(move |cx, event| {
        cx.editor.autoinfo = None;
        if let Some(ch) = event.char() {
            cx.register = Some(ch);
        }
    })
}

fn insert_register(cx: &mut Context) {
    cx.editor.autoinfo = Some(Info::from_registers(
        "Insert register",
        &cx.editor.registers,
    ));
    cx.on_next_key(move |cx, event| {
        cx.editor.autoinfo = None;
        if let Some(ch) = event.char() {
            cx.register = Some(ch);
            paste(
                cx.editor,
                cx.register
                    .unwrap_or(cx.editor.config().default_yank_register),
                Paste::Cursor,
                cx.count(),
            );
        }
    })
}

fn copy_between_registers(cx: &mut Context) {
    cx.editor.autoinfo = Some(Info::from_registers(
        "Copy from register",
        &cx.editor.registers,
    ));
    cx.on_next_key(move |cx, event| {
        cx.editor.autoinfo = None;

        let Some(source) = event.char() else {
            return;
        };

        let Some(values) = cx.editor.registers.read(source, cx.editor) else {
            cx.editor.set_error(format!("register {source} is empty"));
            return;
        };
        let values: Vec<_> = values.map(|value| value.to_string()).collect();

        cx.editor.autoinfo = Some(Info::from_registers(
            "Copy into register",
            &cx.editor.registers,
        ));
        cx.on_next_key(move |cx, event| {
            cx.editor.autoinfo = None;

            let Some(dest) = event.char() else {
                return;
            };

            let n_values = values.len();
            match cx.editor.registers.write(dest, values) {
                Ok(_) => cx.editor.set_status(format!(
                    "yanked {n_values} value{} from register {source} to {dest}",
                    if n_values == 1 { "" } else { "s" }
                )),
                Err(err) => cx.editor.set_error(err.to_string()),
            }
        });
    });
}

fn goto_ts_object_impl(cx: &mut Context, object: &'static str, direction: Direction) {
    let count = cx.count();
    let (view_id, doc) = focused!(cx.editor);
    let doc_id = doc.id();
    cx.editor.apply_motion(move |editor: &mut Editor| {
        helix_view::commands::movement::goto_ts_object(
            editor, view_id, doc_id, object, direction, count,
        );
    });
}

fn goto_next_xml_element(cx: &mut Context) {
    goto_ts_object_impl(cx, "xml-element", Direction::Forward)
}

fn goto_prev_xml_element(cx: &mut Context) {
    goto_ts_object_impl(cx, "xml-element", Direction::Backward)
}

fn select_textobject_inside_type(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Inside, "class");
}

fn select_textobject_around_type(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Around, "class");
}

fn select_textobject_inside_function(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Inside, "function");
}

fn select_textobject_around_function(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Around, "function");
}

fn select_textobject_inside_parameter(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Inside, "parameter");
}

fn select_textobject_around_parameter(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Around, "parameter");
}

fn select_textobject_inside_comment(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Inside, "comment");
}

fn select_textobject_around_comment(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Around, "comment");
}

fn select_textobject_inside_test(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Inside, "test");
}

fn select_textobject_around_test(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Around, "test");
}

fn select_textobject_inside_entry(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Inside, "entry");
}

fn select_textobject_around_entry(cx: &mut Context) {
    textobject_treesitter(cx, textobject::TextObject::Around, "entry");
}

fn textobject_treesitter(
    cx: &mut Context,
    obj_type: textobject::TextObject,
    object_name: &'static str,
) {
    let count = cx.count();
    cx.editor.apply_motion(move |editor: &mut Editor| {
        let (view_id, doc) = focused!(editor);
        let doc_id = doc.id();
        helix_view::commands::editing::textobject_treesitter(
            editor,
            view_id,
            doc_id,
            obj_type,
            object_name,
            count,
        );
    });
}

fn select_textobject_inside_paragraph(cx: &mut Context) {
    textobject_paragraph(cx, textobject::TextObject::Inside);
}

fn select_textobject_around_paragraph(cx: &mut Context) {
    textobject_paragraph(cx, textobject::TextObject::Around);
}

fn textobject_paragraph(cx: &mut Context, textobject: textobject::TextObject) {
    let count = cx.count();
    cx.editor.apply_motion(move |editor: &mut Editor| {
        let (view_id, doc) = focused!(editor);
        let doc_id = doc.id();
        helix_view::commands::editing::textobject_paragraph(
            editor, view_id, doc_id, textobject, count,
        );
    });
}

fn select_textobject_inside_closest_surrounding_pair(cx: &mut Context) {
    textobject_closest_surrounding_pair(cx, textobject::TextObject::Inside);
}

fn select_textobject_around_closest_surrounding_pair(cx: &mut Context) {
    textobject_closest_surrounding_pair(cx, textobject::TextObject::Around);
}

fn textobject_closest_surrounding_pair(cx: &mut Context, textobject: textobject::TextObject) {
    let count = cx.count();
    cx.editor.apply_motion(move |editor: &mut Editor| {
        let (view_id, doc) = focused!(editor);
        let doc_id = doc.id();
        helix_view::commands::editing::textobject_closest_surrounding_pair(
            editor, view_id, doc_id, textobject, count,
        );
    });
}

fn select_textobject_inside_word(cx: &mut Context) {
    textobject_word(cx, textobject::TextObject::Inside, false);
}

fn select_textobject_around_word(cx: &mut Context) {
    textobject_word(cx, textobject::TextObject::Around, false);
}

#[allow(non_snake_case)]
fn select_textobject_inside_WORD(cx: &mut Context) {
    textobject_word(cx, textobject::TextObject::Inside, true);
}

#[allow(non_snake_case)]
fn select_textobject_around_WORD(cx: &mut Context) {
    textobject_word(cx, textobject::TextObject::Around, true);
}

fn textobject_word(cx: &mut Context, textobject: textobject::TextObject, longword: bool) {
    let count = cx.count();
    cx.editor.apply_motion(move |editor: &mut Editor| {
        let (view_id, doc) = focused!(editor);
        let doc_id = doc.id();
        helix_view::commands::editing::textobject_word(
            editor, view_id, doc_id, textobject, count, longword,
        );
    });
}

fn select_textobject_inside_change(cx: &mut Context) {
    textobject_change(cx);
}

fn select_textobject_around_change(cx: &mut Context) {
    textobject_change(cx);
}

fn textobject_change(cx: &mut Context) {
    cx.editor.apply_motion(|editor: &mut Editor| {
        let (view_id, doc) = focused!(editor);
        let doc_id = doc.id();
        helix_view::commands::editing::textobject_change(editor, view_id, doc_id);
    });
}

static SURROUND_HELP_TEXT: [(&str, &str); 6] = [
    ("m", "Nearest matching pair"),
    ("( or )", "Parentheses"),
    ("{ or }", "Curly braces"),
    ("< or >", "Angled brackets"),
    ("[ or ]", "Square brackets"),
    (" ", "... or any character"),
];

fn surround_add(cx: &mut Context) {
    cx.on_next_key(move |cx, event| {
        cx.editor.autoinfo = None;
        let (view_id, doc) = focused!(cx.editor);
        // surround_len is the number of new characters being added.
        let (open, close, surround_len) = match event.char() {
            Some(ch) => {
                let (o, c) = match_brackets::get_pair(ch);
                let mut open = Tendril::new();
                open.push(o);
                let mut close = Tendril::new();
                close.push(c);
                (open, close, 2)
            }
            None if event.code == KeyCode::Enter => (
                doc.line_ending().as_str().into(),
                doc.line_ending().as_str().into(),
                2 * doc.line_ending().len_chars(),
            ),
            None => return,
        };

        let selection = doc.selection(view_id);
        let mut changes = Vec::with_capacity(selection.len() * 2);
        let mut ranges = SmallVec::with_capacity(selection.len());
        let mut offs = 0;

        for range in selection.iter() {
            changes.push((range.from(), range.from(), Some(open.clone())));
            changes.push((range.to(), range.to(), Some(close.clone())));

            ranges.push(
                Range::new(offs + range.from(), offs + range.to() + surround_len)
                    .with_direction(range.direction()),
            );

            offs += surround_len;
        }

        let transaction = Transaction::change(doc.text(), changes.into_iter())
            .with_selection(Selection::new(ranges, selection.primary_index()));
        doc.apply(&transaction, view_id);
        exit_select_mode(cx);
    });

    cx.editor.autoinfo = Some(Info::new(
        "Surround selections with",
        &SURROUND_HELP_TEXT[1..],
    ));
}

fn surround_replace(cx: &mut Context) {
    let count = cx.count();
    cx.on_next_key(move |cx, event| {
        cx.editor.autoinfo = None;
        let surround_ch = match event.char() {
            Some('m') => None, // m selects the closest surround pair
            Some(ch) => Some(ch),
            None => return,
        };
        let (view_id, doc) = focused!(cx.editor);
        let selection = doc.selection(view_id);

        let change_pos = match doc.surround_positions(view_id, surround_ch, count) {
            Ok(c) => c,
            Err(err) => {
                cx.editor.set_error(err.to_string());
                return;
            }
        };

        let selection = selection.clone();
        let ranges: SmallVec<[Range; 1]> = change_pos.iter().map(|&p| Range::point(p)).collect();
        doc.set_selection(
            view_id,
            Selection::new(ranges, selection.primary_index() * 2),
        );

        cx.on_next_key(move |cx, event| {
            cx.editor.autoinfo = None;
            let (view_id, doc) = focused!(cx.editor);
            let to = match event.char() {
                Some(to) => to,
                None => return doc.set_selection(view_id, selection),
            };
            let (open, close) = match_brackets::get_pair(to);

            // the changeset has to be sorted to allow nested surrounds
            let mut sorted_pos: Vec<(usize, char)> = Vec::new();
            for p in change_pos.chunks(2) {
                sorted_pos.push((p[0], open));
                sorted_pos.push((p[1], close));
            }
            sorted_pos.sort_unstable();

            let transaction = Transaction::change(
                doc.text(),
                sorted_pos.iter().map(|&pos| {
                    let mut t = Tendril::new();
                    t.push(pos.1);
                    (pos.0, pos.0 + 1, Some(t))
                }),
            );
            doc.set_selection(view_id, selection);
            doc.apply(&transaction, view_id);
            exit_select_mode(cx);
        });

        cx.editor.autoinfo = Some(Info::new(
            "Replace with a pair of",
            &SURROUND_HELP_TEXT[1..],
        ));
    });

    cx.editor.autoinfo = Some(Info::new(
        "Replace surrounding pair of",
        &SURROUND_HELP_TEXT,
    ));
}

fn surround_delete(cx: &mut Context) {
    let count = cx.count();
    cx.on_next_key(move |cx, event| {
        cx.editor.autoinfo = None;
        let surround_ch = match event.char() {
            Some('m') => None, // m selects the closest surround pair
            Some(ch) => Some(ch),
            None => return,
        };
        let (view_id, doc) = focused!(cx.editor);

        let mut change_pos = match doc.surround_positions(view_id, surround_ch, count) {
            Ok(c) => c,
            Err(err) => {
                cx.editor.set_error(err.to_string());
                return;
            }
        };
        change_pos.sort_unstable(); // the changeset has to be sorted to allow nested surrounds
        let transaction =
            Transaction::change(doc.text(), change_pos.into_iter().map(|p| (p, p + 1, None)));
        doc.apply(&transaction, view_id);
        exit_select_mode(cx);
    });

    cx.editor.autoinfo = Some(Info::new("Delete surrounding pair of", &SURROUND_HELP_TEXT));
}

#[derive(Eq, PartialEq)]
enum ShellBehavior {
    Replace,
    Ignore,
    Insert,
    Append,
}

fn shell_pipe(cx: &mut Context) {
    shell_prompt_for_behavior(cx, "pipe:".into(), ShellBehavior::Replace);
}

fn shell_pipe_to(cx: &mut Context) {
    shell_prompt_for_behavior(cx, "pipe-to:".into(), ShellBehavior::Ignore);
}

fn shell_insert_output(cx: &mut Context) {
    shell_prompt_for_behavior(cx, "insert-output:".into(), ShellBehavior::Insert);
}

fn shell_append_output(cx: &mut Context) {
    shell_prompt_for_behavior(cx, "append-output:".into(), ShellBehavior::Append);
}

fn shell_keep_pipe(cx: &mut Context) {
    shell_prompt(cx, "keep-pipe:".into(), |cx, args| {
        let shell = &cx.editor.config().shell;
        let (view_id, doc) = focused!(cx.editor);
        let selection = doc.selection(view_id);

        let mut ranges = SmallVec::with_capacity(selection.len());
        let old_index = selection.primary_index();
        let mut index: Option<usize> = None;
        let text = doc.text().slice(..);

        for (i, range) in selection.ranges().iter().enumerate() {
            let fragment = range.slice(text);
            if let Err(err) = shell_impl(shell, args.join(" ").as_str(), Some(fragment.into())) {
                log::debug!("Shell command failed: {}", err);
            } else {
                ranges.push(*range);
                if i >= old_index && index.is_none() {
                    index = Some(ranges.len() - 1);
                }
            }
        }

        if ranges.is_empty() {
            cx.editor.set_error("No selections remaining");
            return;
        }

        let index = index.unwrap_or_else(|| ranges.len() - 1);
        doc.set_selection(view_id, Selection::new(ranges, index));
    });
}

fn shell_impl(shell: &[String], cmd: &str, input: Option<Rope>) -> anyhow::Result<Tendril> {
    tokio::task::block_in_place(|| helix_lsp::block_on(shell_impl_async(shell, cmd, input)))
}

async fn shell_impl_async(
    shell: &[String],
    cmd: &str,
    input: Option<Rope>,
) -> anyhow::Result<Tendril> {
    use std::process::Stdio;
    use tokio::process::Command;
    ensure!(!shell.is_empty(), "No shell set");

    let mut process = Command::new(&shell[0]);
    process
        .args(&shell[1..])
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if input.is_some() || cfg!(windows) {
        process.stdin(Stdio::piped());
    } else {
        process.stdin(Stdio::null());
    }

    let mut process = match process.spawn() {
        Ok(process) => process,
        Err(e) => {
            log::error!("Failed to start shell: {}", e);
            return Err(e.into());
        }
    };
    let output = if let Some(mut stdin) = process.stdin.take() {
        let write_input = async move {
            if let Some(input) = input {
                helix_view::document::to_writer(&mut stdin, (encoding::UTF_8, false), &input)
                    .await?;
            }
            anyhow::Ok(())
        };
        let (output, input_result) = tokio::join!(process.wait_with_output(), write_input);
        input_result?;
        output?
    } else {
        // Process has no stdin, so we just take the output
        process.wait_with_output().await?
    };

    let output = if !output.status.success() {
        if output.stderr.is_empty() {
            match output.status.code() {
                Some(exit_code) => bail!("Shell command failed: status {}", exit_code),
                None => bail!("Shell command failed"),
            }
        }
        String::from_utf8_lossy(&output.stderr)
        // Prioritize `stderr` output over `stdout`
    } else if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!("Command printed to stderr: {stderr}");
        stderr
    } else {
        String::from_utf8_lossy(&output.stdout)
    };

    Ok(Tendril::from(output))
}

fn shell(cx: &mut compositor::Context, cmd: &str, behavior: &ShellBehavior) {
    let pipe = match behavior {
        ShellBehavior::Replace | ShellBehavior::Ignore => true,
        ShellBehavior::Insert | ShellBehavior::Append => false,
    };

    let config = cx.editor.config();
    let shell = &config.shell;
    let (view_id, doc) = focused!(cx.editor);
    let selection = doc.selection(view_id);

    let mut changes = Vec::with_capacity(selection.len());
    let mut ranges = SmallVec::with_capacity(selection.len());
    let text = doc.text().slice(..);

    let mut shell_output: Option<Tendril> = None;
    let mut offset = 0isize;
    for range in selection.ranges() {
        let output = if let Some(output) = shell_output.as_ref() {
            output.clone()
        } else {
            let input = range.slice(text);
            match shell_impl(shell, cmd, pipe.then(|| input.into())) {
                Ok(mut output) => {
                    if !input.ends_with("\n") && output.ends_with('\n') {
                        output.pop();
                        if output.ends_with('\r') {
                            output.pop();
                        }
                    }

                    if !pipe {
                        shell_output = Some(output.clone());
                    }
                    output
                }
                Err(err) => {
                    cx.editor.set_error(err.to_string());
                    return;
                }
            }
        };

        let output_len = output.chars().count();

        let (from, to, deleted_len) = match behavior {
            ShellBehavior::Replace => (range.from(), range.to(), range.len()),
            ShellBehavior::Insert => (range.from(), range.from(), 0),
            ShellBehavior::Append => (range.to(), range.to(), 0),
            _ => (range.from(), range.from(), 0),
        };

        // These `usize`s cannot underflow because selection ranges cannot overlap.
        let anchor = to
            .checked_add_signed(offset)
            .expect("Selection ranges cannot overlap")
            .checked_sub(deleted_len)
            .expect("Selection ranges cannot overlap");
        let new_range = Range::new(anchor, anchor + output_len).with_direction(range.direction());
        ranges.push(new_range);
        offset = offset
            .checked_add_unsigned(output_len)
            .expect("Selection ranges cannot overlap")
            .checked_sub_unsigned(deleted_len)
            .expect("Selection ranges cannot overlap");

        changes.push((from, to, Some(output)));
    }

    if behavior != &ShellBehavior::Ignore {
        let transaction = Transaction::change(doc.text(), changes.into_iter())
            .with_selection(Selection::new(ranges, selection.primary_index()));
        doc.apply(&transaction, view_id);
        let view = view_mut!(cx.editor, view_id);
        doc.append_changes_to_history(view);
    }

    // after replace cursor may be out of bounds, do this to
    // make sure cursor is in view and update scroll as well
    let view = view_mut!(cx.editor, view_id);
    view.ensure_cursor_in_view(doc, config.scrolloff);
}

fn shell_prompt<F>(cx: &mut Context, prompt: Cow<'static, str>, mut callback_fn: F)
where
    F: FnMut(&mut compositor::Context, Args) + Send + 'static,
{
    ui::prompt(
        cx,
        prompt,
        Some('|'),
        |editor, input| complete_command_args(editor, SHELL_SIGNATURE, &SHELL_COMPLETER, input, 0),
        move |cx, input, event| {
            if event != PromptEvent::Validate || input.is_empty() {
                return;
            }
            match Args::parse(input, SHELL_SIGNATURE, true, |token| {
                expansion::expand(cx.editor, token).map_err(|err| err.into())
            }) {
                Ok(args) => callback_fn(cx, args),
                Err(err) => cx.editor.set_error(err.to_string()),
            }
        },
    );
}

fn shell_prompt_for_behavior(cx: &mut Context, prompt: Cow<'static, str>, behavior: ShellBehavior) {
    shell_prompt(cx, prompt, move |cx, args| {
        shell(cx, args.join(" ").as_str(), &behavior)
    })
}

fn suspend(_cx: &mut Context) {
    #[cfg(not(windows))]
    {
        // SAFETY: These are calls to standard POSIX functions.
        // Unsafe is necessary since we are calling outside of Rust.
        let is_session_leader = unsafe { libc::getpid() == libc::getsid(0) };

        // If helix is the session leader, there is nothing to suspend to, so skip
        if is_session_leader {
            return;
        }
        _cx.block_try_flush_writes().ok();
        signal_hook::low_level::raise(signal_hook::consts::signal::SIGTSTP).unwrap();
    }
}

fn goto_next_tabstop(cx: &mut Context) {
    goto_next_tabstop_impl(cx, Direction::Forward)
}

fn goto_prev_tabstop(cx: &mut Context) {
    goto_next_tabstop_impl(cx, Direction::Backward)
}

fn goto_next_tabstop_impl(cx: &mut Context, direction: Direction) {
    let (view_id, doc) = focused!(cx.editor);
    let Some(mut snippet) = doc.take_active_snippet() else {
        cx.editor.set_error("no snippet is currently active");
        return;
    };
    let tabstop = match direction {
        Direction::Forward => Some(snippet.next_tabstop(doc.selection(view_id))),
        Direction::Backward => snippet
            .prev_tabstop(doc.selection(view_id))
            .map(|selection| (selection, false)),
    };
    let Some((selection, last_tabstop)) = tabstop else {
        return;
    };
    doc.set_selection(view_id, selection);
    if !last_tabstop {
        doc.set_active_snippet(snippet)
    }
    if cx.editor.mode() == Mode::Insert {
        cx.on_next_key_fallback(|cx, key| {
            if let Some(c) = key.char() {
                let (view_id, doc) = focused!(cx.editor);
                if let Some(snippet) = doc.active_snippet() {
                    doc.apply(&snippet.delete_placeholder(doc.text()), view_id);
                }
                insert_char(cx, c);
            }
        })
    }
}

fn record_macro(cx: &mut Context) {
    if let Some((reg, mut keys)) = cx.editor.macro_recording.take() {
        // Remove the keypress which ends the recording
        keys.pop();
        let s = keys
            .into_iter()
            .map(|key| {
                let s = key.to_string();
                if s.chars().count() == 1 {
                    s
                } else {
                    format!("<{}>", s)
                }
            })
            .collect::<String>();
        match cx.editor.registers.write(reg, vec![s]) {
            Ok(_) => cx
                .editor
                .set_status(format!("Recorded to register [{}]", reg)),
            Err(err) => cx.editor.set_error(err.to_string()),
        }
    } else {
        let reg = cx.register.take().unwrap_or('@');
        cx.editor.macro_recording = Some((reg, Vec::new()));
        cx.editor
            .set_status(format!("Recording to register [{}]", reg));
    }
}

fn replay_macro(cx: &mut Context) {
    let reg = cx.register.unwrap_or('@');

    if cx.editor.macro_replaying.contains(&reg) {
        cx.editor.set_error(format!(
            "Cannot replay from register [{}] because already replaying from same register",
            reg
        ));
        return;
    }

    let keys: Vec<KeyEvent> = if let Some(keys) = cx
        .editor
        .registers
        .read(reg, cx.editor)
        .filter(|values| values.len() == 1)
        .map(|mut values| values.next().unwrap())
    {
        match helix_view::input::parse_macro(&keys) {
            Ok(keys) => keys,
            Err(err) => {
                cx.editor.set_error(format!("Invalid macro: {}", err));
                return;
            }
        }
    } else {
        cx.editor.set_error(format!("Register [{}] empty", reg));
        return;
    };

    // Once the macro has been fully validated, it's marked as being under replay
    // to ensure we don't fall into infinite recursion.
    cx.editor.macro_replaying.push(reg);

    let count = cx.count();
    cx.callback.push(crate::compositor::PostAction::ReplayKeys {
        keys: keys.to_vec(),
        count,
        pop_macro_replaying: true,
    });
}

fn goto_word(cx: &mut Context) {
    jump_to_word(cx, Movement::Move)
}

fn extend_to_word(cx: &mut Context) {
    jump_to_word(cx, Movement::Extend)
}

fn jump_to_label(cx: &mut Context, labels: Vec<Range>, behaviour: Movement) {
    let (_, doc) = focused_ref!(cx.editor);
    let alphabet = &cx.editor.config().jump_label_alphabet;
    if labels.is_empty() {
        return;
    }
    let alphabet_char = |i| {
        let mut res = Tendril::new();
        res.push(alphabet[i]);
        res
    };

    // Add label for each jump candidate to the View as virtual text.
    let text = doc.text().slice(..);
    let mut overlays: Vec<_> = labels
        .iter()
        .enumerate()
        .flat_map(|(i, range)| {
            // Prefer "lower" chars of the given alphabeth.
            // Use all possible combinations of lower chars before extending
            // the used subset upwards.
            // E.g., "abc..." will lead to label sequence
            // "aa", "ba", "ab", "bb", "ca", "cb", "ac", "bc", "cc", ...
            // Labels are generated in a square manner as illustrated by the
            // schematic below.
            //    a  b  c  d
            // a  0  1  4  9
            // b  2  3  5 10
            // c  6  7  8 11
            // d 12 13 14 15
            // The column index determines the leading (outer) char,
            // the row index determines the trailing (inner) char.
            let base = (i as f64).sqrt() as usize;
            let offset = i - base * base;

            let outer = if offset < base { base } else { offset - base };
            let inner = if offset < base { offset } else { base };
            [
                Overlay::new(range.from(), alphabet_char(outer)),
                Overlay::new(
                    graphemes::next_grapheme_boundary(text, range.from()),
                    alphabet_char(inner),
                ),
            ]
        })
        .collect();
    overlays.sort_unstable_by_key(|overlay| overlay.char_idx);
    let (view_id, doc) = focused!(cx.editor);
    doc.set_jump_labels(view_id, overlays);

    // Accept two characters matching a visible label. Jump to the candidate
    // for that label if it exists.
    let primary_selection = doc.selection(view_id).primary();
    let view = view_id;
    let doc = doc.id();
    cx.on_next_key(move |cx, event| {
        let alphabet = &cx.editor.config().jump_label_alphabet;
        let Some(outer) = event
            .char()
            .filter(|_| event.modifiers.is_empty())
            .and_then(|ch| alphabet.iter().position(|&it| it == ch))
        else {
            doc_mut!(cx.editor, &doc).remove_jump_labels(view);
            return;
        };

        cx.on_next_key(move |cx, event| {
            doc_mut!(cx.editor, &doc).remove_jump_labels(view);
            let alphabet = &cx.editor.config().jump_label_alphabet;
            let Some(inner) = event
                .char()
                .filter(|_| event.modifiers.is_empty())
                .and_then(|ch| alphabet.iter().position(|&it| it == ch))
            else {
                return;
            };
            // Mapping back a label to an index requires to distinguish 2 cases
            // (see label generation above for illustration):
            // 1. We are in the new column
            //    => size of inner square + pos in column.
            // 2. We are in the new row (including corner)
            //    => size of extended inner square + pos in row.
            let index = if outer > inner {
                outer * outer + inner
            } else {
                inner * (inner + 1) + outer
            };
            if let Some(mut range) = labels.get(index).copied() {
                range = if behaviour == Movement::Extend {
                    let anchor = if range.anchor < range.head {
                        let from = primary_selection.from();
                        if range.anchor < from {
                            range.anchor
                        } else {
                            from
                        }
                    } else {
                        let to = primary_selection.to();
                        if range.anchor > to {
                            range.anchor
                        } else {
                            to
                        }
                    };
                    Range::new(anchor, range.head)
                } else {
                    range.with_direction(Direction::Forward)
                };
                save_selection(cx);
                doc_mut!(cx.editor, &doc).set_selection(view, range.into());
            }
        });
    });
}

fn jump_to_word(cx: &mut Context, behaviour: Movement) {
    // Calculate the jump candidates: ranges for any visible words with two or
    // more characters.
    let alphabet = &cx.editor.config().jump_label_alphabet;
    let jump_label_limit = alphabet.len() * alphabet.len();
    let mut words = Vec::with_capacity(jump_label_limit);
    let (view_id, doc) = focused_ref!(cx.editor);
    let view = view!(cx.editor, view_id);
    let text = doc.text().slice(..);
    let annotations = view.text_annotations(doc, None);

    // This is not necessarily exact if there is virtual text like soft wrap.
    // It's ok though because the extra jump labels will not be rendered.
    let start = text.line_to_char(text.char_to_line(doc.view_offset(view_id).anchor));
    let end = text.line_to_char(view.estimate_last_doc_line(&annotations, doc) + 1);

    let primary_selection = doc.selection(view_id).primary();
    let cursor = primary_selection.cursor(text);
    let mut cursor_fwd = Range::point(cursor);
    let mut cursor_rev = Range::point(cursor);
    if text.get_char(cursor).is_some_and(|c| !c.is_whitespace()) {
        let cursor_word_end = movement::move_next_word_end(text, &annotations, cursor_fwd, 1);
        //  single grapheme words need a special case
        if cursor_word_end.anchor == cursor {
            cursor_fwd = cursor_word_end;
        }
        let cursor_word_start = movement::move_prev_word_start(text, &annotations, cursor_rev, 1);
        if cursor_word_start.anchor == next_grapheme_boundary(text, cursor) {
            cursor_rev = cursor_word_start;
        }
    }
    'outer: loop {
        let mut changed = false;
        while cursor_fwd.head < end {
            cursor_fwd = movement::move_next_word_end(text, &annotations, cursor_fwd, 1);
            // The cursor is on a word that is atleast two graphemes long and
            // madeup of word characters. The latter condition is needed because
            // move_next_word_end simply treats a sequence of characters from
            // the same char class as a word so `=<` would also count as a word.
            let add_label = text
                .slice(..cursor_fwd.head)
                .graphemes_rev()
                .take(2)
                .take_while(|g| g.chars().all(char_is_word))
                .count()
                == 2;
            if !add_label {
                continue;
            }
            changed = true;
            // skip any leading whitespace
            cursor_fwd.anchor += text
                .chars_at(cursor_fwd.anchor)
                .take_while(|&c| !char_is_word(c))
                .count();
            words.push(cursor_fwd);
            if words.len() == jump_label_limit {
                break 'outer;
            }
            break;
        }
        while cursor_rev.head > start {
            cursor_rev = movement::move_prev_word_start(text, &annotations, cursor_rev, 1);
            // The cursor is on a word that is atleast two graphemes long and
            // madeup of word characters. The latter condition is needed because
            // move_prev_word_start simply treats a sequence of characters from
            // the same char class as a word so `=<` would also count as a word.
            let add_label = text
                .slice(cursor_rev.head..)
                .graphemes()
                .take(2)
                .take_while(|g| g.chars().all(char_is_word))
                .count()
                == 2;
            if !add_label {
                continue;
            }
            changed = true;
            cursor_rev.anchor -= text
                .chars_at(cursor_rev.anchor)
                .reversed()
                .take_while(|&c| !char_is_word(c))
                .count();
            words.push(cursor_rev);
            if words.len() == jump_label_limit {
                break 'outer;
            }
            break;
        }
        if !changed {
            break;
        }
    }
    drop(annotations);
    jump_to_label(cx, words, behaviour)
}

fn lsp_or_syntax_symbol_picker(cx: &mut Context) {
    let (_, doc) = focused_ref!(cx.editor);

    if doc
        .language_servers_with_feature(LanguageServerFeature::DocumentSymbols)
        .next()
        .is_some()
    {
        lsp::symbol_picker(cx);
    } else if doc.has_syntax() {
        syntax_symbol_picker(cx);
    } else {
        cx.editor
            .set_error("No language server supporting document symbols or syntax info available");
    }
}

fn lsp_or_syntax_workspace_symbol_picker(cx: &mut Context) {
    let (_, doc) = focused_ref!(cx.editor);

    if doc
        .language_servers_with_feature(LanguageServerFeature::WorkspaceSymbols)
        .next()
        .is_some()
    {
        lsp::workspace_symbol_picker(cx);
    } else {
        syntax_workspace_symbol_picker(cx);
    }
}

fn fold(cx: &mut Context) {
    let command: MappableCommand = ":fold --all".parse().unwrap();
    command.execute(cx);
}

fn unfold(cx: &mut Context) {
    let command: MappableCommand = ":unfold --all".parse().unwrap();
    command.execute(cx);
}

fn toggle_fold(cx: &mut Context) {
    use graphemes::ensure_grapheme_boundary_prev;
    use text_folding::{Fold, FoldObject};

    let (view_id, doc) = focused!(cx.editor);
    let text = doc.text().slice(..);
    let cursor = doc.selection(view_id).primary().cursor(text);
    let loader = cx.editor.syn_loader.load();

    if !doc.has_syntax() {
        cx.editor
            .set_error("Syntax is unavailable in the current buffer.");
        return;
    }
    let Some((syntax, textobject_query)) = doc.textobject_context(&loader) else {
        cx.editor.set_error("Failed to compile text object query.");
        return;
    };

    let textobjects = &[
        "class.around",
        "function.around",
        "comment.around",
        "test.around",
        "conditional.around",
        "loop.around",
    ];
    let root_node = syntax.tree().root_node();

    // search for a textobject at the cursor
    let Some((capture_name, node_range)) = textobject_query
        .capture_nodes_all(textobjects, &root_node, text)
        .map(|(cap, node)| (cap.name(textobject_query.query()), node.byte_range()))
        .filter(|(_, range)| range.contains(&text.char_to_byte(cursor)))
        .min_by_key(|(_, range)| range.len())
        .map(|(cap, range)| {
            (cap, {
                let start = text.byte_to_char(range.start);
                let end = ensure_grapheme_boundary_prev(text, text.byte_to_char(range.end - 1));
                start..=end
            })
        })
    else {
        cx.editor
            .set_status("There is no text object at the cursor.");
        return;
    };

    let object = {
        let textobject = match capture_name {
            "class.around" => "class",
            "function.around" => "function",
            "comment.around" => "comment",
            "test.around" => "test",
            "conditional.around" => "conditional",
            "loop.around" => "loop",
            other => unreachable!("Unexpected textobject {other}"),
        };
        FoldObject::TextObject(textobject)
    };

    let fold = doc.fold_container(view_id).and_then(|container| {
        container.find(&object, &node_range, |fold| fold.header()..=fold.end.target)
    });
    if let Some(fold) = fold {
        let view = view!(cx.editor, view_id);
        doc.remove_folds(view, &[fold.start_idx()]);
        return;
    }

    let header = *node_range.start();
    let target = {
        match capture_name {
            "class.around" | "function.around" | "test.around" | "conditional.around"
            | "loop.around" => {
                let capture = match capture_name {
                    "class.around" => "class.inside",
                    "function.around" => "function.inside",
                    "test.around" => "test.inside",
                    "conditional.around" => "conditional.inside",
                    "loop.around" => "loop.inside",
                    _ => unreachable!(),
                };
                let byte_range = {
                    let start = text.char_to_byte(*node_range.start());
                    let end = text.char_to_byte(next_grapheme_boundary(text, *node_range.end()));
                    start..end
                };
                let node = syntax
                    .descendant_for_byte_range(byte_range.start as u32, byte_range.end as u32)
                    .expect("The range must belong to the captured node.");
                let Some(target) = || -> Option<_> {
                    textobject_query
                        .capture_nodes(capture, &node, text)?
                        .next()
                        .map(|cap_node| {
                            let start = text.byte_to_char(cap_node.start_byte());
                            let end = ensure_grapheme_boundary_prev(
                                text,
                                text.byte_to_char(cap_node.end_byte() - 1),
                            );
                            start..=end
                        })
                }() else {
                    return;
                };
                target
            }
            "comment.around" => {
                let start_line = text.char_to_line(*node_range.start());
                let end_line = text.char_to_line(*node_range.end());
                if start_line >= end_line {
                    cx.editor.set_status("One-line comment does not fold.");
                    return;
                }
                let start = text.line_to_char(start_line + 1)
                    + text
                        .line(start_line + 1)
                        .first_non_whitespace_char()
                        .unwrap_or(0);
                start..=*node_range.end()
            }
            other => unreachable!("Unexpected textobject {other}"),
        }
    };

    let new_fold = Fold::new_points(text, object, header, &target);
    let view = view!(cx.editor, view_id);
    doc.add_folds(view, vec![new_fold]);
}
