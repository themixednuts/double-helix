//! Typed events delivered from async/runtime work into the main application loop.
//!
//! Background and UI tasks send through [`RuntimeIngress`] typed methods. The raw
//! mailbox event is private so producer code cannot enqueue arbitrary variants or
//! bypass the non-blocking/full-queue handoff policy.
//!
//! **Split:** [`RuntimeTaskEvent`] applies editor-side effects (often via [`crate::effect`]).
//! [`UiCommand`] drives compositor / layers / widgets. Keep that boundary when adding variants.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use helix_core::diagnostic::{DiagnosticProvider, Severity};
use helix_core::{Selection, Transaction, Uri};
use helix_dap::ThreadId as DebugThreadId;
use helix_lsp::{lsp, LanguageServerId};
use helix_runtime::{
    DebouncedSender, LatestByKeyReceiver, LatestByKeySender, Receiver as IngressReceiver, Runtime,
    Sender as IngressSender, TimerId, Token, Work,
};
use helix_view::document::DocumentInlayHintsId;
use helix_view::document::FormatterError;
use helix_view::handlers::lsp::{
    LspFeatureRefreshKind, SignatureHelpInvoked, SignatureHelpRequestId,
};
use helix_view::{
    editor::{Action, Activation, FrameSelection, PanelBehavior, SavePolicy, ThreadSelectPolicy},
    DocumentId, ViewId,
};

use crate::handlers::diagnostics::{
    pull_diagnostics_channel, PullDiagnosticsCoordinator, PullDiagnosticsRequestOutcome,
    PullDiagnosticsSender, PullDiagnosticsTarget,
};

use super::ui::{
    command::{DocumentOpenLane, DocumentOpenRequest, DocumentReloadOrigin},
    document::{DocumentOpenQueue, DocumentReloadQueue},
    file_explorer::{
        FileExplorerPreviewLoadRequest, FileExplorerPreviewQueue, FileExplorerPreviewRequest,
        FileExplorerSearchQueue, FileExplorerSearchRequest, FileExplorerTreeQueue,
        PreparedFileExplorerPreview,
    },
    UiCommand,
};

/// Default capacity for the ingress mailbox (bounded backpressure).
pub const BOUND: usize = 1024;

#[derive(Debug)]
enum StatusLane {}

#[derive(Debug)]
enum TimerLane {}

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

pub struct PreparedLanguageLoader {
    pub generation: u64,
    pub changed_grammars: BTreeSet<String>,
    pub loader: helix_core::syntax::Loader,
}

pub struct PreparedConfigReload {
    pub request: u64,
    pub config: Box<crate::config::Config>,
    pub language_loader: helix_core::syntax::Loader,
}

#[derive(Debug, Clone)]
pub struct DapParentRequest {
    pub client_id: helix_dap::registry::DebugAdapterId,
    pub sequence: u64,
    pub command: String,
}

#[derive(Debug)]
pub struct DapSessionRequest {
    pub connection_type: helix_dap::ConnectionType,
    pub arguments: serde_json::Value,
    pub parent: Option<DapParentRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DapOperation {
    StackTrace,
    Configuration,
    Termination,
}

#[derive(Debug)]
pub struct DapConfiguredBreakpoints {
    pub path: PathBuf,
    pub expected: Vec<helix_view::editor::Breakpoint>,
    pub result: Result<Option<Vec<helix_dap::Breakpoint>>, String>,
}

#[derive(Debug, Default)]
struct DapOperationTracker {
    next: std::sync::atomic::AtomicU64,
    current: Mutex<HashMap<(helix_dap::registry::DebugAdapterId, DapOperation), u64>>,
}

impl DapOperationTracker {
    fn begin(
        &self,
        client_id: helix_dap::registry::DebugAdapterId,
        operation: DapOperation,
    ) -> u64 {
        let generation = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((client_id, operation), generation);
        generation
    }

    fn is_current(
        &self,
        client_id: helix_dap::registry::DebugAdapterId,
        operation: DapOperation,
        generation: u64,
    ) -> bool {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(client_id, operation))
            .is_some_and(|current| *current == generation)
    }

    fn clear(&self, client_id: helix_dap::registry::DebugAdapterId) {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(current_client, _), _| *current_client != client_id);
    }
}

#[derive(Debug)]
pub struct AssistantBackendConnection {
    pub launch: helix_view::editor::AssistantBackendLaunch,
    pub profile: Option<helix_view::assistant::profile::Defaults>,
    pub panel: PanelBehavior,
}

impl AssistantBackendConnection {
    pub fn from_agent(
        agent: helix_view::editor::AgentConfig,
        profile: Option<helix_view::assistant::profile::Defaults>,
        panel: PanelBehavior,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            launch: agent.into_backend_launch()?,
            profile,
            panel,
        })
    }

    pub fn direct(
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        mcp_servers: Vec<helix_acp::types::McpServer>,
        profile: Option<helix_view::assistant::profile::Defaults>,
        panel: PanelBehavior,
    ) -> Self {
        Self {
            launch: helix_view::editor::AgentConfig::direct_backend_launch(
                command,
                args,
                env,
                mcp_servers,
            ),
            profile,
            panel,
        }
    }
}

#[derive(Debug)]
pub struct PreparedAssistantAgents {
    pub config_generation: u64,
    pub generation: u64,
    pub agents: Arc<BTreeMap<String, helix_view::editor::AgentConfig>>,
}

impl std::fmt::Debug for PreparedLanguageLoader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedLanguageLoader")
            .field("generation", &self.generation)
            .field("changed_grammars", &self.changed_grammars)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PreparedConfigReload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedConfigReload")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
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

impl From<String> for StatusMessage {
    fn from(message: String) -> Self {
        Self {
            severity: Severity::Info,
            message: Cow::Owned(message),
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
    Task(Box<RuntimeTaskEvent>),
    /// Assistant permission decision returned from a frontend popup.
    AssistantPermissionResolved {
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::RequestId,
        decision: helix_view::assistant::permission::Decision,
    },
    /// Typed compositor command (UI thread).
    Ui(UiCommand),
    /// Plugin event to resolve against current editor state on the UI thread.
    Plugin(super::PluginNotification),
}

/// Private mailbox payload. Only [`RuntimeIngress`] can construct and queue this
/// type, which prevents callers from bypassing the typed ingress API.
struct RuntimeEvent(RuntimeDelivery);

#[derive(Debug, thiserror::Error)]
pub enum RuntimeSendError {
    #[error("runtime ingress is full")]
    Full(RuntimeDelivery),
    #[error("runtime ingress is closed")]
    Closed(RuntimeDelivery),
}

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
            RuntimeDelivery::Plugin(notification) => {
                f.debug_tuple("Plugin").field(notification).finish()
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeIngress {
    tx: IngressSender<RuntimeEvent>,
    status_tx: LatestByKeySender<(), StatusMessage, StatusLane>,
    timer_tx: LatestByKeySender<TimerId, (), TimerLane>,
    pull_diagnostics: PullDiagnosticsSender,
    file_explorer_previews: FileExplorerPreviewQueue,
    file_explorer_searches: FileExplorerSearchQueue,
    file_explorer_trees: FileExplorerTreeQueue,
    document_reloads: DocumentReloadQueue,
    document_opens: DocumentOpenQueue,
    syntax: super::syntax::SyntaxService,
    packages: super::pkg::PkgService,
    dap_operations: Arc<DapOperationTracker>,
}

#[derive(Debug)]
pub struct RuntimeIngressReceiver {
    rx: IngressReceiver<RuntimeEvent>,
    status_rx: LatestByKeyReceiver<(), StatusMessage, StatusLane>,
    timer_rx: LatestByKeyReceiver<TimerId, (), TimerLane>,
    reliable_open: bool,
    status_open: bool,
    timer_open: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeTaskSink {
    tx: IngressSender<RuntimeEvent>,
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
    pub(crate) fn channel(runtime: Runtime) -> (Self, RuntimeIngressReceiver) {
        let work = runtime.work().clone();
        let block = runtime.block().clone();
        let clock = runtime.clock().clone();
        let (tx, rx) = helix_runtime::channel(BOUND);
        let (status_tx, status_rx) = helix_runtime::latest_by_key_for(1);
        let (timer_tx, timer_rx) = helix_runtime::latest_by_key_for(BOUND);
        let (pull_diagnostics, pull_diagnostics_rx) = pull_diagnostics_channel();
        let file_explorer_previews = FileExplorerPreviewQueue::new(work.clone(), block.clone());
        let file_explorer_searches = FileExplorerSearchQueue::spawn(work.clone(), block.clone());
        let file_explorer_trees = FileExplorerTreeQueue::spawn(work.clone(), block.clone());
        let document_reloads = DocumentReloadQueue::new(work.clone(), block.clone());
        let document_opens = DocumentOpenQueue::new(work.clone(), block.clone());
        let task_sink = RuntimeTaskSink { tx: tx.clone() };
        let syntax = super::syntax::SyntaxService::spawn(
            work.clone(),
            block.clone(),
            BOUND,
            task_sink.clone(),
        );
        let packages = super::pkg::PkgService::spawn(work.clone(), block, task_sink.clone());
        let ingress = Self {
            tx,
            status_tx,
            timer_tx,
            pull_diagnostics,
            file_explorer_previews,
            file_explorer_searches,
            file_explorer_trees,
            document_reloads,
            document_opens,
            syntax,
            packages,
            dap_operations: Arc::new(DapOperationTracker::default()),
        };
        PullDiagnosticsCoordinator::spawn(work, clock, pull_diagnostics_rx, task_sink);
        (
            ingress,
            RuntimeIngressReceiver {
                rx,
                status_rx,
                timer_rx,
                reliable_open: true,
                status_open: true,
                timer_open: true,
            },
        )
    }

    fn try_send(&self, event: RuntimeEvent) -> Result<(), RuntimeSendError> {
        match self.tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(helix_runtime::TrySend::Full(event)) => Err(RuntimeSendError::Full(event.0)),
            Err(helix_runtime::TrySend::Closed(event)) => Err(RuntimeSendError::Closed(event.0)),
        }
    }

    async fn send(&self, event: RuntimeEvent) -> Result<(), RuntimeSendError> {
        self.tx
            .send(event)
            .await
            .map_err(|event| RuntimeSendError::Closed(event.0 .0))
    }

    pub fn status(&self, message: impl Into<StatusMessage>) {
        let message = message.into();
        let _ = self.status_tx.try_send((), message);
    }

    pub async fn send_status(&self, message: impl Into<StatusMessage>) {
        self.status(message);
    }

    pub fn task(&self, task: RuntimeTaskEvent) -> Result<(), RuntimeSendError> {
        self.try_send(RuntimeEvent(RuntimeDelivery::Task(Box::new(task))))
    }

    pub async fn send_task(&self, task: RuntimeTaskEvent) -> Result<(), RuntimeSendError> {
        self.send(RuntimeEvent(RuntimeDelivery::Task(Box::new(task))))
            .await
    }

    pub fn package(
        &self,
        operation: super::pkg::PkgOperation,
        config: helix_pkg::PkgConfig,
    ) -> Result<(), super::pkg::PkgAdmissionError> {
        self.package_with_origin(operation, config, super::pkg::PkgOperationOrigin::User)
    }

    pub fn package_with_origin(
        &self,
        operation: super::pkg::PkgOperation,
        config: helix_pkg::PkgConfig,
        origin: super::pkg::PkgOperationOrigin,
    ) -> Result<(), super::pkg::PkgAdmissionError> {
        self.packages.submit(operation, config, origin)
    }

    pub(crate) fn schedule_pull_diagnostics(&self, targets: Vec<PullDiagnosticsTarget>) {
        if targets.is_empty() {
            return;
        }
        self.pull_diagnostics.schedule(targets);
    }

    pub(crate) async fn finish_pull_diagnostics(
        &self,
        target: PullDiagnosticsTarget,
        outcome: PullDiagnosticsRequestOutcome,
    ) {
        self.pull_diagnostics.finish(target, outcome);
    }

    pub(crate) fn finish_pull_diagnostics_now(
        &self,
        target: PullDiagnosticsTarget,
        outcome: PullDiagnosticsRequestOutcome,
    ) {
        self.pull_diagnostics.finish(target, outcome);
    }

    pub(crate) fn pull_diagnostics_server_exited(&self, server_id: LanguageServerId) {
        self.pull_diagnostics.server_exited(server_id);
    }

    pub(crate) fn debounce_pull_diagnostics_document(&self, document_id: DocumentId) {
        self.pull_diagnostics.debounce_document(document_id);
    }

    pub(crate) fn debounce_pull_diagnostics_inter_file_sweep(
        &self,
        language_servers: HashSet<LanguageServerId>,
    ) {
        if !language_servers.is_empty() {
            self.pull_diagnostics
                .debounce_inter_file_sweep(language_servers);
        }
    }

    pub fn ui(&self, cmd: UiCommand) -> Result<(), RuntimeSendError> {
        self.try_send(RuntimeEvent(RuntimeDelivery::Ui(cmd)))
    }

    pub(crate) fn file_explorer_search(&self, request: FileExplorerSearchRequest) {
        self.file_explorer_searches.submit(request, self.clone());
    }

    pub(crate) fn document_reload(
        &self,
        work: helix_view::editor::DocumentReloadWork,
        origin: DocumentReloadOrigin,
    ) {
        self.document_reloads.submit(work, origin, self.clone());
    }

    pub(crate) fn syntax_refresh(
        &self,
        request: helix_view::document::SyntaxRefreshRequest,
    ) -> Result<(), super::syntax::SyntaxAdmissionError> {
        self.syntax.submit(request)
    }

    pub(crate) fn take_document_reload(&self, document: DocumentId, generation: u64) -> bool {
        self.document_reloads.take_current(document, generation)
    }

    pub(crate) fn document_open(
        &self,
        work: helix_view::editor::DocumentOpenWork,
        request: DocumentOpenRequest,
    ) {
        self.document_opens.submit(work, request, self.clone());
    }

    pub(crate) fn document_open_batch(
        &self,
        jobs: Vec<(helix_view::editor::DocumentOpenWork, DocumentOpenRequest)>,
        lane: DocumentOpenLane,
        stop_on_error: bool,
    ) {
        self.document_opens
            .submit_batch(jobs, lane, stop_on_error, self.clone());
    }

    pub(crate) fn take_document_open(&self, lane: DocumentOpenLane, generation: u64) -> bool {
        self.document_opens.take_current(lane, generation)
    }

    pub(crate) fn cancel_document_open(&self, lane: DocumentOpenLane) {
        self.document_opens.cancel(lane);
    }

    pub(crate) fn begin_dap_operation(
        &self,
        client_id: helix_dap::registry::DebugAdapterId,
        operation: DapOperation,
    ) -> u64 {
        self.dap_operations.begin(client_id, operation)
    }

    pub(crate) fn is_current_dap_operation(
        &self,
        client_id: helix_dap::registry::DebugAdapterId,
        operation: DapOperation,
        generation: u64,
    ) -> bool {
        self.dap_operations
            .is_current(client_id, operation, generation)
    }

    pub(crate) fn clear_dap_client(&self, client_id: helix_dap::registry::DebugAdapterId) {
        self.dap_operations.clear(client_id);
    }

    pub(crate) fn file_explorer_preview(&self, request: FileExplorerPreviewLoadRequest) {
        self.file_explorer_previews.submit(request, self.clone());
    }

    pub(crate) fn file_explorer_tree(&self, work: crate::ui::FileExplorerTreeWork) {
        self.file_explorer_trees.submit(work, self.clone());
    }

    pub(crate) fn take_file_explorer_tree(
        &self,
        root: &std::path::Path,
        generation: u64,
    ) -> Option<crate::ui::PreparedFileExplorerTree> {
        self.file_explorer_trees.take(root, generation)
    }

    pub(crate) fn cancel_file_explorer_preview(&self) {
        self.file_explorer_previews.cancel();
    }

    pub(crate) fn take_file_explorer_preview(
        &self,
        request: &FileExplorerPreviewRequest,
    ) -> Option<PreparedFileExplorerPreview> {
        self.file_explorer_previews.take(request)
    }

    #[cfg(test)]
    pub(crate) fn store_file_explorer_preview(&self, prepared: PreparedFileExplorerPreview) {
        self.file_explorer_previews.store_prepared(prepared);
    }

    pub fn plugin(&self, notification: super::PluginNotification) -> Result<(), RuntimeSendError> {
        self.try_send(RuntimeEvent(RuntimeDelivery::Plugin(notification)))
    }

    pub async fn send_ui(&self, cmd: UiCommand) -> Result<(), RuntimeSendError> {
        self.send(RuntimeEvent(RuntimeDelivery::Ui(cmd))).await
    }

    pub async fn send_plugin(
        &self,
        notification: super::PluginNotification,
    ) -> Result<(), RuntimeSendError> {
        self.send(RuntimeEvent(RuntimeDelivery::Plugin(notification)))
            .await
    }

    pub async fn send_timer(&self, id: TimerId) {
        let _ = self.timer_tx.send(id, ()).await;
    }

    pub fn assistant_permission_resolved(
        &self,
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::RequestId,
        decision: helix_view::assistant::permission::Decision,
    ) -> Result<(), RuntimeSendError> {
        self.try_send(RuntimeEvent(RuntimeDelivery::AssistantPermissionResolved {
            thread,
            request,
            decision,
        }))
    }

    pub async fn send_assistant_permission_resolved(
        &self,
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::RequestId,
        decision: helix_view::assistant::permission::Decision,
    ) -> Result<(), RuntimeSendError> {
        self.send(RuntimeEvent(RuntimeDelivery::AssistantPermissionResolved {
            thread,
            request,
            decision,
        }))
        .await
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

impl RuntimeTaskSink {
    pub(crate) async fn send(&self, task: RuntimeTaskEvent) -> bool {
        self.tx
            .send(RuntimeEvent(RuntimeDelivery::Task(Box::new(task))))
            .await
            .is_ok()
    }
}

impl RuntimeIngressReceiver {
    pub async fn recv(&mut self) -> Option<RuntimeDelivery> {
        loop {
            if !self.reliable_open && !self.status_open && !self.timer_open {
                return None;
            }

            tokio::select! {
                event = self.rx.recv(), if self.reliable_open => match event {
                    Some(event) => return Some(event.0),
                    None => self.reliable_open = false,
                },
                status = self.status_rx.recv(), if self.status_open => match status {
                    Some(((), status)) => return Some(RuntimeDelivery::Status {
                        message: status.message.into_owned(),
                        severity: status.severity,
                    }),
                    None => self.status_open = false,
                },
                timer = self.timer_rx.recv(), if self.timer_open => match timer {
                    Some((id, ())) => return Some(RuntimeDelivery::Timer(id)),
                    None => self.timer_open = false,
                },
            }
        }
    }

    pub fn try_recv(&mut self) -> Result<RuntimeDelivery, helix_runtime::TryRecvError> {
        if self.reliable_open {
            match self.rx.try_recv() {
                Ok(event) => return Ok(event.0),
                Err(helix_runtime::TryRecvError::Closed) => self.reliable_open = false,
                Err(helix_runtime::TryRecvError::Empty) => {}
            }
        }

        if self.timer_open {
            match self.timer_rx.try_recv() {
                Ok((id, ())) => return Ok(RuntimeDelivery::Timer(id)),
                Err(helix_runtime::TryRecvError::Closed) => self.timer_open = false,
                Err(helix_runtime::TryRecvError::Empty) => {}
            }
        }

        if self.status_open {
            match self.status_rx.try_recv() {
                Ok(((), status)) => {
                    return Ok(RuntimeDelivery::Status {
                        message: status.message.into_owned(),
                        severity: status.severity,
                    });
                }
                Err(helix_runtime::TryRecvError::Closed) => self.status_open = false,
                Err(helix_runtime::TryRecvError::Empty) => {}
            }
        }

        if !self.reliable_open && !self.status_open && !self.timer_open {
            Err(helix_runtime::TryRecvError::Closed)
        } else {
            Err(helix_runtime::TryRecvError::Empty)
        }
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
        self.inner
            .send(RuntimeEvent(RuntimeDelivery::Task(Box::new(task))));
    }

    pub fn send_now(&self, task: RuntimeTaskEvent) {
        self.inner
            .send_now(RuntimeEvent(RuntimeDelivery::Task(Box::new(task))));
    }

    pub fn send_after(&self, task: RuntimeTaskEvent, delay: std::time::Duration) {
        self.inner
            .send_after(RuntimeEvent(RuntimeDelivery::Task(Box::new(task))), delay);
    }

    pub fn send_after_with(
        &self,
        delay: std::time::Duration,
        build: impl FnOnce() -> Option<RuntimeTaskEvent> + Send + 'static,
    ) {
        self.inner.send_after_with(delay, move || {
            build()
                .map(Box::new)
                .map(RuntimeDelivery::Task)
                .map(RuntimeEvent)
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
    /// A background syntax parse completed for an exact document version.
    ApplySyntax {
        document: DocumentId,
        version: i32,
        syntax: helix_core::Syntax,
    },
    /// Blocking inspection completed for the active file-operation FIFO entry.
    FileOperationInspected {
        id: helix_view::editor::FileOperationId,
        result: Result<
            helix_view::editor::FileOperationPrepared,
            helix_view::editor::FileOperationError,
        >,
    },
    /// Ordered LSP `will*` requests completed for the active file operation.
    FileOperationWillCompleted {
        id: helix_view::editor::FileOperationId,
        edits: Vec<(helix_lsp::OffsetEncoding, lsp::WorkspaceEdit)>,
        errors: Vec<String>,
    },
    /// A WorkspaceEdit was planned after closed-file inspection on a blocking worker.
    WorkspaceEditPrepared {
        parent: Option<helix_view::editor::FileOperationId>,
        continuation: Option<helix_view::editor::WorkspaceEditContinuation>,
        result: Result<
            helix_view::handlers::workspace_edit::WorkspaceEditPlan,
            helix_view::handlers::workspace_edit::ApplyEditError,
        >,
    },
    /// The active file operation completed entirely on a blocking worker.
    FileOperationMutated(helix_view::editor::FileOperationOutcome),
    /// Apply a prepared transaction on the main loop if the document revision still matches.
    /// Used by async LSP work (e.g. range format) instead of untyped main-thread closures.
    ApplyTransactionIfCurrent {
        doc_id: DocumentId,
        view_id: ViewId,
        expected_version: i32,
        transaction: Transaction,
    },
    /// Apply output from an external shell command against its exact source revision.
    ApplyShellResult {
        doc_id: DocumentId,
        view_id: ViewId,
        expected_version: i32,
        transaction: Option<Transaction>,
        selection: Option<Selection>,
    },
    /// Apply supplemental edits from an asynchronously resolved completion.
    ApplyCompletionAdditionalEdits {
        doc_id: DocumentId,
        view_id: ViewId,
        expected_version: i32,
        offset_encoding: helix_lsp::OffsetEncoding,
        edits: Vec<lsp::TextEdit>,
    },
    /// Apply an asynchronous LSP selection-range response against its exact source state.
    ApplyLspSelectionRange(helix_view::handlers::lsp::SelectionRangeResponse),
    /// Apply diagnostics after sorting, counting, and range conversion completed off-thread.
    ApplyPreparedLspDiagnostics {
        server_id: LanguageServerId,
        uri: Uri,
        generation: u64,
        prepared: helix_view::handlers::lsp::PreparedLspDiagnostics,
    },
    /// Dismiss a popup notification by id (timeout expiry path).
    DismissNotification {
        id: usize,
    },
    /// Async formatter / LSP format finished; apply on the main loop if the revision matches.
    ApplyFormattingResult {
        doc_id: DocumentId,
        view_id: ViewId,
        expected_version: i32,
        format_result: Result<Transaction, FormatterError>,
        write: Option<PendingFormatWrite>,
    },
    /// Show an error on the editor after async work (e.g. external URL open failed).
    SetEditorError {
        message: String,
    },
    /// Attach document color swatches on the main thread.
    AttachDocumentColors {
        doc_id: DocumentId,
        expected_version: i32,
        request: Token,
        colors: Vec<(usize, lsp::Color)>,
    },
    /// Start one coordinator-approved pull-diagnostics request on the main thread.
    StartPullDiagnostics {
        target: PullDiagnosticsTarget,
        cancel: Token,
    },
    /// Apply a current LSP pull-diagnostics report on the main thread.
    PullDiagnosticsResponse {
        target: PullDiagnosticsTarget,
        uri: Uri,
        provider: DiagnosticProvider,
        result: lsp::DocumentDiagnosticReportResult,
    },
    /// Debounced document color refresh (after [`DocumentColorsHandler`] debounce).
    RequestDocumentColorsDebounced {
        doc_ids: HashSet<DocumentId>,
    },
    RequestLspFeaturesDebounced {
        docs: HashMap<DocumentId, HashSet<LspFeatureRefreshKind>>,
    },
    /// Resolve a debounced document set into coordinator targets on the main thread.
    QueuePullDiagnosticsForDocuments {
        document_ids: HashSet<DocumentId>,
    },
    /// Resolve a debounced inter-file sweep into coordinator targets on the main thread.
    QueuePullDiagnosticsInterFileSweep {
        language_servers: HashSet<LanguageServerId>,
    },
    /// Debounced LSP signature request (after [`SignatureHelpHandler`] debounce).
    RequestSignatureDebounced {
        invoked: SignatureHelpInvoked,
        request: SignatureHelpRequestId,
        trigger_kind: lsp::SignatureHelpTriggerKind,
        is_retrigger: bool,
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
        server_id: LanguageServerId,
        offset_encoding: helix_lsp::OffsetEncoding,
        id: DocumentInlayHintsId,
        hints: Vec<lsp::InlayHint>,
    },
    ApplyCodeLenses {
        doc_id: DocumentId,
        expected_version: i32,
        request: Token,
        lenses: Vec<(LanguageServerId, helix_lsp::OffsetEncoding, lsp::CodeLens)>,
    },
    ApplySemanticTokens {
        doc_id: DocumentId,
        server_id: LanguageServerId,
        request: Token,
        tokens: helix_view::document_lsp::DocumentSemanticTokenUpdate,
    },
    ApplyInlineCompletion {
        doc_id: DocumentId,
        request: Token,
        completion: helix_view::document_lsp::InlineCompletionGhost,
    },
    ApplyInlineValues {
        doc_id: DocumentId,
        expected_version: i32,
        request: Token,
        values: helix_view::document_lsp::DocumentInlineValues,
    },
    /// Request debugger inline values after a stack-frame document is ready.
    RequestInlineValues {
        doc_id: DocumentId,
    },
    ApplyDocumentLinks {
        doc_id: DocumentId,
        expected_version: i32,
        request: Token,
        links: Vec<(
            LanguageServerId,
            helix_lsp::OffsetEncoding,
            lsp::DocumentLink,
        )>,
    },
    ApplyFoldingRanges {
        doc_id: DocumentId,
        expected_version: i32,
        request: Token,
        folds: helix_core::text_folding::FoldContainer,
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
    /// A debugger transport and initialize request completed away from the UI thread.
    DapClientStartupCompleted {
        id: helix_dap::registry::DebugAdapterId,
        result: Box<Result<helix_dap::registry::StartedClient, helix_dap::Error>>,
        session: DapSessionRequest,
    },
    /// A launch/attach request completed after the initialized client was installed.
    DapSessionStartupCompleted {
        client_id: helix_dap::registry::DebugAdapterId,
        parent: Option<DapParentRequest>,
        result: Result<(), String>,
    },
    /// Stack traces fetched for one stopped event. Superseded stop/continue results are dropped.
    DapStoppedCompleted {
        client_id: helix_dap::registry::DebugAdapterId,
        generation: u64,
        preferred_thread_id: Option<DebugThreadId>,
        stacks: Vec<(DebugThreadId, Vec<helix_dap::StackFrame>)>,
        errors: Vec<String>,
    },
    /// Existing breakpoints and configurationDone completed for an initialized adapter.
    DapInitializedCompleted {
        client_id: helix_dap::registry::DebugAdapterId,
        generation: u64,
        breakpoints: Vec<DapConfiguredBreakpoints>,
        configuration_result: Result<(), String>,
    },
    /// The disconnect phase of a terminated event completed.
    DapDisconnectCompleted {
        client_id: helix_dap::registry::DebugAdapterId,
        generation: u64,
        restart: Option<serde_json::Value>,
        connection_type: Option<helix_dap::ConnectionType>,
        result: Result<(), String>,
    },
    /// Relaunch/reattach after a terminated event completed.
    DapRelaunchCompleted {
        client_id: helix_dap::registry::DebugAdapterId,
        generation: u64,
        result: Result<(), String>,
    },
    /// A reverse DAP request is ready to be replied to on its original client.
    DapAdapterReplyReady {
        parent: DapParentRequest,
        result: Result<serde_json::Value, String>,
    },
    /// Report a DAP request failure only while its originating client still exists.
    DapRequestFailed {
        client_id: helix_dap::registry::DebugAdapterId,
        message: String,
    },
    /// DAP restart completed; update editor state and status.
    DapRestarted,
    /// DAP resume/continue/step completed; resume application state.
    ResumeDebuggerApplication,
    /// DAP terminate completed; unset active debugger client.
    UnsetActiveDebugClient,
    /// DAP exception breakpoint configuration completed.
    DapExceptionsConfigured,
    /// Package-manager operation progress from a blocking engine task.
    PkgEvent(helix_pkg::OpEvent),
    /// A newer coherent runtime activation snapshot was published.
    RuntimeAssetsChanged(helix_loader::RuntimeAssetsChange),
    /// A replacement language loader built off-thread for one runtime generation.
    ApplyRuntimeLanguageLoader(PreparedLanguageLoader),
    /// A complete config and language loader prepared away from the UI thread.
    ApplyConfigReload(PreparedConfigReload),
    ConfigReloadFailed {
        request: u64,
        message: String,
    },
    /// A packaged ACP-agent cache built off-thread for one runtime generation.
    ApplyAssistantAgents(PreparedAssistantAgents),
    /// Correlated terminal package-operation state; progress events are presentation-only.
    PkgOperationFinished(super::pkg::PkgOperationOutcome),
    /// Retry language-server attachment after a capability became available.
    RefreshLanguageServers {
        document_ids: BTreeSet<DocumentId>,
    },
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
    /// Service an out-of-process plugin host request on the editor main thread.
    PluginHostRequest {
        state: crate::plugin_registry::PluginHostState,
        request: helix_plugin::rpc::PluginRequest,
        respond_to: crate::plugin_registry::PluginHostResponder,
    },
    /// Remove the assistant panel model entry from editor-owned layout state.
    RemoveAssistantPanel,
    /// Connect an assistant backend and apply resulting editor-owned state.
    ConnectAssistantBackend(Box<AssistantBackendConnection>),
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
    /// Delete an assistant history entry locally and remotely when supported.
    DeleteAssistantHistoryThread {
        thread: helix_view::assistant::thread::Id,
        delete_remote: bool,
    },
    /// Fetch another assistant history page from the active backend.
    FetchAssistantHistoryPage {
        scope: helix_view::assistant::thread::Scope,
        cursor: Option<helix_view::assistant::history::Cursor>,
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
        client_id: helix_dap::registry::DebugAdapterId,
        generation: u64,
        thread_id: DebugThreadId,
        frames: Vec<helix_dap::StackFrame>,
        selection: FrameSelection,
    },
    /// Apply a DAP breakpoint response only if the submitted snapshot is still current.
    ApplyBreakpointsResponse {
        client_id: helix_dap::registry::DebugAdapterId,
        path: PathBuf,
        expected: Vec<helix_view::editor::Breakpoint>,
        response: Option<Vec<helix_dap::Breakpoint>>,
    },
    /// Execute an LSP command through the editor-owned main-thread path.
    ExecuteLspCommand {
        command: lsp::Command,
        server_id: LanguageServerId,
    },
    /// Resolve and execute a code lens only while its source document is unchanged.
    ApplyResolvedCodeLens {
        doc_id: DocumentId,
        expected_version: i32,
        server_id: LanguageServerId,
        original: lsp::CodeLens,
        resolved: lsp::CodeLens,
    },
    /// Open an asynchronously resolved document link if its source is still current.
    OpenResolvedDocumentLink {
        doc_id: DocumentId,
        expected_version: i32,
        target: lsp::Url,
        action: Action,
    },
    /// Apply a rename response only against the document revision that requested it.
    ApplyRenameEdit {
        doc_id: DocumentId,
        expected_version: i32,
        offset_encoding: helix_lsp::OffsetEncoding,
        workspace_edit: Option<lsp::WorkspaceEdit>,
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
    AutoSaveRun {
        save_pending: Arc<AtomicBool>,
    },
    /// Auto-reload debounce finished (insert mode defers; else reload).
    AutoReloadRun {
        documents: BTreeSet<DocumentId>,
        reload_pending: Arc<Mutex<BTreeSet<DocumentId>>>,
    },
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
    let _ = ingress.send_task(task).await;
}

/// Send a typed [`UiCommand`] on the ingress channel.
pub async fn send_ui_command_with(cmd: UiCommand, ingress: RuntimeIngress) {
    let _ = ingress.send_ui(cmd).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lane_keeps_only_the_latest_value() {
        let rt = helix_runtime::test::RuntimeTest::default();
        let (ingress, mut receiver) = RuntimeIngress::channel(rt.runtime().clone());
        for index in 0..10_000 {
            ingress.status(index.to_string());
        }

        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeDelivery::Status { message, .. }) if message == "9999"
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
    }

    #[test]
    fn timer_lane_coalesces_duplicate_ids() {
        let rt = helix_runtime::test::RuntimeTest::default();
        let (ingress, mut receiver) = RuntimeIngress::channel(rt.runtime().clone());
        rt.block_on(async {
            for _ in 0..100 {
                ingress.send_timer(TimerId(7)).await;
            }
        });

        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeDelivery::Timer(TimerId(7)))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
    }

    #[test]
    fn reliable_lane_reports_saturation_without_spilling() {
        let rt = helix_runtime::test::RuntimeTest::default();
        let (ingress, _receiver) = RuntimeIngress::channel(rt.runtime().clone());
        for _ in 0..BOUND {
            ingress.task(RuntimeTaskEvent::DapRestarted).unwrap();
        }
        assert!(matches!(
            ingress.task(RuntimeTaskEvent::DapRestarted),
            Err(RuntimeSendError::Full(RuntimeDelivery::Task(_)))
        ));
    }

    #[test]
    fn dap_operation_tracker_rejects_superseded_completions_per_operation() {
        let tracker = DapOperationTracker::default();
        let mut registry = helix_dap::registry::Registry::new();
        let id = registry
            .start_client(
                None,
                helix_core::syntax::config::DebugAdapterConfig {
                    name: "test".into(),
                    transport: "stdio".into(),
                    command: String::new(),
                    args: Vec::new(),
                    port_arg: None,
                    templates: Vec::new(),
                    quirks: Default::default(),
                },
            )
            .id();

        let first = tracker.begin(id, DapOperation::StackTrace);
        let configuration = tracker.begin(id, DapOperation::Configuration);
        let second = tracker.begin(id, DapOperation::StackTrace);

        assert!(!tracker.is_current(id, DapOperation::StackTrace, first));
        assert!(tracker.is_current(id, DapOperation::StackTrace, second));
        assert!(tracker.is_current(id, DapOperation::Configuration, configuration));

        tracker.clear(id);
        assert!(!tracker.is_current(id, DapOperation::StackTrace, second));
        assert!(!tracker.is_current(id, DapOperation::Configuration, configuration));
    }
}
