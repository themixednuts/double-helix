//! Typed events delivered from async/runtime work into the main application loop.
//!
//! Background and UI tasks send through [`RuntimeIngress`] typed methods. The raw
//! mailbox event is private so producer code cannot enqueue arbitrary variants or
//! bypass the non-blocking/full-queue handoff policy.
//!
//! **Split:** [`RuntimeTaskEvent`] applies editor-side effects (often via [`crate::effect`]).
//! [`UiCommand`] drives compositor / layers / widgets. Keep that boundary when adding variants.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use helix_core::diagnostic::{DiagnosticProvider, Severity};
use helix_core::{Transaction, Uri};
use helix_dap::ThreadId as DebugThreadId;
use helix_lsp::{lsp, LanguageServerId};
use helix_runtime::DebouncedSender;
use helix_runtime::{Receiver as IngressReceiver, Sender as IngressSender, TimerId, Token, Work};
use helix_view::document::DocumentInlayHintsId;
use helix_view::document::FormatterError;
use helix_view::handlers::lsp::{
    LspFeatureRefreshKind, SignatureHelpInvoked, SignatureHelpRequestId,
};
use helix_view::{
    editor::{Activation, FrameSelection, PanelBehavior, SavePolicy, ThreadSelectPolicy},
    DocumentId, ViewId,
};

use super::ui::UiCommand;

/// Default capacity for the ingress mailbox (bounded backpressure).
pub const BOUND: usize = 1024;

#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub severity: Severity,
    pub message: Cow<'static, str>,
}

#[derive(Debug, Clone)]
pub struct PendingFormatWrite {
    pub path: Option<PathBuf>,
    pub policy: SavePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleRender {
    Defer,
    Immediate,
}

impl IdleRender {
    pub const fn should_render_immediately(self) -> bool {
        matches!(self, Self::Immediate)
    }
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

/// Application-level delivery from the `helix-runtime` stack (timers, tasks, status).
#[derive(Debug)]
pub enum RuntimeDelivery {
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

/// Private mailbox payload. Only [`RuntimeIngress`] can construct and queue this
/// type, which prevents callers from bypassing the typed ingress API.
struct RuntimeEvent(RuntimeDelivery);

impl std::fmt::Debug for RuntimeEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            RuntimeDelivery::Status { message, severity } => f
                .debug_struct("Status")
                .field("message", message)
                .field("severity", severity)
                .finish(),
            RuntimeDelivery::Timer(id) => f.debug_tuple("Timer").field(id).finish(),
            RuntimeDelivery::Task(t) => f.debug_tuple("Task").field(t).finish(),
            RuntimeDelivery::AssistantPermissionResolved {
                thread,
                request,
                decision,
            } => f
                .debug_struct("AssistantPermissionResolved")
                .field("thread", thread)
                .field("request", request)
                .field("decision", decision)
                .finish(),
            RuntimeDelivery::Ui(cmd) => f.debug_tuple("Ui").field(cmd).finish(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeIngress {
    tx: IngressSender<RuntimeEvent>,
    work: Work,
}

#[derive(Debug)]
pub struct RuntimeIngressReceiver {
    rx: IngressReceiver<RuntimeEvent>,
}

#[derive(Clone, Debug)]
pub struct RuntimeTaskDebouncer {
    inner: DebouncedSender<RuntimeEvent>,
}

#[derive(Clone, Debug)]
pub struct RuntimeUiDebouncer {
    inner: DebouncedSender<RuntimeEvent>,
}

impl RuntimeIngress {
    pub(crate) fn channel(work: Work) -> (Self, RuntimeIngressReceiver) {
        let (tx, rx) = helix_runtime::channel(BOUND);
        (Self { tx, work }, RuntimeIngressReceiver { rx })
    }

    fn enqueue(&self, event: RuntimeEvent) {
        match self.tx.try_send(event) {
            Ok(()) | Err(helix_runtime::TrySend::Closed(_)) => {}
            Err(helix_runtime::TrySend::Full(event)) => {
                let tx = self.tx.clone();
                self.work
                    .spawn(async move {
                        let _ = tx.send(event).await;
                    })
                    .detach();
            }
        }
    }

    async fn send(&self, event: RuntimeEvent) {
        let _ = self.tx.send(event).await;
    }

    pub fn status(&self, message: impl Into<StatusMessage>) {
        let message = message.into();
        self.enqueue(RuntimeEvent(RuntimeDelivery::Status {
            message: message.message.into_owned(),
            severity: message.severity,
        }));
    }

    pub async fn send_status(&self, message: impl Into<StatusMessage>) {
        let message = message.into();
        self.send(RuntimeEvent(RuntimeDelivery::Status {
            message: message.message.into_owned(),
            severity: message.severity,
        }))
        .await;
    }

    pub fn task(&self, task: RuntimeTaskEvent) {
        self.enqueue(RuntimeEvent(RuntimeDelivery::Task(task)));
    }

    pub async fn send_task(&self, task: RuntimeTaskEvent) {
        self.send(RuntimeEvent(RuntimeDelivery::Task(task))).await;
    }

    pub fn ui(&self, cmd: UiCommand) {
        self.enqueue(RuntimeEvent(RuntimeDelivery::Ui(cmd)));
    }

    pub async fn send_ui(&self, cmd: UiCommand) {
        self.send(RuntimeEvent(RuntimeDelivery::Ui(cmd))).await;
    }

    pub fn timer(&self, id: TimerId) {
        self.enqueue(RuntimeEvent(RuntimeDelivery::Timer(id)));
    }

    pub async fn send_timer(&self, id: TimerId) {
        self.send(RuntimeEvent(RuntimeDelivery::Timer(id))).await;
    }

    pub fn assistant_permission_resolved(
        &self,
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::RequestId,
        decision: helix_view::assistant::permission::Decision,
    ) {
        self.enqueue(RuntimeEvent(RuntimeDelivery::AssistantPermissionResolved {
            thread,
            request,
            decision,
        }));
    }

    pub async fn send_assistant_permission_resolved(
        &self,
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::RequestId,
        decision: helix_view::assistant::permission::Decision,
    ) {
        self.send(RuntimeEvent(RuntimeDelivery::AssistantPermissionResolved {
            thread,
            request,
            decision,
        }))
        .await;
    }

    pub fn task_debouncer(
        &self,
        delay: std::time::Duration,
        runtime: &helix_runtime::Runtime,
    ) -> RuntimeTaskDebouncer {
        RuntimeTaskDebouncer {
            inner: DebouncedSender::new(
                delay,
                runtime.work().clone(),
                runtime.clock().clone(),
                self.tx.clone(),
            ),
        }
    }

    pub fn ui_debouncer(
        &self,
        delay: std::time::Duration,
        runtime: &helix_runtime::Runtime,
    ) -> RuntimeUiDebouncer {
        RuntimeUiDebouncer {
            inner: DebouncedSender::new(
                delay,
                runtime.work().clone(),
                runtime.clock().clone(),
                self.tx.clone(),
            ),
        }
    }
}

impl RuntimeIngressReceiver {
    pub async fn recv(&mut self) -> Option<RuntimeDelivery> {
        self.rx.recv().await.map(|event| event.0)
    }

    pub fn try_recv(&mut self) -> Result<RuntimeDelivery, helix_runtime::TryRecvError> {
        self.rx.try_recv().map(|event| event.0)
    }
}

impl RuntimeTaskDebouncer {
    pub fn new(
        delay: std::time::Duration,
        work: Work,
        clock: helix_runtime::Clock,
        ingress: RuntimeIngress,
    ) -> Self {
        Self {
            inner: DebouncedSender::new(delay, work, clock, ingress.tx),
        }
    }

    pub fn send(&self, task: RuntimeTaskEvent) {
        self.inner.send(RuntimeEvent(RuntimeDelivery::Task(task)));
    }

    pub fn send_now(&self, task: RuntimeTaskEvent) {
        self.inner
            .send_now(RuntimeEvent(RuntimeDelivery::Task(task)));
    }

    pub fn send_after(&self, task: RuntimeTaskEvent, delay: std::time::Duration) {
        self.inner
            .send_after(RuntimeEvent(RuntimeDelivery::Task(task)), delay);
    }

    pub fn send_after_with(
        &self,
        delay: std::time::Duration,
        build: impl FnOnce() -> Option<RuntimeTaskEvent> + Send + 'static,
    ) {
        self.inner.send_after_with(delay, move || {
            build().map(RuntimeDelivery::Task).map(RuntimeEvent)
        });
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

impl RuntimeUiDebouncer {
    pub fn new(
        delay: std::time::Duration,
        work: Work,
        clock: helix_runtime::Clock,
        ingress: RuntimeIngress,
    ) -> Self {
        Self {
            inner: DebouncedSender::new(delay, work, clock, ingress.tx),
        }
    }

    pub fn send_after(&self, cmd: UiCommand, delay: std::time::Duration) {
        self.inner
            .send_after(RuntimeEvent(RuntimeDelivery::Ui(cmd)), delay);
    }

    pub fn send(&self, cmd: UiCommand) {
        self.inner.send(RuntimeEvent(RuntimeDelivery::Ui(cmd)));
    }

    pub fn send_now(&self, cmd: UiCommand) {
        self.inner.send_now(RuntimeEvent(RuntimeDelivery::Ui(cmd)));
    }

    pub fn send_after_with(
        &self,
        delay: std::time::Duration,
        build: impl FnOnce() -> Option<UiCommand> + Send + 'static,
    ) {
        self.inner.send_after_with(delay, move || {
            build().map(RuntimeDelivery::Ui).map(RuntimeEvent)
        });
    }

    pub fn cancel(&self) {
        self.inner.cancel();
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
        write: Option<PendingFormatWrite>,
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
    RequestLspFeaturesDebounced {
        docs: HashMap<DocumentId, HashSet<LspFeatureRefreshKind>>,
    },
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
    ApplyCodeLenses {
        doc_id: DocumentId,
        lenses: Vec<(LanguageServerId, helix_lsp::OffsetEncoding, lsp::CodeLens)>,
    },
    ApplyDocumentLinks {
        doc_id: DocumentId,
        links: Vec<(
            LanguageServerId,
            helix_lsp::OffsetEncoding,
            lsp::DocumentLink,
        )>,
    },
    ApplyFoldingRanges {
        doc_id: DocumentId,
        ranges: Vec<(
            LanguageServerId,
            helix_lsp::OffsetEncoding,
            lsp::FoldingRange,
        )>,
    },
    ApplyLinkedEditingRanges {
        offset_encoding: helix_lsp::OffsetEncoding,
        ranges: lsp::LinkedEditingRanges,
    },
    ApplyOnTypeFormatting {
        doc_id: DocumentId,
        view_id: ViewId,
        expected_version: i32,
        offset_encoding: helix_lsp::OffsetEncoding,
        edits: Vec<lsp::TextEdit>,
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
        record: Box<helix_view::assistant::history::Record>,
        activation: Activation,
        panel: PanelBehavior,
    },
    /// Activate an existing assistant thread in editor-owned state.
    ActivateAssistantThread {
        thread: helix_view::assistant::thread::Id,
        panel: PanelBehavior,
    },
    /// Detach an assistant context item from the active thread.
    DetachAssistantContext {
        item: helix_view::assistant::context::Id,
    },
    /// Deliver a plugin UI callback value to the plugin engine on the editor side.
    DeliverPluginUiCallback {
        callback: helix_plugin::contract::UiCallbackToken,
        value: helix_plugin::contract::DynamicValue,
    },
    /// Service an out-of-process plugin host request on the editor main thread.
    PluginHostRequest {
        state: crate::plugin_registry::RemoteHostState,
        request: helix_plugin::rpc::PluginRequest,
        respond_to: crate::plugin_registry::PluginHostResponder,
    },
    /// Remove the assistant panel model entry from editor-owned layout state.
    RemoveAssistantPanel,
    /// Connect an assistant backend and apply resulting editor-owned state.
    ConnectAssistantBackend {
        command: String,
        args: Vec<String>,
        panel: PanelBehavior,
    },
    /// Cycle the active assistant thread and open the panel.
    CycleAssistantThread { delta: isize },
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
    SubmitAssistantPrompt { text: String },
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
        activation: Activation,
        panel: PanelBehavior,
    },
    /// Bootstrap assistant history scope and persisted layout at application startup.
    BootstrapAssistantHistory {
        scope: helix_view::assistant::thread::Scope,
    },
    /// Switch the active DAP thread and fetch its stack trace.
    SelectDebugThread {
        thread_id: DebugThreadId,
        policy: ThreadSelectPolicy,
    },
    /// Pause a specific DAP thread.
    PauseDebugThread { thread_id: DebugThreadId },
    /// Select a stack frame within the active debug thread.
    SelectStackFrame {
        thread_id: DebugThreadId,
        frame_id: usize,
    },
    /// Apply fetched stack frames for a debug thread on the main loop.
    ApplyStackFrames {
        thread_id: DebugThreadId,
        frames: Vec<helix_dap::StackFrame>,
        selection: FrameSelection,
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
    ToggleBreakpoint { path: PathBuf, line: usize },
    /// Auto-save debounce finished (insert mode defers; else save).
    AutoSaveRun { save_pending: Arc<AtomicBool> },
    /// Auto-reload debounce finished (insert mode defers; else reload).
    AutoReloadRun { reload_pending: Arc<AtomicBool> },
}

pub fn status_error_reporter(
    ingress: RuntimeIngress,
) -> std::sync::Arc<dyn Fn(anyhow::Error) + Send + Sync> {
    std::sync::Arc::new(move |err| {
        let message = StatusMessage::from(err);
        ingress.status(message);
    })
}

/// Send a typed [`RuntimeTaskEvent`] on the ingress channel (same semantics as [`send_ui_command_with`]).
pub async fn send_task_event_with(task: RuntimeTaskEvent, ingress: RuntimeIngress) {
    ingress.send_task(task).await;
}

/// Send a typed [`UiCommand`] on the ingress channel.
pub async fn send_ui_command_with(cmd: UiCommand, ingress: RuntimeIngress) {
    ingress.send_ui(cmd).await;
}

pub async fn send_status_message_with(message: impl Into<StatusMessage>, ingress: RuntimeIngress) {
    ingress.send_status(message).await;
}

pub fn spawn_task_event_with_future(
    work: helix_runtime::Work,
    future: impl std::future::Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
    ingress: RuntimeIngress,
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
    ingress: RuntimeIngress,
) {
    work.spawn(async move {
        match future.await {
            Ok(cmd) => send_ui_command_with(cmd, ingress.clone()).await,
            Err(err) => send_status_message_with(err, ingress).await,
        }
    })
    .detach();
}
