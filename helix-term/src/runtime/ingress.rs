//! Typed events delivered from async/runtime work into the main application loop.
//!
//! Background and UI tasks send through [`helix_runtime::Sender`] owned by
//! [`crate::application::Application`]; the receiving side is drained in
//! `Application::event_loop_until_idle`.
//!
//! **Split:** [`RuntimeTaskEvent`] applies editor-side effects (often via [`crate::effect`]).
//! [`UiCommand`] drives compositor / layers / widgets. Keep that boundary when adding variants.

use std::collections::HashSet;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use helix_core::diagnostic::{DiagnosticProvider, Severity};
use helix_core::{Transaction, Uri};
use helix_dap::ThreadId as DebugThreadId;
use helix_lsp::{lsp, LanguageServerId};
use helix_runtime::{Sender as IngressSender, TimerId, Token};
use helix_view::document::DocumentInlayHintsId;
use helix_view::document::FormatterError;
use helix_view::handlers::lsp::{SignatureHelpInvoked, SignatureHelpRequestId};
use helix_view::{DocumentId, ViewId};

use super::ui::UiCommand;

/// Default capacity for the ingress mailbox (bounded backpressure).
pub const BOUND: usize = 1024;

#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub severity: Severity,
    pub message: Cow<'static, str>,
}

impl From<anyhow::Error> for StatusMessage {
    fn from(err: anyhow::Error) -> Self {
        Self {
            severity: Severity::Error,
            message: err.to_string().into(),
        }
    }
}

impl From<&'static str> for StatusMessage {
    fn from(message: &'static str) -> Self {
        Self {
            severity: Severity::Info,
            message: message.into(),
        }
    }
}

/// Application-level ingress from the `helix-runtime` stack (timers, tasks, redraw, status).
pub enum RuntimeEvent {
    /// Request a UI refresh (replaces ad hoc redraw hooks over time).
    Redraw,
    /// Show a transient status line message.
    Status { message: String, severity: Severity },
    /// A [`Clock`] timer fired.
    Timer(TimerId),
    /// Structured task notifications (completion, errors); extended in later phases.
    Task(RuntimeTaskEvent),
    /// Assistant permission decision returned from a frontend popup.
    AssistantPermissionResolved {
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::RequestId,
        decision: helix_view::assistant::permission::Decision,
    },
    /// Typed compositor command (UI thread).
    Ui(UiCommand),
}

impl std::fmt::Debug for RuntimeEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Redraw => f.write_str("Redraw"),
            Self::Status { message, severity } => f
                .debug_struct("Status")
                .field("message", message)
                .field("severity", severity)
                .finish(),
            Self::Timer(id) => f.debug_tuple("Timer").field(id).finish(),
            Self::Task(t) => f.debug_tuple("Task").field(t).finish(),
            Self::AssistantPermissionResolved {
                thread,
                request,
                decision,
            } => f
                .debug_struct("AssistantPermissionResolved")
                .field("thread", thread)
                .field("request", request)
                .field("decision", decision)
                .finish(),
            Self::Ui(cmd) => f.debug_tuple("Ui").field(cmd).finish(),
        }
    }
}

/// Editor-side and neutral main-thread effects (not compositor widget construction). Applied via [`crate::effect::apply_runtime_task_event`] and related editor-side effect helpers.
#[derive(Debug)]
pub enum RuntimeTaskEvent {
    /// No-op success (e.g. best-effort steps that did not need an effect).
    Stub,
    /// Apply a prepared transaction on the main loop if the document revision still matches.
    /// Used by async LSP work (e.g. range format) instead of untyped main-thread closures.
    ApplyTransactionIfCurrent {
        doc_id: DocumentId,
        view_id: ViewId,
        expected_version: i32,
        transaction: Transaction,
    },
    /// Dismiss a popup notification by id (timeout expiry path).
    DismissNotification { id: usize },
    /// Async formatter / LSP format finished; apply on the main loop if the revision matches.
    ApplyFormattingResult {
        doc_id: DocumentId,
        view_id: ViewId,
        expected_version: i32,
        format_result: Result<Transaction, FormatterError>,
        write: Option<(Option<PathBuf>, bool)>,
    },
    /// Show an error on the editor after async work (e.g. external URL open failed).
    SetEditorError { message: String },
    /// Attach document color swatches on the main thread.
    AttachDocumentColors {
        doc_id: DocumentId,
        colors: Vec<(usize, lsp::Color)>,
    },
    /// Apply LSP pull-diagnostics report on the main thread.
    PullDiagnosticsResponse {
        doc_id: DocumentId,
        uri: Uri,
        provider: DiagnosticProvider,
        result: lsp::DocumentDiagnosticReportResult,
    },
    /// Re-run pull diagnostics for a document subset after LSP `retrigger_request` (post-delay).
    RetryPullDiagnostics {
        doc_id: DocumentId,
        language_servers: HashSet<LanguageServerId>,
    },
    /// Debounced document color refresh (after [`DocumentColorsHandler`] debounce).
    RequestDocumentColorsDebounced { doc_ids: HashSet<DocumentId> },
    /// Debounced pull diagnostics for listed documents.
    PullDiagnosticsDebounced { document_ids: HashSet<DocumentId> },
    /// Debounced pull diagnostics for inter-file dependency language servers.
    PullAllDocumentsDiagnosticsDebounced {
        language_servers: HashSet<LanguageServerId>,
    },
    /// Debounced LSP signature request (after [`SignatureHelpHandler`] debounce).
    RequestSignatureDebounced {
        invoked: SignatureHelpInvoked,
        request: SignatureHelpRequestId,
        cancel: Token,
    },
    /// Debounced blame fetch for statusline / inline blame.
    BlameFetchDebounced {
        doc_id: DocumentId,
        path: PathBuf,
        line: Option<u32>,
    },
    /// Apply document highlight selections on the editor main loop.
    SelectDocumentHighlights {
        offset_encoding: helix_lsp::OffsetEncoding,
        highlights: Vec<lsp::DocumentHighlight>,
    },
    /// Apply inlay hints to a document/view pair on the editor main loop.
    ApplyInlayHints {
        view_id: ViewId,
        doc_id: DocumentId,
        offset_encoding: helix_lsp::OffsetEncoding,
        id: DocumentInlayHintsId,
        hints: Vec<lsp::InlayHint>,
    },
    /// DAP restart completed; update editor state and status.
    DapRestarted,
    /// DAP resume/continue/step completed; resume application state.
    ResumeDebuggerApplication,
    /// DAP terminate completed; unset active debugger client.
    UnsetActiveDebugClient,
    /// DAP exception breakpoint configuration completed.
    DapExceptionsConfigured,
    /// Restore a persisted assistant thread record into editor-owned state.
    RestoreAssistantHistoryThread {
        record: helix_view::assistant::history::Record,
        activate: bool,
        open_panel: bool,
    },
    /// Activate an existing assistant thread in editor-owned state.
    ActivateAssistantThread {
        thread: helix_view::assistant::thread::Id,
        open_panel: bool,
    },
    /// Detach an assistant context item from the active thread.
    DetachAssistantContext {
        item: helix_view::assistant::context::Id,
    },
    /// Register a plugin panel in editor-owned layout state.
    RegisterPluginPanel {
        plugin_name: String,
        panel_id: String,
        title: String,
        side: String,
        width: u16,
        render_callback_id: u64,
        event_callback_id: Option<u64>,
    },
    /// Remove a plugin panel from editor-owned layout state.
    RemovePluginPanel {
        panel_id: String,
    },
    /// Deliver a plugin UI callback value to the plugin engine on the editor side.
    DeliverPluginUiCallback {
        plugin_name: String,
        callback_id: u64,
        value: serde_json::Value,
    },
    /// Remove the assistant panel model entry from editor-owned layout state.
    RemoveAssistantPanel,
    /// Connect an assistant backend and apply resulting editor-owned state.
    ConnectAssistantBackend {
        command: String,
        args: Vec<String>,
        open_panel: bool,
    },
    /// Cycle the active assistant thread and open the panel.
    CycleAssistantThread {
        delta: isize,
    },
    /// Close the active assistant thread and keep panel state in sync.
    CloseActiveAssistantThread,
    /// Create a new assistant thread from the active backend and open the panel.
    NewAssistantThreadFromActiveBackend,
    /// Toggle follow state on the active assistant thread.
    ToggleActiveAssistantFollow,
    /// Attach a context item to the active assistant thread.
    AttachAssistantContext {
        item: helix_view::assistant::context::Kind,
        status: &'static str,
    },
    /// Submit a prompt to the active assistant backend.
    SubmitAssistantPrompt {
        text: String,
    },
    /// Cancel the active assistant thread if one exists.
    CancelActiveAssistantThread,
    /// Open the selected assistant entry in a scratch document.
    OpenSelectedAssistantEntryScratch,
    /// Open the selected assistant turn changes if available.
    OpenSelectedAssistantTurnChanges,
    /// Open the active assistant thread changes if available.
    OpenActiveAssistantThreadChanges,
    /// Apply assistant history entries into editor-owned cache state.
    ApplyAssistantHistoryEntries {
        scope: helix_view::assistant::thread::Scope,
        entries: Vec<helix_view::assistant::history::Stub>,
    },
    /// Load an assistant history thread record for activation.
    LoadAssistantHistoryThread {
        thread: helix_view::assistant::thread::Id,
        activate: bool,
        open_panel: bool,
    },
    /// Bootstrap assistant history scope and persisted layout at application startup.
    BootstrapAssistantHistory {
        scope: helix_view::assistant::thread::Scope,
    },
    /// Switch the active DAP thread and fetch its stack trace.
    SelectDebugThread {
        thread_id: DebugThreadId,
        force: bool,
    },
    /// Pause a specific DAP thread.
    PauseDebugThread {
        thread_id: DebugThreadId,
    },
    /// Select a stack frame within the active debug thread.
    SelectStackFrame {
        thread_id: DebugThreadId,
        frame_id: usize,
    },
    /// Apply fetched stack frames for a debug thread on the main loop.
    ApplyStackFrames {
        thread_id: DebugThreadId,
        frames: Vec<helix_dap::StackFrame>,
        auto_select_first_frame: bool,
    },
    /// Execute an LSP command through the editor-owned main-thread path.
    ExecuteLspCommand {
        command: lsp::Command,
        server_id: LanguageServerId,
    },
    /// Apply a resolved LSP code action on the editor main loop.
    ApplyCodeAction {
        offset_encoding: helix_lsp::OffsetEncoding,
        workspace_edit: Option<lsp::WorkspaceEdit>,
        command: Option<lsp::Command>,
        server_id: LanguageServerId,
    },
    /// Update a DAP breakpoint condition and notify the active debugger.
    SetBreakpointCondition {
        path: PathBuf,
        index: usize,
        condition: Option<String>,
    },
    /// Update a DAP breakpoint log message and notify the active debugger.
    SetBreakpointLogMessage {
        path: PathBuf,
        index: usize,
        log_message: Option<String>,
    },
    /// Toggle a DAP breakpoint at a line and notify the active debugger.
    ToggleBreakpoint {
        path: PathBuf,
        line: usize,
    },
    /// Auto-save debounce finished (insert mode defers; else save).
    AutoSaveRun { save_pending: Arc<AtomicBool> },
    /// Auto-reload debounce finished (insert mode defers; else reload).
    AutoReloadRun { reload_pending: Arc<AtomicBool> },
}

// ---------------------------------------------------------------------------
// Ingress sender wiring.
// ---------------------------------------------------------------------------

/// Wire hook-error reporting into the application-owned ingress sender.
pub type StatusBridge = helix_event::ErrorReporterGuard;

pub fn install_status_bridge(ingress_tx: IngressSender<RuntimeEvent>) -> StatusBridge {
    helix_event::scoped_error_reporter(std::sync::Arc::new(move |err| {
        let message = StatusMessage::from(err);
        helix_runtime::send_blocking(
            &ingress_tx,
            RuntimeEvent::Status {
                message: message.message.into_owned(),
                severity: message.severity,
            },
        );
    }))
}

/// Send a typed [`RuntimeTaskEvent`] on the ingress channel (same semantics as [`send_ui_command_with`]).
pub async fn send_task_event_with(task: RuntimeTaskEvent, ingress: IngressSender<RuntimeEvent>) {
    let ev = RuntimeEvent::Task(task);
    let _ = ingress.send(ev).await;
}

/// Send a typed [`UiCommand`] on the ingress channel.
pub async fn send_ui_command_with(cmd: UiCommand, ingress: IngressSender<RuntimeEvent>) {
    let ev = RuntimeEvent::Ui(cmd);
    let _ = ingress.send(ev).await;
}

pub async fn send_redraw_with(ingress: IngressSender<RuntimeEvent>) {
    let event = RuntimeEvent::Redraw;
    let _ = ingress.send(event).await;
}

pub async fn send_status_message_with(
    message: impl Into<StatusMessage>,
    ingress: IngressSender<RuntimeEvent>,
) {
    let message = message.into();
    let event = RuntimeEvent::Status {
        message: message.message.into_owned(),
        severity: message.severity,
    };

    let _ = ingress.send(event).await;
}

pub fn spawn_task_event_with_future(
    work: helix_runtime::Work,
    future: impl std::future::Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
    ingress: IngressSender<RuntimeEvent>,
) {
    work.spawn(async move {
        match future.await {
            Ok(task) => send_task_event_with(task, ingress.clone()).await,
            Err(err) => send_status_message_with(err, ingress).await,
        }
    })
    .detach();
}

pub fn spawn_ui_command_with_future(
    work: helix_runtime::Work,
    future: impl std::future::Future<Output = anyhow::Result<UiCommand>> + Send + 'static,
    ingress: IngressSender<RuntimeEvent>,
) {
    work.spawn(async move {
        match future.await {
            Ok(cmd) => send_ui_command_with(cmd, ingress.clone()).await,
            Err(err) => send_status_message_with(err, ingress).await,
        }
    })
    .detach();
}


