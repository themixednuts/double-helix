//! Typed compositor-facing ingress (Phase 3).
//!
//! Group commands by domain so new UI re-entry stays structured.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::handlers::completion::{CompletionItem, CompletionResponse, LspCompletionItem, Trigger};
use crate::ui::prompt::Movement;
use helix_core::completion::CompletionProvider;
use helix_core::syntax::config::DebugConfigCompletion;
use helix_core::Syntax;
use helix_core::Uri;
use helix_dap::{StackFrame, Thread, ThreadId};
use helix_lsp::lsp;
use helix_lsp::LanguageServerId;
use helix_lsp::OffsetEncoding;
use helix_view::editor::AgentConfig;
use helix_view::handlers::completion::RequestId as CompletionRequestId;
use helix_view::handlers::completion::ResponseContext;
use helix_view::handlers::lsp::{SignatureHelpInvoked, SignatureHelpRequestId};

/// LSP text position for navigation (goto / picker); mirrors `lsp::Location` with `Uri` + encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLocation {
    pub uri: Uri,
    pub range: lsp::Range,
    pub offset_encoding: OffsetEncoding,
}

/// How to show multi-server hover content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspHoverDisplay {
    Popup,
    /// Open aggregated markdown in a scratch buffer (`goto_hover`).
    FileBuffer,
}

/// Menu vs list picker for code actions (`code_action` vs `code_action_picker`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspCodeActionPresentation {
    Menu,
    Picker,
}

/// One row for code-action menu/picker (async gather → typed UI).
#[derive(Debug, Clone)]
pub struct LspCodeActionItem {
    pub lsp_item: lsp::CodeActionOrCommand,
    pub language_server_id: LanguageServerId,
}

/// Document symbol picker row (`textDocument/documentSymbol`).
#[derive(Debug, Clone)]
pub struct DocumentSymbolPickerItem {
    pub location: LspLocation,
    pub symbol: lsp::SymbolInformation,
}

/// LSP-driven compositor UI (ingress schema; apply in [`super::apply`] + [`crate::ui::lsp`]).
#[derive(Debug)]
pub enum LspCommand {
    /// Picker or single-file jump; if `locations` is empty, show `empty_message`.
    Goto {
        locations: Vec<LspLocation>,
        empty_message: &'static str,
    },
    /// Hover popup or scratch buffer; empty `hovers` → status in apply.
    Hover {
        hovers: Vec<(String, lsp::Hover)>,
        display: LspHoverDisplay,
    },
    /// Code action menu or picker; empty `items` → error in apply.
    CodeActions {
        items: Vec<LspCodeActionItem>,
        presentation: LspCodeActionPresentation,
    },
    /// Document symbols picker; empty `symbols` → status in apply.
    DocumentSymbols {
        symbols: Vec<DocumentSymbolPickerItem>,
    },
    SignatureHelp {
        invoked: SignatureHelpInvoked,
        request: SignatureHelpRequestId,
        response: Option<lsp::SignatureHelp>,
    },
    PrepareRename {
        prefill: String,
        history_register: Option<char>,
        language_server_id: Option<LanguageServerId>,
    },
}

/// Completion-specific UI ingress (async completion list / resolve).
pub enum CompletionCommand {
    ApplyProviderResponse {
        request: CompletionRequestId,
        response: CompletionResponse,
        is_incomplete: bool,
    },
    ReplaceResolvedItem {
        previous: Arc<LspCompletionItem>,
        resolved: Box<CompletionItem>,
    },
    Show {
        request: CompletionRequestId,
        items: Vec<CompletionItem>,
        context: HashMap<CompletionProvider, ResponseContext>,
        trigger: Trigger,
    },
    /// Debounced completion request after [`CompletionHandler`] timeout.
    RequestDebounced { trigger: Trigger },
}

pub enum PickerCommand {
    RequestPreviewHighlight { path: PathBuf },
    ApplyPreviewSyntax { path: PathBuf, syntax: Syntax },
    RunDynamicQuery { query: Arc<str> },
}

impl std::fmt::Debug for PickerCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestPreviewHighlight { path } => f
                .debug_struct("RequestPreviewHighlight")
                .field("path", path)
                .finish(),
            Self::ApplyPreviewSyntax { path, .. } => f
                .debug_struct("ApplyPreviewSyntax")
                .field("path", path)
                .finish_non_exhaustive(),
            Self::RunDynamicQuery { query } => f
                .debug_struct("RunDynamicQuery")
                .field("query", query)
                .finish(),
        }
    }
}

impl std::fmt::Debug for CompletionCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApplyProviderResponse { .. } => f.write_str("ApplyProviderResponse(..)"),
            Self::ReplaceResolvedItem { .. } => f.write_str("ReplaceResolvedItem(..)"),
            Self::Show { .. } => f.write_str("Show(..)"),
            Self::RequestDebounced { .. } => f.write_str("RequestDebounced(..)"),
        }
    }
}

/// Debugger UI ingress (async → main thread; apply in [`crate::commands::dap::apply_dap_command`]).
pub enum DapCommand {
    /// Multi-field debug template parameter entry (`debug_parameter_prompt`).
    PushDebugParameterPrompt {
        completions: Vec<DebugConfigCompletion>,
        config_name: String,
        params: Vec<String>,
    },
    /// Edit breakpoint condition.
    PushBreakpointConditionPrompt {
        path: PathBuf,
        index: usize,
        initial: Option<String>,
    },
    /// Edit breakpoint log message.
    PushBreakpointLogPrompt {
        path: PathBuf,
        index: usize,
        initial: Option<String>,
    },
    /// `threads` response shown in a picker with a typed action.
    ThreadsPicker {
        threads: Vec<Thread>,
        action: DapThreadAction,
    },
    StackFramesPicker {
        thread_id: ThreadId,
        frames: Vec<StackFrame>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum DapThreadAction {
    Switch,
    Pause,
}

impl std::fmt::Debug for DapCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PushDebugParameterPrompt { .. } => f.write_str("PushDebugParameterPrompt(..)"),
            Self::PushBreakpointConditionPrompt { .. } => {
                f.write_str("PushBreakpointConditionPrompt(..)")
            }
            Self::PushBreakpointLogPrompt { .. } => f.write_str("PushBreakpointLogPrompt(..)"),
            Self::ThreadsPicker { threads, action } => f
                .debug_struct("ThreadsPicker")
                .field("threads", threads)
                .field("action", action)
                .finish_non_exhaustive(),
            Self::StackFramesPicker { thread_id, frames } => f
                .debug_struct("StackFramesPicker")
                .field("thread_id", thread_id)
                .field("frames", frames)
                .finish_non_exhaustive(),
        }
    }
}

/// Assistant panel UI ingress (apply in [`crate::commands::typed::apply_assistant_command`]).
#[derive(Debug, Clone)]
pub enum AssistantCommand {
    TogglePanelFocus,
    ClosePanel,
    FocusPanelInput,
    FocusPanelEntries,
    /// `:assistant-connect` with no args — pick from configured `[[editor.agents]]`.
    PushConfiguredAgentsPicker {
        agents: Vec<AgentConfig>,
    },
    /// Show assistant history entries using normalized history stubs.
    PushHistoryPicker {
        entries: Vec<helix_view::assistant::history::Stub>,
    },
    /// Show a picker for detaching one of several attached assistant context items.
    PushDetachContextPicker {
        items: Vec<helix_view::assistant::context::Item>,
    },
    ShowPermissionRequest {
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::Request,
    },
    /// Open the assistant panel shell; assistant data comes from editor-owned state.
    OpenPanel,
}

#[derive(Debug, Clone)]
pub enum FileExplorerCommand {
    RefreshPanel {
        root: PathBuf,
        cursor: u32,
    },
    PromptCreate {
        root: PathBuf,
        cursor: u32,
        prefill: String,
    },
    ConfirmCreate {
        root: PathBuf,
        cursor: u32,
        input: String,
        target: PathBuf,
    },
    PromptMove {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        prefill: String,
        movement: Option<Movement>,
    },
    ConfirmMove {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        input: String,
        destination: PathBuf,
    },
    PromptDelete {
        target: PathBuf,
        root: PathBuf,
        cursor: u32,
    },
    PromptCopy {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        prefill: String,
    },
    ConfirmCopy {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        input: String,
        destination: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub enum PluginCommand {
    Prompt {
        request: helix_plugin::contract::requests::PromptRequest,
        callback: helix_plugin::contract::UiCallbackToken,
    },
    Confirm {
        request: helix_plugin::contract::requests::ConfirmRequest,
        callback: helix_plugin::contract::UiCallbackToken,
    },
    Picker {
        request: helix_plugin::contract::requests::PickerRequest,
        callback: helix_plugin::contract::UiCallbackToken,
    },
    PushPanel {
        panel: helix_plugin::contract::PanelHandle,
    },
    RemovePanel {
        panel: helix_plugin::contract::PanelHandle,
    },
}

/// Top-level UI command delivered on the main thread via [`crate::runtime::ingress::RuntimeEvent::Ui`].
pub enum UiCommand {
    Layer(LayerCommand),
    Completion(Box<CompletionCommand>),
    Picker(PickerCommand),
    /// LSP navigation / overlays.
    Lsp(LspCommand),
    /// Async work completed with nothing to apply on the main loop.
    Nop,
    /// Full compositor redraw (e.g. after async prompt validation).
    NeedFullRedraw,
    /// Debugger prompts / overlays.
    Dap(DapCommand),
    /// Assistant panels / pickers.
    Assistant(AssistantCommand),
    /// File explorer prompts / confirmations.
    FileExplorer(FileExplorerCommand),
    /// Plugin-originated typed UI requests.
    Plugin(PluginCommand),
}

impl std::fmt::Debug for UiCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Layer(c) => f.debug_tuple("Layer").field(c).finish(),
            Self::Completion(c) => f.debug_tuple("Completion").field(c).finish(),
            Self::Picker(c) => f.debug_tuple("Picker").field(c).finish(),
            Self::Lsp(c) => match c {
                LspCommand::Goto { .. } => f.write_str("Lsp(Goto(..))"),
                LspCommand::Hover { .. } => f.write_str("Lsp(Hover(..))"),
                LspCommand::CodeActions { .. } => f.write_str("Lsp(CodeActions(..))"),
                LspCommand::DocumentSymbols { .. } => f.write_str("Lsp(DocumentSymbols(..))"),
                LspCommand::SignatureHelp { .. } => f.write_str("Lsp(SignatureHelp(..))"),
                LspCommand::PrepareRename { .. } => f.write_str("Lsp(PrepareRename(..))"),
            },
            Self::Nop => f.write_str("Nop"),
            Self::NeedFullRedraw => f.write_str("NeedFullRedraw"),
            Self::Dap(c) => f.debug_tuple("Dap").field(c).finish(),
            Self::Assistant(c) => f.debug_tuple("Acp").field(c).finish(),
            Self::FileExplorer(c) => f.debug_tuple("FileExplorer").field(c).finish(),
            Self::Plugin(c) => f.debug_tuple("Plugin").field(c).finish(),
        }
    }
}

/// Layer stack / overlay operations.
#[derive(Debug, Clone)]
pub enum LayerCommand {
    /// Show notification history in a popup (content derived from editor state at apply time).
    PushNotificationHistory,
    /// Regex compile failed in cmdline; show error in a small overlay.
    InvalidRegexPopup { message: String },
    /// Remove cmdline prompt overlay if present (focus lost).
    DismissPromptIfPresent,
    /// Markdown in a popup (`Markdown::new` uses editor `syn_loader` at apply time).
    MarkdownPopup {
        layer_id: &'static str,
        markdown: String,
    },
    /// Push the directory file picker rooted at `root` (cmdline `:open` on a directory).
    PushFilePicker { root: PathBuf },
    /// Picker to run an LSP command when multiple servers advertise the same command list.
    LspCommandPicker {
        commands: Vec<(LanguageServerId, lsp::Command)>,
    },
    /// `:run-shell-command` output: positioned markdown popup (if non-empty) + status.
    ShellRunOutput { output: String },
}
