use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use crate::{
    compositor::Compositor,
    runtime::{
        ui::command::{FileExplorerCommand, ModifiedBufferCheck},
        ui::snapshot::UiSnapshotRequest,
        UiCommand,
    },
    ui::{
        Confirmation, ExplorerPath, ExplorerSource, ExplorerSourceId, FileExplorerPanel,
        FileExplorerTreeRefresh, FileExplorerTreeWork, PreparedFileExplorerTree, Prompt,
        PromptEvent, FILE_EXPLORER_ID,
    },
};
use helix_view::{
    editor::{
        DocumentOpenRole, PreparedWorkspaceDocumentOpen, SavePolicy, WorkspaceDocumentOpenWork,
    },
    DocumentId, Editor,
};

struct FileExplorerTreeJob {
    work: FileExplorerTreeWork,
    ingress: crate::runtime::RuntimeIngress,
}

#[derive(Default)]
struct FileExplorerTreeQueueState {
    pending: Option<FileExplorerTreeJob>,
    active: Option<tokio_util::sync::CancellationToken>,
    prepared: Option<PreparedFileExplorerTree>,
}

#[derive(Debug)]
enum FileExplorerTreePulse {}

#[derive(Clone)]
pub(crate) struct FileExplorerTreeQueue {
    state: Arc<Mutex<FileExplorerTreeQueueState>>,
    wake: helix_runtime::PulseHandle<FileExplorerTreePulse>,
}

impl std::fmt::Debug for FileExplorerTreeQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("FileExplorerTreeQueue")
            .field("pending", &state.pending.is_some())
            .field("prepared", &state.prepared.is_some())
            .finish()
    }
}

impl FileExplorerTreeQueue {
    pub(crate) fn spawn(work: helix_runtime::Work, block: helix_runtime::Block) -> Self {
        let state = Arc::new(Mutex::new(FileExplorerTreeQueueState::default()));
        let mut gate = helix_runtime::PulseGate::<FileExplorerTreePulse>::new();
        let wake = gate.handle();
        let mut wake_rx = gate.take_receiver();
        let actor_state = state.clone();
        work.spawn(async move {
            while wake_rx.recv().await.is_some() {
                loop {
                    let Some(job) = actor_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pending
                        .take()
                    else {
                        break;
                    };
                    let FileExplorerTreeJob { work, ingress } = job;
                    let generation = work.generation();
                    let root = work.root().clone();
                    let canceled = tokio_util::sync::CancellationToken::new();
                    actor_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .active = Some(canceled.clone());
                    let result = if work.is_remote() {
                        Ok(work.execute_remote(canceled.clone()).await)
                    } else if work.is_collaboration() {
                        Ok(work.execute_collaboration(canceled.clone()).await)
                    } else {
                        block.spawn(move || work.execute()).await
                    };
                    let outcome = {
                        let mut state = actor_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.active = None;
                        if canceled.is_cancelled() {
                            continue;
                        }
                        if state.pending.is_some() {
                            continue;
                        }
                        match result {
                            Ok(Ok(prepared)) => {
                                state.prepared = Some(prepared);
                                Ok(())
                            }
                            Ok(Err(error)) => {
                                Err(format!("Failed to refresh file explorer: {error}"))
                            }
                            Err(error) => {
                                Err(format!("File explorer refresh worker failed: {error}"))
                            }
                        }
                    };
                    match outcome {
                        Ok(()) => {
                            let _ = ingress
                                .send_ui(UiCommand::FileExplorer(FileExplorerCommand::ApplyTree {
                                    root,
                                    generation,
                                }))
                                .await;
                        }
                        Err(error) => ingress.status(anyhow::anyhow!(error)),
                    }
                }
            }
        })
        .detach();
        Self { state, wake }
    }

    pub(crate) fn submit(
        &self,
        work: FileExplorerTreeWork,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = &state.active {
            active.cancel();
        }
        state.pending = Some(FileExplorerTreeJob { work, ingress });
        state.prepared = None;
        drop(state);
        self.wake.request();
    }

    pub(crate) fn take(
        &self,
        root: &ExplorerPath,
        generation: u64,
    ) -> Option<PreparedFileExplorerTree> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .prepared
            .as_ref()
            .is_some_and(|prepared| &prepared.root == root && prepared.generation == generation)
        {
            state.prepared.take()
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileExplorerPreviewRequest {
    pub(crate) source: ExplorerSourceId,
    pub(crate) root: ExplorerPath,
    pub(crate) path: ExplorerPath,
    pub(crate) cursor: u32,
    pub(crate) generation: u64,
}

pub(crate) struct PreparedFileExplorerPreview {
    pub(crate) request: FileExplorerPreviewRequest,
    pub(crate) result: Result<PreparedWorkspaceDocumentOpen, String>,
}

pub(crate) struct FileExplorerPreviewLoadRequest {
    request: FileExplorerPreviewRequest,
    work: WorkspaceDocumentOpenWork,
}

struct FileExplorerPreviewJob {
    load: FileExplorerPreviewLoadRequest,
    ingress: crate::runtime::RuntimeIngress,
}

#[derive(Default)]
struct FileExplorerPreviewQueueState {
    generation: Option<u64>,
    pending: Option<FileExplorerPreviewJob>,
    active: Option<(u64, tokio_util::sync::CancellationToken)>,
    prepared: Option<PreparedFileExplorerPreview>,
}

#[derive(Debug)]
enum FileExplorerPreviewPulse {}

#[derive(Clone)]
pub(crate) struct FileExplorerPreviewQueue {
    state: Arc<Mutex<FileExplorerPreviewQueueState>>,
    wake: helix_runtime::PulseHandle<FileExplorerPreviewPulse>,
}

impl std::fmt::Debug for FileExplorerPreviewQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("FileExplorerPreviewQueue")
            .field("generation", &state.generation)
            .field("running", &state.active.is_some())
            .field("pending", &state.pending.is_some())
            .field("prepared", &state.prepared.is_some())
            .finish()
    }
}

impl FileExplorerPreviewQueue {
    pub(crate) fn new(work: helix_runtime::Work, block: helix_runtime::Block) -> Self {
        let state = Arc::new(Mutex::new(FileExplorerPreviewQueueState::default()));
        let mut gate = helix_runtime::PulseGate::<FileExplorerPreviewPulse>::new();
        let wake = gate.handle();
        let mut wake_rx = gate.take_receiver();
        let actor_state = state.clone();

        work.spawn(async move {
            while wake_rx.recv().await.is_some() {
                loop {
                    let Some(job) = actor_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pending
                        .take()
                    else {
                        break;
                    };
                    let request = job.load.request.clone();
                    let generation = request.generation;
                    let token = tokio_util::sync::CancellationToken::new();
                    {
                        let mut queue = actor_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        queue.active = Some((generation, token.clone()));
                    }

                    let result = match job.load.work {
                        WorkspaceDocumentOpenWork::Local(work) => {
                            let worker_token = token.clone();
                            block
                                .spawn(move || {
                                    prepare_local_file_explorer_preview(work, &worker_token)
                                })
                                .await
                                .unwrap_or_else(|error| {
                                    Err(format!("preview worker failed: {error}"))
                                })
                        }
                        WorkspaceDocumentOpenWork::Remote(work) => work
                            .execute(token.child_token(), false)
                            .await
                            .map(PreparedWorkspaceDocumentOpen::Remote)
                            .map_err(|error| error.to_string()),
                        WorkspaceDocumentOpenWork::Collaboration(work) => work
                            .execute()
                            .await
                            .map(PreparedWorkspaceDocumentOpen::Collaboration)
                            .map_err(|error| error.to_string()),
                        WorkspaceDocumentOpenWork::Failed { error, .. } => Err(error.to_string()),
                    };

                    let should_notify = {
                        let mut queue = actor_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        queue.active = None;
                        let current = !token.is_cancelled()
                            && queue.generation == Some(generation)
                            && queue.pending.is_none();
                        if current {
                            queue.prepared = Some(PreparedFileExplorerPreview {
                                request: request.clone(),
                                result,
                            });
                        }
                        current
                    };

                    if should_notify {
                        let _ = job
                            .ingress
                            .send_ui(UiCommand::FileExplorer(FileExplorerCommand::ApplyPreview {
                                source: request.source,
                                root: request.root,
                                path: request.path,
                                cursor: request.cursor,
                                generation: request.generation,
                            }))
                            .await;
                    }
                }
            }
        })
        .detach();

        Self { state, wake }
    }

    pub(crate) fn submit(
        &self,
        load: FileExplorerPreviewLoadRequest,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        let generation = load.request.generation;
        let mut queue = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, token)) = &queue.active {
            token.cancel();
        }
        queue.generation = Some(generation);
        queue.prepared = None;
        queue.pending = Some(FileExplorerPreviewJob { load, ingress });
        drop(queue);
        self.wake.request();
    }

    pub(crate) fn cancel(&self) {
        let mut queue = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.generation = None;
        queue.pending = None;
        queue.prepared = None;
        if let Some((_, token)) = queue.active.take() {
            token.cancel();
        }
    }

    pub(crate) fn take(
        &self,
        request: &FileExplorerPreviewRequest,
    ) -> Option<PreparedFileExplorerPreview> {
        let mut queue = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.request == *request)
        {
            queue.prepared.take()
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(crate) fn store_prepared(&self, prepared: PreparedFileExplorerPreview) {
        let mut queue = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.pending = None;
        if let Some((_, token)) = queue.active.take() {
            token.cancel();
        }
        queue.generation = Some(prepared.request.generation);
        queue.prepared = Some(prepared);
    }
}

fn prepare_local_file_explorer_preview(
    work: helix_view::editor::DocumentOpenWork,
    token: &tokio_util::sync::CancellationToken,
) -> Result<PreparedWorkspaceDocumentOpen, String> {
    let start = Instant::now();
    let prepared = work.execute().map_err(|error| error.to_string())?;
    if token.is_cancelled() {
        return Err(String::from("preview request canceled"));
    }
    log::info!(
        "[file_explorer] preview prepared path={} generation={} elapsed_us={}",
        prepared.path().display(),
        0,
        start.elapsed().as_micros(),
    );
    Ok(PreparedWorkspaceDocumentOpen::Local(prepared))
}

pub(crate) fn queue_file_explorer_preview(
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
    request: FileExplorerPreviewRequest,
) {
    let work =
        editor.prepare_workspace_document_open(request.path.clone(), DocumentOpenRole::Preview);
    ingress.file_explorer_preview(FileExplorerPreviewLoadRequest { request, work });
}

#[derive(Clone, Debug)]
pub(crate) struct FileExplorerSearchRequest {
    source: ExplorerSource,
    source_id: ExplorerSourceId,
    root: ExplorerPath,
    query: String,
    generation: u64,
    config: helix_view::editor::FileExplorerConfig,
}

struct FileExplorerSearchJob {
    request: FileExplorerSearchRequest,
    ingress: crate::runtime::RuntimeIngress,
    abort: Arc<AtomicBool>,
    canceled: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
struct FileExplorerSearchState {
    pending: Option<FileExplorerSearchJob>,
    active_abort: Option<Arc<AtomicBool>>,
    active_canceled: Option<tokio_util::sync::CancellationToken>,
}

impl FileExplorerSearchState {
    fn replace(&mut self, job: FileExplorerSearchJob) {
        if let Some(active) = &self.active_abort {
            active.store(true, Ordering::Release);
        }
        if let Some(active) = &self.active_canceled {
            active.cancel();
        }
        if let Some(pending) = &self.pending {
            pending.abort.store(true, Ordering::Release);
            pending.canceled.cancel();
        }
        self.pending = Some(job);
    }

    fn take(&mut self) -> Option<FileExplorerSearchJob> {
        let job = self.pending.take()?;
        self.active_abort = Some(job.abort.clone());
        self.active_canceled = Some(job.canceled.clone());
        Some(job)
    }

    fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn finish(&mut self, abort: &Arc<AtomicBool>) -> bool {
        if self
            .active_abort
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, abort))
        {
            self.active_abort = None;
            self.active_canceled = None;
        }
        abort.load(Ordering::Acquire) || self.is_pending()
    }
}

#[derive(Debug)]
enum FileExplorerSearchPulse {}

#[derive(Clone)]
pub(crate) struct FileExplorerSearchQueue {
    state: Arc<Mutex<FileExplorerSearchState>>,
    wake: helix_runtime::PulseHandle<FileExplorerSearchPulse>,
}

impl std::fmt::Debug for FileExplorerSearchQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileExplorerSearchQueue")
            .field(
                "pending",
                &self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_pending(),
            )
            .finish()
    }
}

impl FileExplorerSearchQueue {
    pub(crate) fn spawn(work: helix_runtime::Work, block: helix_runtime::Block) -> Self {
        let state = Arc::new(Mutex::new(FileExplorerSearchState::default()));
        let mut gate = helix_runtime::PulseGate::<FileExplorerSearchPulse>::new();
        let wake = gate.handle();
        let mut wake_rx = gate.take_receiver();
        let actor_state = state.clone();

        work.spawn(async move {
            while wake_rx.recv().await.is_some() {
                loop {
                    let Some(job) = actor_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                    else {
                        break;
                    };
                    let request = job.request.clone();
                    let abort = job.abort.clone();
                    let result = match request.source.backend() {
                        helix_view::editor::WorkspaceBackend::Local => {
                            let worker_abort = abort.clone();
                            block
                                .spawn(move || {
                                    execute_local_file_explorer_search(request, &worker_abort)
                                })
                                .await
                                .map(Some)
                                .map_err(|error| error.to_string())
                        }
                        helix_view::editor::WorkspaceBackend::Remote(_) => {
                            execute_remote_file_explorer_search(
                                &request,
                                &job.ingress,
                                job.canceled.clone(),
                            )
                            .await
                            .map(|()| None)
                            .map_err(|error| error.to_string())
                        }
                        helix_view::editor::WorkspaceBackend::Collaboration(_) => {
                            execute_collaboration_file_explorer_search(
                                &request,
                                &abort,
                                job.canceled.clone(),
                                block.clone(),
                            )
                            .await
                            .map(Some)
                            .map_err(|error| error.to_string())
                        }
                    };
                    let superseded = actor_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .finish(&abort);
                    if superseded {
                        continue;
                    }
                    match result {
                        Ok(Some(matches)) => {
                            let _ = job
                                .ingress
                                .send_ui(UiCommand::FileExplorer(
                                    FileExplorerCommand::ApplySearchResults {
                                        source: job.request.source_id,
                                        root: job.request.root,
                                        query: job.request.query,
                                        generation: job.request.generation,
                                        matches,
                                    },
                                ))
                                .await;
                        }
                        Ok(None) => {}
                        Err(error) => log::warn!(
                            "[file_explorer] search worker failed generation={}: {error}",
                            job.request.generation
                        ),
                    }
                }
            }
        })
        .detach();

        Self { state, wake }
    }

    pub(crate) fn submit(
        &self,
        request: FileExplorerSearchRequest,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(FileExplorerSearchJob {
                request,
                ingress,
                abort: Arc::new(AtomicBool::new(false)),
                canceled: tokio_util::sync::CancellationToken::new(),
            });
        self.wake.request();
    }
}

fn execute_local_file_explorer_search(
    request: FileExplorerSearchRequest,
    abort: &AtomicBool,
) -> Vec<ExplorerPath> {
    let start = Instant::now();
    let Some(root) = request.root.local_path() else {
        return Vec::new();
    };
    match crate::fff::search_file_explorer_available_cancellable(
        root,
        &request.query,
        &request.config,
        Some(abort),
    ) {
        Ok(matches) => {
            log::info!(
                "[file_explorer] search_load_done root={} query={:?} generation={} cancelled={} matches={} first_match={} elapsed_us={}",
                request.root.display(),
                request.query,
                request.generation,
                abort.load(Ordering::Acquire),
                matches.len(),
                matches
                    .first()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| String::from("<none>")),
                start.elapsed().as_micros(),
            );
            matches.into_iter().map(ExplorerPath::Local).collect()
        }
        Err(error) => {
            log::debug!(
                "failed to query FFF file explorer search for {} query={:?}: {error:#}",
                request.root.display(),
                request.query
            );
            Vec::new()
        }
    }
}

async fn execute_remote_file_explorer_search(
    request: &FileExplorerSearchRequest,
    ingress: &crate::runtime::RuntimeIngress,
    canceled: tokio_util::sync::CancellationToken,
) -> Result<(), helix_remote::backend::BackendError> {
    let remote = request
        .source
        .remote()
        .expect("remote search request has a remote source");
    let mut search = remote
        .search_files(
            request
                .root
                .remote_path()
                .cloned()
                .unwrap_or_else(helix_remote::WorkspacePath::root),
            request.query.clone(),
            request.config.workspace_scan_options(),
            100_000,
            canceled.child_token(),
        )
        .await?;

    loop {
        let snapshot = tokio::select! {
            _ = canceled.cancelled() => {
                return search.cancel().await;
            }
            snapshot = search.next() => snapshot,
        };
        let Some(snapshot) = snapshot else {
            return Ok(());
        };
        let matches = snapshot
            .entries
            .into_iter()
            .map(|entry| ExplorerPath::Remote(entry.path))
            .collect();
        let _ = ingress
            .send_ui(UiCommand::FileExplorer(
                FileExplorerCommand::ApplySearchResults {
                    source: request.source_id.clone(),
                    root: request.root.clone(),
                    query: request.query.clone(),
                    generation: request.generation,
                    matches,
                },
            ))
            .await;
        if snapshot.done {
            return Ok(());
        }
    }
}

async fn execute_collaboration_file_explorer_search(
    request: &FileExplorerSearchRequest,
    abort: &Arc<AtomicBool>,
    canceled: tokio_util::sync::CancellationToken,
    block: helix_runtime::Block,
) -> anyhow::Result<Vec<ExplorerPath>> {
    let session = request
        .source
        .collaboration()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("collaboration search has a local source"))?;
    let files = session
        .list_files(request.config.workspace_scan_options())
        .await?;
    if canceled.is_cancelled() || abort.load(Ordering::Acquire) {
        return Ok(Vec::new());
    }
    let query = request.query.clone();
    let root = request
        .root
        .collaboration_path()
        .cloned()
        .unwrap_or_else(helix_remote::WorkspacePath::root);
    let project = session.project().id;
    let abort = abort.clone();
    block
        .spawn(move || {
            use nucleo::{
                pattern::{Atom, AtomKind, CaseMatching, Normalization},
                Utf32Str,
            };

            let mut matcher = nucleo::Matcher::default();
            matcher.config.set_match_paths();
            let pattern = Atom::new(
                &query,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Fuzzy,
                false,
            );
            let mut utf32 = Vec::new();
            let mut matches = Vec::new();
            for (index, path) in files.iter().enumerate() {
                if index % 256 == 0 && abort.load(Ordering::Acquire) {
                    return Vec::new();
                }
                if !path.starts_with(&root) {
                    continue;
                }
                let display = path.to_string();
                if let Some(score) =
                    pattern.score(Utf32Str::new(&display, &mut utf32), &mut matcher)
                {
                    matches.push((score, path));
                }
            }
            matches.sort_unstable_by(|(left_score, left), (right_score, right)| {
                right_score.cmp(left_score).then_with(|| left.cmp(right))
            });
            matches.truncate(100_000);
            matches
                .into_iter()
                .map(|(_, path)| ExplorerPath::Collaboration {
                    project,
                    path: path.clone(),
                })
                .collect()
        })
        .await
        .map_err(|error| anyhow::anyhow!("collaboration search worker failed: {error}"))
}

struct FileExplorerApplyContext<'a> {
    editor: &'a mut Editor,
    ingress: crate::runtime::RuntimeIngress,
}

fn file_explorer_command_name(cmd: &FileExplorerCommand) -> &'static str {
    match cmd {
        FileExplorerCommand::ToggleSourceOption { .. } => "ToggleSourceOption",
        FileExplorerCommand::RefreshCollaboration { .. } => "RefreshCollaboration",
        FileExplorerCommand::FileOperationCompleted { .. } => "FileOperationCompleted",
        FileExplorerCommand::ApplyTree { .. } => "ApplyTree",
        FileExplorerCommand::PreviewSelection { .. } => "PreviewSelection",
        FileExplorerCommand::ApplyPreview { .. } => "ApplyPreview",
        FileExplorerCommand::ApplyVcsSnapshot { .. } => "ApplyVcsSnapshot",
        FileExplorerCommand::StartSearch { .. } => "StartSearch",
        FileExplorerCommand::ApplySearchResults { .. } => "ApplySearchResults",
        FileExplorerCommand::ApplyWorkspaceTransaction { .. } => "ApplyWorkspaceTransaction",
        FileExplorerCommand::ApplyWorkspacePaste { .. } => "ApplyWorkspacePaste",
        FileExplorerCommand::PromptWorkspaceDelete { .. } => "PromptWorkspaceDelete",
        FileExplorerCommand::ReplayWorkspaceTransaction { .. } => "ReplayWorkspaceTransaction",
        FileExplorerCommand::WorkspaceTransactionCompleted { .. } => {
            "WorkspaceTransactionCompleted"
        }
        FileExplorerCommand::ApplyCreate { .. } => "ApplyCreate",
        FileExplorerCommand::ApplyMove { .. } => "ApplyMove",
        FileExplorerCommand::PromptDelete { .. } => "PromptDelete",
        FileExplorerCommand::ApplyConfirmedDelete { .. } => "ApplyConfirmedDelete",
        FileExplorerCommand::PromptCopy { .. } => "PromptCopy",
        FileExplorerCommand::ApplyCopy { .. } => "ApplyCopy",
        FileExplorerCommand::PromptSaveBefore { .. } => "PromptSaveBefore",
    }
}

fn spawn_file_explorer_command(cx: &mut crate::compositor::Context, command: FileExplorerCommand) {
    cx.spawn_ui(async move { Ok(UiCommand::FileExplorer(command)) });
}

fn notify_file_explorer_confirmation(editor: &mut Editor, message: impl Into<String>) {
    editor.notify_warning(format!("File explorer: {}", message.into()));
}

fn notify_file_explorer_info(editor: &mut Editor, message: impl Into<String>) {
    editor.notify_info(format!("File explorer: {}", message.into()));
}

fn notify_file_explorer_error(editor: &mut Editor, message: impl Into<String>) {
    editor.notify_error(format!("File explorer: {}", message.into()));
}

fn notify_file_explorer_result(editor: &mut Editor, result: Result<String, String>) {
    match result {
        Ok(message) => notify_file_explorer_info(editor, message),
        Err(message) => notify_file_explorer_error(editor, message),
    }
}

fn validate_explorer_descendant(
    root: &Path,
    path: &Path,
    operation: &str,
    allow_root: bool,
) -> Result<(), String> {
    let root = helix_stdx::path::canonicalize(root);
    let path = helix_stdx::path::canonicalize(path);
    if !path.starts_with(&root) {
        return Err(format!(
            "Refusing to {operation} {} because it is outside explorer root {}",
            path.display(),
            root.display()
        ));
    }
    if !allow_root && path == root {
        return Err(format!(
            "Refusing to {operation} the explorer root {}",
            root.display()
        ));
    }
    Ok(())
}

fn validate_explorer_destination(
    root: &Path,
    destination: &helix_view::editor::FileOperationDestination,
    operation: &str,
) -> Result<(), String> {
    let (path, allow_root) = match destination {
        helix_view::editor::FileOperationDestination::Exact(path) => (path, false),
        helix_view::editor::FileOperationDestination::PathOrDirectory(path)
        | helix_view::editor::FileOperationDestination::UniqueInDirectory(path) => (path, true),
    };
    validate_explorer_descendant(root, path, operation, allow_root)
}

fn exact_destination(
    destination: &helix_view::editor::FileOperationDestination,
) -> Option<PathBuf> {
    match destination {
        helix_view::editor::FileOperationDestination::Exact(path) => Some(path.clone()),
        helix_view::editor::FileOperationDestination::PathOrDirectory(_)
        | helix_view::editor::FileOperationDestination::UniqueInDirectory(_) => None,
    }
}

fn same_explorer_root(left: &Path, right: &Path) -> bool {
    helix_stdx::path::canonicalize(left) == helix_stdx::path::canonicalize(right)
}

fn queue_file_explorer_command(
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
    command: FileExplorerCommand,
) {
    editor
        .work()
        .spawn(async move {
            crate::runtime::send_ui_command_with(UiCommand::FileExplorer(command), ingress).await;
        })
        .detach();
}

fn local_vcs_snapshot_root(root: ExplorerPath) -> Option<PathBuf> {
    root.into_local()
}

pub(crate) fn queue_file_explorer_vcs_snapshot(
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
    root: ExplorerPath,
) {
    if !editor.config().file_explorer.vcs {
        return;
    }
    let Some(root) = local_vcs_snapshot_root(root) else {
        return;
    };

    let diff_providers = editor.diff_providers.clone();
    UiSnapshotRequest::new("[file_explorer] vcs_snapshot", root)
        .load_with(move |root| {
            diff_providers
                .changed_files(&root)
                .map(|changes| crate::ui::VcsSnapshot::from_changes(&root, changes))
        })
        .apply_with(|root, snapshot| {
            UiCommand::FileExplorer(FileExplorerCommand::ApplyVcsSnapshot { root, snapshot })
        })
        .spawn(editor.work(), editor.runtime().block().clone(), ingress);
}

fn queue_file_explorer_search(
    ingress: crate::runtime::RuntimeIngress,
    source: ExplorerSource,
    root: ExplorerPath,
    query: String,
    generation: u64,
    config: helix_view::editor::FileExplorerConfig,
) {
    log::info!(
        "[file_explorer] search_enqueue root={} query={query:?} generation={} hidden={} ignore={} git_ignore={} git_global={} git_exclude={} follow_symlinks={}",
        root.display(),
        generation,
        config.hidden,
        config.ignore,
        config.git_ignore,
        config.git_global,
        config.git_exclude,
        config.follow_symlinks,
    );
    let request = FileExplorerSearchRequest {
        source_id: source.identity(),
        source,
        root,
        query,
        generation,
        config,
    };
    ingress.file_explorer_search(request);
}

pub(crate) fn queue_file_explorer_tree_refresh(
    panel: &mut FileExplorerPanel,
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
    request: crate::ui::FileExplorerTreeRefresh,
) {
    let work = panel.prepare_tree_refresh(editor, request);
    ingress.file_explorer_tree(work);
}

pub(crate) fn open_file_explorer(
    compositor: &mut Compositor,
    cx: &mut crate::compositor::Context,
    root: ExplorerPath,
) {
    let root = match root {
        ExplorerPath::Local(root) => ExplorerPath::Local(helix_stdx::path::normalize(root)),
        root => root,
    };
    let source = match ExplorerSource::from_root(root, &cx.editor.workspace_backend) {
        Ok(source) => source,
        Err(error) => {
            notify_file_explorer_error(cx.editor, format!("Failed to open file explorer: {error}"));
            return;
        }
    };
    let explorer_root = source.root().clone();
    let source_id = source.identity();

    if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
        if panel.root_path() == &explorer_root && panel.source_identity() == source_id {
            panel.focus_panel(cx.editor);
            log::info!(
                "[file_explorer] open_reuse root={} reason=already_open",
                explorer_root.display()
            );
            return;
        }
        panel.dismiss_panel(cx.editor, &cx.ingress);
        compositor.remove(FILE_EXPLORER_ID);
    }

    let mut panel = FileExplorerPanel::new_deferred(source, cx.editor);
    let refresh = if let Some(state) =
        FileExplorerPanel::take_matching_session(panel.root_path(), &panel.source_identity())
    {
        let selected = state.selected_path.clone();
        panel.restore_ui_state(state);
        log::info!(
            "[file_explorer] open_restore root={} selected={}",
            explorer_root.display(),
            selected
                .as_ref()
                .map(|path| path.display())
                .unwrap_or_else(|| "-".into()),
        );
        FileExplorerTreeRefresh::preserve().selecting(selected)
    } else {
        log::info!(
            "[file_explorer] open_fresh root={} reason=no_session",
            explorer_root.display()
        );
        FileExplorerTreeRefresh::follow_current_file()
    };

    queue_file_explorer_tree_refresh(&mut panel, cx.editor, cx.ingress.clone(), refresh);
    compositor.push(Box::new(panel));
    queue_file_explorer_vcs_snapshot(cx.editor, cx.ingress.clone(), explorer_root);
}

fn refresh_file_explorer_panel(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    root: PathBuf,
    cursor: u32,
) {
    let start = Instant::now();
    let requested_root = root.clone();
    let cursor = usize::try_from(cursor).unwrap_or(usize::MAX);
    if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
        queue_file_explorer_tree_refresh(
            panel,
            editor,
            ingress.clone(),
            FileExplorerTreeRefresh::invalidate_cache()
                .at_root(ExplorerPath::Local(root))
                .at_cursor(cursor),
        );
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=true root={} cursor={} elapsed_us={}",
            requested_root.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, ExplorerPath::Local(requested_root));
    } else {
        let source =
            match ExplorerSource::from_root(ExplorerPath::Local(root), &editor.workspace_backend) {
                Ok(source) => source,
                Err(error) => {
                    notify_file_explorer_error(
                        editor,
                        format!("Failed to refresh file explorer: {error}"),
                    );
                    return;
                }
            };
        let mut panel = FileExplorerPanel::new_deferred(source, editor);
        queue_file_explorer_tree_refresh(
            &mut panel,
            editor,
            ingress.clone(),
            FileExplorerTreeRefresh::preserve().at_cursor(cursor),
        );
        compositor.push(Box::new(panel));
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=false root={} cursor={} elapsed_us={}",
            requested_root.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, ExplorerPath::Local(requested_root));
    }
}

fn refresh_file_explorer_panel_selecting_path(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    root: PathBuf,
    path: PathBuf,
    cursor: u32,
) {
    let start = Instant::now();
    let requested_root = root.clone();
    let requested_path = path.clone();
    let cursor = usize::try_from(cursor).unwrap_or(usize::MAX);
    if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
        queue_file_explorer_tree_refresh(
            panel,
            editor,
            ingress.clone(),
            FileExplorerTreeRefresh::invalidate_cache()
                .at_root(ExplorerPath::Local(root))
                .at_cursor(cursor)
                .selecting_path(ExplorerPath::Local(path)),
        );
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=true root={} select_path={} fallback_cursor={} elapsed_us={}",
            requested_root.display(),
            requested_path.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, ExplorerPath::Local(requested_root));
    } else {
        let source =
            match ExplorerSource::from_root(ExplorerPath::Local(root), &editor.workspace_backend) {
                Ok(source) => source,
                Err(error) => {
                    notify_file_explorer_error(
                        editor,
                        format!("Failed to refresh file explorer: {error}"),
                    );
                    return;
                }
            };
        let mut panel = FileExplorerPanel::new_deferred(source, editor);
        queue_file_explorer_tree_refresh(
            &mut panel,
            editor,
            ingress.clone(),
            FileExplorerTreeRefresh::preserve()
                .at_cursor(cursor)
                .selecting_path(ExplorerPath::Local(path)),
        );
        compositor.push(Box::new(panel));
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=false root={} select_path={} fallback_cursor={} elapsed_us={}",
            requested_root.display(),
            requested_path.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, ExplorerPath::Local(requested_root));
    }
}

fn path_affects_document(path: &Path, document_path: &Path) -> bool {
    document_path == path || document_path.starts_with(path)
}

fn modified_documents_for_paths(editor: &Editor, paths: &[PathBuf]) -> Vec<DocumentId> {
    let mut documents = Vec::new();
    for doc in editor.documents() {
        if !doc.is_modified() {
            continue;
        }
        let Some(path) = doc.path() else {
            continue;
        };
        if paths
            .iter()
            .any(|operation_path| path_affects_document(operation_path, path))
            && !documents.contains(&doc.id())
        {
            documents.push(doc.id());
        }
    }
    documents
}

fn workspace_transaction_paths(
    transaction: &helix_workspace::FileTransaction,
) -> Vec<&helix_workspace::WorkspacePath> {
    transaction
        .operations
        .iter()
        .flat_map(|operation| match operation {
            helix_remote::FileOperation::CreateFile { path, .. }
            | helix_remote::FileOperation::CreateDirectory { path }
            | helix_remote::FileOperation::Remove { path, .. } => vec![path],
            helix_remote::FileOperation::Copy { from, to, .. }
            | helix_remote::FileOperation::Rename { from, to, .. } => vec![from, to],
        })
        .collect()
}

fn modified_documents_for_workspace_paths(
    editor: &Editor,
    root: &ExplorerPath,
    paths: &[&helix_workspace::WorkspacePath],
) -> Vec<DocumentId> {
    editor
        .documents()
        .filter(|document| document.is_modified())
        .filter_map(|document| {
            let path = match root {
                ExplorerPath::Remote(_) => {
                    document.remote_location().map(|location| &location.path)
                }
                ExplorerPath::Collaboration { project, .. } => document
                    .collaboration_location()
                    .filter(|location| location.project == *project)
                    .map(|location| &location.path),
                ExplorerPath::Local(_) => None,
            }?;
            paths
                .iter()
                .any(|operation| path == *operation || path.starts_with(operation))
                .then_some(document.id())
        })
        .collect()
}

#[derive(Clone)]
enum WorkspaceFileBackend {
    Remote(std::sync::Arc<helix_remote::backend::RemoteWorkspaceClient>),
    Collaboration(helix_collab::GuestSessionHandle),
}

impl WorkspaceFileBackend {
    async fn apply(&self, transaction: helix_workspace::FileTransaction) -> Result<bool, String> {
        match self {
            Self::Remote(remote) => remote
                .apply_file_transaction(transaction)
                .await
                .map(|_| true)
                .map_err(|error| error.to_string()),
            Self::Collaboration(session) => session
                .apply_file_transaction(transaction)
                .await
                .map_err(|error| error.to_string()),
        }
    }

    async fn replay(&self, redo: bool) -> Result<bool, String> {
        match self {
            Self::Remote(remote) => if redo {
                remote.redo_file_transaction().await
            } else {
                remote.undo_file_transaction().await
            }
            .map_err(|error| error.to_string()),
            Self::Collaboration(session) => session
                .replay_file_transaction(redo)
                .await
                .map_err(|error| error.to_string()),
        }
    }

    async fn path_exists(&self, path: helix_workspace::WorkspacePath) -> Result<bool, String> {
        match self {
            Self::Remote(remote) => remote
                .stat(path, tokio_util::sync::CancellationToken::new())
                .await
                .map(|metadata| metadata.is_some())
                .map_err(|error| error.to_string()),
            Self::Collaboration(session) => session
                .path_exists(path)
                .await
                .map_err(|error| error.to_string()),
        }
    }
}

fn workspace_file_backend(
    editor: &Editor,
    root: &ExplorerPath,
) -> Result<WorkspaceFileBackend, &'static str> {
    match root {
        ExplorerPath::Remote(_) if editor.collaboration.is_hosting() => editor
            .collaboration
            .session()
            .map(WorkspaceFileBackend::Collaboration)
            .ok_or("shared remote project is no longer connected"),
        ExplorerPath::Remote(_) => editor
            .workspace_backend
            .remote()
            .cloned()
            .map(WorkspaceFileBackend::Remote)
            .ok_or("remote workspace is no longer connected"),
        ExplorerPath::Collaboration { project, .. } => editor
            .workspace_backend
            .collaboration()
            .filter(|session| session.project().id == *project)
            .cloned()
            .map(WorkspaceFileBackend::Collaboration)
            .ok_or("shared project is no longer connected"),
        ExplorerPath::Local(_) => Err("local path was routed to a workspace transaction"),
    }
}

fn save_modified_documents(
    cx: &mut crate::compositor::Context,
    documents: &[DocumentId],
) -> anyhow::Result<()> {
    for doc_id in documents.iter().copied() {
        let Some(doc) = cx.editor.document(doc_id) else {
            continue;
        };
        if !doc.is_modified() {
            continue;
        }

        append_document_changes_to_history(cx.editor, doc_id);
        cx.editor.save(doc_id, None, SavePolicy::Safe)?;
    }
    Ok(())
}

fn append_document_changes_to_history(editor: &mut Editor, doc_id: DocumentId) {
    let Some(view_id) = editor
        .tree
        .views()
        .find_map(|(view, focused)| (focused && view.doc == doc_id).then_some(view.id))
        .or_else(|| {
            editor
                .tree
                .views()
                .find_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
        })
    else {
        return;
    };

    let view = helix_view::view_mut!(editor, view_id);
    let doc = helix_view::doc_mut!(editor, &doc_id);
    doc.append_changes_to_history(view);
}

fn without_modified_buffer_check(mut command: FileExplorerCommand) -> FileExplorerCommand {
    match &mut command {
        FileExplorerCommand::ApplyCreate {
            modified_buffer_check,
            ..
        }
        | FileExplorerCommand::ApplyMove {
            modified_buffer_check,
            ..
        }
        | FileExplorerCommand::ApplyConfirmedDelete {
            modified_buffer_check,
            ..
        }
        | FileExplorerCommand::ApplyCopy {
            modified_buffer_check,
            ..
        }
        | FileExplorerCommand::ApplyWorkspaceTransaction {
            modified_buffer_check,
            ..
        }
        | FileExplorerCommand::ApplyWorkspacePaste {
            modified_buffer_check,
            ..
        } => *modified_buffer_check = ModifiedBufferCheck::Skip,
        _ => {}
    }
    command
}

fn prompt_save_before_modified_documents(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    operation: String,
    paths: &[PathBuf],
    continuation: FileExplorerCommand,
) -> bool {
    let documents = modified_documents_for_paths(editor, paths);
    if documents.is_empty() {
        return false;
    }

    queue_file_explorer_command(
        editor,
        ingress,
        FileExplorerCommand::PromptSaveBefore {
            operation,
            documents,
            continuation: Box::new(continuation),
        },
    );
    true
}

fn prompt_save_before_workspace_transaction(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    operation: String,
    root: &ExplorerPath,
    transaction: &helix_workspace::FileTransaction,
    continuation: FileExplorerCommand,
) -> bool {
    let paths = workspace_transaction_paths(transaction);
    let documents = modified_documents_for_workspace_paths(editor, root, &paths);
    if documents.is_empty() {
        return false;
    }
    queue_file_explorer_command(
        editor,
        ingress,
        FileExplorerCommand::PromptSaveBefore {
            operation,
            documents,
            continuation: Box::new(continuation),
        },
    );
    true
}

fn apply_workspace_transaction(
    cx: &mut FileExplorerApplyContext<'_>,
    root: ExplorerPath,
    cursor: u32,
    select_path: Option<ExplorerPath>,
    transaction: helix_workspace::FileTransaction,
    success: String,
    modified_buffer_check: ModifiedBufferCheck,
) {
    let command = FileExplorerCommand::ApplyWorkspaceTransaction {
        root: root.clone(),
        cursor,
        select_path: select_path.clone(),
        transaction: transaction.clone(),
        success: success.clone(),
        modified_buffer_check,
    };
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_workspace_transaction(
            cx.editor,
            cx.ingress.clone(),
            success.clone(),
            &root,
            &transaction,
            command,
        )
    {
        return;
    }
    let backend = match workspace_file_backend(cx.editor, &root) {
        Ok(backend) => backend,
        Err(error) => {
            notify_file_explorer_error(cx.editor, error);
            return;
        }
    };
    let ingress = cx.ingress.clone();
    cx.editor
        .work()
        .spawn(async move {
            let result = backend.apply(transaction).await.and_then(|changed| {
                changed
                    .then_some(success)
                    .ok_or("No file changes".to_owned())
            });
            let _ = ingress
                .send_ui(UiCommand::FileExplorer(
                    FileExplorerCommand::WorkspaceTransactionCompleted {
                        root,
                        cursor,
                        select_path,
                        result,
                    },
                ))
                .await;
        })
        .detach();
}

fn replay_workspace_transaction(
    cx: &mut FileExplorerApplyContext<'_>,
    root: ExplorerPath,
    cursor: u32,
    redo: bool,
) {
    let backend = match workspace_file_backend(cx.editor, &root) {
        Ok(backend) => backend,
        Err(error) => {
            notify_file_explorer_error(cx.editor, error);
            return;
        }
    };
    let ingress = cx.ingress.clone();
    cx.editor
        .work()
        .spawn(async move {
            let result = backend.replay(redo).await.and_then(|changed| {
                changed
                    .then(|| {
                        if redo {
                            String::from("Redid file operation")
                        } else {
                            String::from("Undid file operation")
                        }
                    })
                    .ok_or_else(|| String::from("No file operation to replay"))
            });
            let _ = ingress
                .send_ui(UiCommand::FileExplorer(
                    FileExplorerCommand::WorkspaceTransactionCompleted {
                        root,
                        cursor,
                        select_path: None,
                        result,
                    },
                ))
                .await;
        })
        .detach();
}

fn apply_workspace_paste(
    cx: &mut FileExplorerApplyContext<'_>,
    root: ExplorerPath,
    cursor: u32,
    source: helix_workspace::WorkspacePath,
    destination: helix_workspace::WorkspacePath,
    move_source: bool,
    modified_buffer_check: ModifiedBufferCheck,
) {
    let command = FileExplorerCommand::ApplyWorkspacePaste {
        root: root.clone(),
        cursor,
        source: source.clone(),
        destination: destination.clone(),
        move_source,
        modified_buffer_check,
    };
    let inspection = helix_remote::FileTransaction {
        operations: vec![helix_remote::FileOperation::Copy {
            from: source.clone(),
            to: destination.clone(),
            overwrite: false,
        }],
    };
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_workspace_transaction(
            cx.editor,
            cx.ingress.clone(),
            if move_source {
                format!("moving {source}")
            } else {
                format!("copying {source}")
            },
            &root,
            &inspection,
            command,
        )
    {
        return;
    }
    let backend = match workspace_file_backend(cx.editor, &root) {
        Ok(backend) => backend,
        Err(error) => {
            notify_file_explorer_error(cx.editor, error);
            return;
        }
    };
    let ingress = cx.ingress.clone();
    cx.editor
        .work()
        .spawn(async move {
            let result = async {
                let target = workspace_unique_destination(&backend, &destination, &source).await?;
                let operation = if move_source {
                    helix_remote::FileOperation::Rename {
                        from: source,
                        to: target.clone(),
                        overwrite: false,
                    }
                } else {
                    helix_remote::FileOperation::Copy {
                        from: source,
                        to: target.clone(),
                        overwrite: false,
                    }
                };
                backend
                    .apply(helix_workspace::FileTransaction {
                        operations: vec![operation],
                    })
                    .await?;
                Ok::<_, String>((target, if move_source { "Moved" } else { "Copied" }))
            }
            .await;
            let (select_path, result) = match result {
                Ok((target, verb)) => (
                    root.with_workspace_path(target),
                    Ok(format!("{verb} workspace path")),
                ),
                Err(error) => (None, Err(error.to_string())),
            };
            let _ = ingress
                .send_ui(UiCommand::FileExplorer(
                    FileExplorerCommand::WorkspaceTransactionCompleted {
                        root,
                        cursor,
                        select_path,
                        result,
                    },
                ))
                .await;
        })
        .detach();
}

async fn workspace_unique_destination(
    backend: &WorkspaceFileBackend,
    directory: &helix_workspace::WorkspacePath,
    source: &helix_workspace::WorkspacePath,
) -> Result<helix_workspace::WorkspacePath, String> {
    let name = source
        .file_name()
        .ok_or_else(|| String::from("source path has no file name"))?;
    let candidate = directory
        .join(name.to_owned())
        .map_err(|error| error.to_string())?;
    if !backend.path_exists(candidate.clone()).await? {
        return Ok(candidate);
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    for index in 1_u64.. {
        let name = extension.map_or_else(
            || format!("{stem} ({index})"),
            |extension| format!("{stem} ({index}).{extension}"),
        );
        let candidate = directory.join(name).map_err(|error| error.to_string())?;
        if !backend.path_exists(candidate.clone()).await? {
            return Ok(candidate);
        }
    }
    unreachable!("unbounded unique destination loop")
}

fn apply_create(
    cx: &mut FileExplorerApplyContext<'_>,
    root: PathBuf,
    cursor: u32,
    is_dir: bool,
    target: PathBuf,
    modified_buffer_check: ModifiedBufferCheck,
) {
    let command = FileExplorerCommand::ApplyCreate {
        root: root.clone(),
        cursor,
        is_dir,
        target: target.clone(),
        modified_buffer_check,
    };
    if let Err(error) = validate_explorer_descendant(&root, &target, "create", false) {
        notify_file_explorer_error(cx.editor, error);
        return;
    }
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_modified_documents(
            cx.editor,
            cx.ingress.clone(),
            format!("creating {}", target.display()),
            std::slice::from_ref(&target),
            command,
        )
    {
        return;
    }

    crate::effect::file_operation::submit(
        cx.editor,
        cx.ingress.clone(),
        helix_view::editor::FileOperationRequest::create(
            helix_view::editor::FileOperationOrigin::Explorer {
                root,
                cursor,
                select_path: Some(target.clone()),
            },
            target,
            is_dir,
        ),
    );
}

fn apply_move(
    cx: &mut FileExplorerApplyContext<'_>,
    source: PathBuf,
    root: PathBuf,
    cursor: u32,
    destination: helix_view::editor::FileOperationDestination,
    modified_buffer_check: ModifiedBufferCheck,
) {
    let command = FileExplorerCommand::ApplyMove {
        source: source.clone(),
        root: root.clone(),
        cursor,
        destination: destination.clone(),
        modified_buffer_check,
    };
    if let Err(error) = validate_explorer_descendant(&root, &source, "move", false)
        .and_then(|()| validate_explorer_destination(&root, &destination, "move to"))
    {
        notify_file_explorer_error(cx.editor, error);
        return;
    }
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_modified_documents(
            cx.editor,
            cx.ingress.clone(),
            format!("moving {}", source.display()),
            std::slice::from_ref(&source),
            command,
        )
    {
        return;
    }

    crate::effect::file_operation::submit(
        cx.editor,
        cx.ingress.clone(),
        helix_view::editor::FileOperationRequest::move_to_destination(
            helix_view::editor::FileOperationOrigin::Explorer {
                root,
                cursor,
                select_path: exact_destination(&destination),
            },
            source,
            destination,
            true,
        ),
    );
}

fn apply_confirmed_delete(
    cx: &mut FileExplorerApplyContext<'_>,
    target: PathBuf,
    root: PathBuf,
    cursor: u32,
    modified_buffer_check: ModifiedBufferCheck,
) {
    if let Err(error) = validate_explorer_descendant(&root, &target, "move to trash", false) {
        notify_file_explorer_error(cx.editor, error);
        return;
    }
    let command = FileExplorerCommand::ApplyConfirmedDelete {
        target: target.clone(),
        root: root.clone(),
        cursor,
        modified_buffer_check,
    };
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_modified_documents(
            cx.editor,
            cx.ingress.clone(),
            format!("deleting {}", target.display()),
            std::slice::from_ref(&target),
            command,
        )
    {
        return;
    }

    crate::effect::file_operation::submit(
        cx.editor,
        cx.ingress.clone(),
        helix_view::editor::FileOperationRequest::trash(
            helix_view::editor::FileOperationOrigin::Explorer {
                root,
                cursor,
                select_path: None,
            },
            target,
        ),
    );
}

fn apply_copy(
    cx: &mut FileExplorerApplyContext<'_>,
    source: PathBuf,
    root: PathBuf,
    cursor: u32,
    destination: helix_view::editor::FileOperationDestination,
    modified_buffer_check: ModifiedBufferCheck,
) {
    let command = FileExplorerCommand::ApplyCopy {
        source: source.clone(),
        root: root.clone(),
        cursor,
        destination: destination.clone(),
        modified_buffer_check,
    };
    if let Err(error) = validate_explorer_descendant(&root, &source, "copy", false) {
        notify_file_explorer_error(cx.editor, error);
        return;
    }
    if matches!(
        &destination,
        helix_view::editor::FileOperationDestination::UniqueInDirectory(_)
    ) {
        if let Err(error) = validate_explorer_destination(&root, &destination, "copy to") {
            notify_file_explorer_error(cx.editor, error);
            return;
        }
    }
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_modified_documents(
            cx.editor,
            cx.ingress.clone(),
            format!("copying {}", source.display()),
            std::slice::from_ref(&source),
            command,
        )
    {
        return;
    }

    crate::effect::file_operation::submit(
        cx.editor,
        cx.ingress.clone(),
        helix_view::editor::FileOperationRequest::copy_path(
            helix_view::editor::FileOperationOrigin::Explorer {
                root,
                cursor,
                select_path: exact_destination(&destination),
            },
            source,
            destination,
        ),
    );
}

pub(crate) fn apply_file_explorer_command(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    cmd: FileExplorerCommand,
) {
    let command_name = file_explorer_command_name(&cmd);
    let command_start = Instant::now();
    log::info!("[file_explorer] command_apply_start command={command_name}");
    match cmd {
        FileExplorerCommand::ToggleSourceOption { option } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                panel.toggle_source_option(option);
                let cursor = panel.selection_for_log();
                queue_file_explorer_tree_refresh(
                    panel,
                    editor,
                    ingress.clone(),
                    FileExplorerTreeRefresh::invalidate_cache().at_cursor(cursor),
                );
                panel.queue_current_search(editor, ingress);
            }
        }
        FileExplorerCommand::RefreshCollaboration { project } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                let matches = panel.source_for_context().identity()
                    == ExplorerSourceId::Collaboration { project };
                if matches {
                    let cursor = panel.selection_for_log();
                    queue_file_explorer_tree_refresh(
                        panel,
                        editor,
                        ingress.clone(),
                        FileExplorerTreeRefresh::invalidate_cache().at_cursor(cursor),
                    );
                    panel.queue_current_search(editor, ingress);
                }
            }
        }
        FileExplorerCommand::FileOperationCompleted {
            root,
            cursor,
            select_path,
            result,
        } => {
            let matching_panel = compositor
                .find_id::<FileExplorerPanel>(FILE_EXPLORER_ID)
                .and_then(|panel| panel.root_for_context().local_path())
                .is_some_and(|panel_root| same_explorer_root(panel_root, &root));
            if result.is_ok() && matching_panel {
                if let Some(path) = select_path {
                    refresh_file_explorer_panel_selecting_path(
                        editor,
                        compositor,
                        ingress.clone(),
                        root,
                        path,
                        cursor,
                    );
                } else {
                    refresh_file_explorer_panel(editor, compositor, ingress.clone(), root, cursor);
                }
            } else if result.is_ok() {
                log::info!(
                    "[file_explorer] operation_refresh_skip root={} reason=stale_or_closed_panel",
                    root.display()
                );
            }
            notify_file_explorer_result(editor, result);
        }
        FileExplorerCommand::ApplyTree { root, generation } => {
            let Some(prepared) = ingress.take_file_explorer_tree(&root, generation) else {
                log::info!(
                    "[file_explorer] tree_apply_skip root={} generation={} reason=missing_prepared",
                    root.display(),
                    generation,
                );
                return;
            };
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                if panel.apply_prepared_tree(editor, prepared) {
                    panel.queue_selected_preview(editor, ingress.clone());
                }
            }
        }
        FileExplorerCommand::PreviewSelection {
            source,
            root,
            path,
            cursor,
            generation,
        } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                panel.apply_preview_request(
                    editor,
                    ingress.clone(),
                    FileExplorerPreviewRequest {
                        source,
                        root,
                        path,
                        cursor,
                        generation,
                    },
                );
                if panel.take_dismiss_after_open() {
                    panel.dismiss_panel(editor, &ingress);
                    compositor.remove(FILE_EXPLORER_ID);
                }
            }
        }
        FileExplorerCommand::ApplyPreview {
            source,
            root,
            path,
            cursor,
            generation,
        } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                panel.apply_prepared_preview(
                    editor,
                    ingress.clone(),
                    FileExplorerPreviewRequest {
                        source,
                        root,
                        path,
                        cursor,
                        generation,
                    },
                );
                if panel.take_dismiss_after_open() {
                    panel.dismiss_panel(editor, &ingress);
                    compositor.remove(FILE_EXPLORER_ID);
                }
            }
        }
        FileExplorerCommand::ApplyVcsSnapshot { root, snapshot } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                if panel.apply_vcs_snapshot_state(editor, root, snapshot) {
                    let cursor = panel.selection_for_log();
                    queue_file_explorer_tree_refresh(
                        panel,
                        editor,
                        ingress.clone(),
                        FileExplorerTreeRefresh::preserve().at_cursor(cursor),
                    );
                }
            }
        }
        FileExplorerCommand::StartSearch {
            source,
            root,
            query,
            generation,
            config,
        } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                let accepted = panel.source_for_context().identity() == source.identity()
                    && panel.accepts_search_request(&root, &query, generation);
                log::info!(
                    "[file_explorer] search_start_request root={} query={query:?} generation={} accepted={} panel_query={:?} panel_generation={} panel_pending={} rows={} selection={} selected={}",
                    root.display(),
                    generation,
                    accepted,
                    panel.search_query_for_log(),
                    panel.search_generation_for_log(),
                    panel.search_pending_for_log(),
                    panel.row_count_for_log(),
                    panel.selection_for_log(),
                    panel.selected_path_for_log(),
                );
                if accepted {
                    queue_file_explorer_search(ingress, source, root, query, generation, config);
                }
            } else {
                log::info!(
                    "[file_explorer] search_start_request root={} query={query:?} generation={} accepted=false reason=no_panel",
                    root.display(),
                    generation,
                );
            }
        }
        FileExplorerCommand::ApplySearchResults {
            source,
            root,
            query,
            generation,
            matches,
        } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                let applied = panel.source_for_context().identity() == source
                    && panel.apply_search_results(editor, root, query, generation, matches);
                log::info!(
                    "[file_explorer] search_results_command applied={} rows={} selection={} selected={} pending={} generation={}",
                    applied,
                    panel.row_count_for_log(),
                    panel.selection_for_log(),
                    panel.selected_path_for_log(),
                    panel.search_pending_for_log(),
                    panel.search_generation_for_log(),
                );
                if applied {
                    panel.queue_selected_preview(editor, ingress.clone());
                }
            } else {
                log::info!("[file_explorer] search_results_command applied=false reason=no_panel");
            }
        }
        FileExplorerCommand::ApplyWorkspaceTransaction {
            root,
            cursor,
            select_path,
            transaction,
            success,
            modified_buffer_check,
        } => apply_workspace_transaction(
            &mut FileExplorerApplyContext { editor, ingress },
            root,
            cursor,
            select_path,
            transaction,
            success,
            modified_buffer_check,
        ),
        FileExplorerCommand::ApplyWorkspacePaste {
            root,
            cursor,
            source,
            destination,
            move_source,
            modified_buffer_check,
        } => apply_workspace_paste(
            &mut FileExplorerApplyContext { editor, ingress },
            root,
            cursor,
            source,
            destination,
            move_source,
            modified_buffer_check,
        ),
        FileExplorerCommand::PromptWorkspaceDelete {
            root,
            cursor,
            target,
        } => {
            let kind = if matches!(root, ExplorerPath::Collaboration { .. }) {
                "shared"
            } else {
                "remote"
            };
            let message = format!("Delete {kind} path {target}?");
            let cancelled_target = target.clone();
            let confirmation = Confirmation::new(message, move |cx| {
                spawn_file_explorer_command(
                    cx,
                    FileExplorerCommand::ApplyWorkspaceTransaction {
                        root: root.clone(),
                        cursor,
                        select_path: None,
                        transaction: helix_workspace::FileTransaction {
                            operations: vec![helix_workspace::FileOperation::Remove {
                                path: target.clone(),
                                recursive: true,
                            }],
                        },
                        success: format!("Deleted {kind} path {target}"),
                        modified_buffer_check: ModifiedBufferCheck::Prompt,
                    },
                );
            })
            .on_cancel(move |cx| {
                cx.editor
                    .notify_info(format!("Delete canceled: {cancelled_target}"));
            });
            compositor.push(Box::new(confirmation.into_prompt()));
        }
        FileExplorerCommand::ReplayWorkspaceTransaction { root, cursor, redo } => {
            replay_workspace_transaction(
                &mut FileExplorerApplyContext { editor, ingress },
                root,
                cursor,
                redo,
            );
        }
        FileExplorerCommand::WorkspaceTransactionCompleted {
            root,
            cursor,
            select_path,
            result,
        } => {
            let matching_panel = compositor
                .find_id::<FileExplorerPanel>(FILE_EXPLORER_ID)
                .is_some_and(|panel| panel.root_for_context() == &root);
            if result.is_ok() && matching_panel {
                if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                    queue_file_explorer_tree_refresh(
                        panel,
                        editor,
                        ingress.clone(),
                        FileExplorerTreeRefresh::invalidate_cache()
                            .at_root(root)
                            .at_cursor(usize::try_from(cursor).unwrap_or(usize::MAX))
                            .selecting(select_path),
                    );
                    panel.queue_current_search(editor, ingress.clone());
                }
            }
            notify_file_explorer_result(editor, result);
        }
        FileExplorerCommand::PromptDelete {
            target,
            root,
            cursor,
        } => {
            if let Err(error) = validate_explorer_descendant(&root, &target, "move to trash", false)
            {
                notify_file_explorer_error(editor, error);
                return;
            }
            let message = format!("Move {} to trash?", target.display());
            notify_file_explorer_confirmation(editor, format!("{message} Enter y to confirm."));
            let cancelled_target = target.clone();
            let confirmation = Confirmation::new(message, move |cx| {
                spawn_file_explorer_command(
                    cx,
                    FileExplorerCommand::ApplyConfirmedDelete {
                        target: target.clone(),
                        root: root.clone(),
                        cursor,
                        modified_buffer_check: ModifiedBufferCheck::Prompt,
                    },
                );
            })
            .on_cancel(move |cx| {
                notify_file_explorer_info(
                    cx.editor,
                    format!("Cancelled trash: {}", cancelled_target.display()),
                );
            });

            compositor.push(Box::new(confirmation.into_prompt()));
        }
        FileExplorerCommand::PromptCopy {
            source,
            root,
            cursor,
            prefill,
        } => {
            let prompt = Prompt::new(
                format!("Copy {} -> ", source.display()).into(),
                None,
                crate::ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }

                    let copy_to_string = input.to_owned();
                    let copy_to = helix_stdx::path::expand_tilde(PathBuf::from(&copy_to_string));

                    spawn_file_explorer_command(
                        cx,
                        FileExplorerCommand::ApplyCopy {
                            source: source.clone(),
                            root: root.clone(),
                            cursor,
                            destination: helix_view::editor::FileOperationDestination::Exact(
                                copy_to.to_path_buf(),
                            ),
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    );
                },
            )
            .with_line(prefill, editor);

            compositor.push(Box::new(prompt));
        }
        FileExplorerCommand::ApplyCreate {
            root,
            cursor,
            is_dir,
            target,
            modified_buffer_check,
        } => {
            let mut cx = FileExplorerApplyContext { editor, ingress };
            apply_create(&mut cx, root, cursor, is_dir, target, modified_buffer_check);
        }
        FileExplorerCommand::ApplyMove {
            source,
            root,
            cursor,
            destination,
            modified_buffer_check,
        } => {
            let mut cx = FileExplorerApplyContext { editor, ingress };
            apply_move(
                &mut cx,
                source,
                root,
                cursor,
                destination,
                modified_buffer_check,
            );
        }
        FileExplorerCommand::ApplyConfirmedDelete {
            target,
            root,
            cursor,
            modified_buffer_check,
        } => {
            let mut cx = FileExplorerApplyContext { editor, ingress };
            apply_confirmed_delete(&mut cx, target, root, cursor, modified_buffer_check);
        }
        FileExplorerCommand::ApplyCopy {
            source,
            root,
            cursor,
            destination,
            modified_buffer_check,
        } => {
            let mut cx = FileExplorerApplyContext { editor, ingress };
            apply_copy(
                &mut cx,
                source,
                root,
                cursor,
                destination,
                modified_buffer_check,
            );
        }
        FileExplorerCommand::PromptSaveBefore {
            operation,
            documents,
            continuation,
        } => {
            notify_file_explorer_confirmation(
                editor,
                format!(
                    "{} modified buffer(s) affected while {}. Type y to save, n to continue, c to cancel.",
                    documents.len(),
                    operation
                ),
            );
            let prompt = Prompt::new(
                format!(
                    "{} modified buffer(s) affected while {}. Save first? (y/n/c): ",
                    documents.len(),
                    operation
                )
                .into(),
                None,
                crate::ui::completers::none,
                move |cx, answer: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }

                    match answer {
                        "y" => match save_modified_documents(cx, &documents) {
                            Ok(()) => {
                                cx.submit_ui(crate::runtime::UiCommand::AfterWrites {
                                    documents: documents.clone(),
                                    command: Box::new(crate::runtime::UiCommand::FileExplorer(
                                        without_modified_buffer_check((*continuation).clone()),
                                    )),
                                });
                            }
                            Err(err) => notify_file_explorer_error(cx.editor, format!("{err}")),
                        },
                        "n" => spawn_file_explorer_command(
                            cx,
                            without_modified_buffer_check((*continuation).clone()),
                        ),
                        _ => {
                            notify_file_explorer_info(cx.editor, format!("Cancelled {}", operation))
                        }
                    }
                },
            );
            compositor.push(Box::new(prompt));
        }
    }
    log::info!(
        "[file_explorer] command_apply_done command={} elapsed_us={} focused_view={:?} focused_doc={:?} documents={}",
        command_name,
        command_start.elapsed().as_micros(),
        editor.focused_view_id(),
        editor.focused_document_id(),
        editor.document_count(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_core::Transaction;
    use helix_view::{
        doc_mut,
        editor::{Action, Config, Severity},
        graphics::Rect,
        handlers::Handlers,
        theme, Editor,
    };
    use std::sync::Arc;

    #[test]
    fn latest_search_slot_replaces_every_intermediate_query() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        let (ingress, _receiver) =
            crate::runtime::RuntimeIngress::channel(runtime.runtime().clone());
        let request = |generation, query: &str| {
            let root = PathBuf::from("workspace");
            let source = ExplorerSource::from_backend(
                root.clone(),
                &helix_view::editor::WorkspaceBackend::Local,
            );
            FileExplorerSearchRequest {
                source_id: source.identity(),
                source,
                root: ExplorerPath::Local(root),
                query: query.to_owned(),
                generation,
                config: helix_view::editor::FileExplorerConfig::default(),
            }
        };
        let mut latest = FileExplorerSearchState::default();

        latest.replace(FileExplorerSearchJob {
            request: request(1, "s"),
            ingress: ingress.clone(),
            abort: Arc::new(AtomicBool::new(false)),
            canceled: tokio_util::sync::CancellationToken::new(),
        });
        latest.replace(FileExplorerSearchJob {
            request: request(2, "sr"),
            ingress: ingress.clone(),
            abort: Arc::new(AtomicBool::new(false)),
            canceled: tokio_util::sync::CancellationToken::new(),
        });
        latest.replace(FileExplorerSearchJob {
            request: request(3, "src"),
            ingress,
            abort: Arc::new(AtomicBool::new(false)),
            canceled: tokio_util::sync::CancellationToken::new(),
        });

        let job = latest.take().expect("latest search");
        assert_eq!(job.request.generation, 3);
        assert_eq!(job.request.query, "src");
        assert!(!latest.is_pending());
    }

    #[test]
    fn replacing_search_cancels_active_generation() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        let (ingress, _receiver) =
            crate::runtime::RuntimeIngress::channel(runtime.runtime().clone());
        let request = |generation, query: &str| {
            let root = PathBuf::from("workspace");
            let source = ExplorerSource::from_backend(
                root.clone(),
                &helix_view::editor::WorkspaceBackend::Local,
            );
            FileExplorerSearchRequest {
                source_id: source.identity(),
                source,
                root: ExplorerPath::Local(root),
                query: query.to_owned(),
                generation,
                config: helix_view::editor::FileExplorerConfig::default(),
            }
        };
        let mut latest = FileExplorerSearchState::default();
        latest.replace(FileExplorerSearchJob {
            request: request(1, "s"),
            ingress: ingress.clone(),
            abort: Arc::new(AtomicBool::new(false)),
            canceled: tokio_util::sync::CancellationToken::new(),
        });
        let active = latest.take().expect("active search");

        latest.replace(FileExplorerSearchJob {
            request: request(2, "src"),
            ingress,
            abort: Arc::new(AtomicBool::new(false)),
            canceled: tokio_util::sync::CancellationToken::new(),
        });

        assert!(active.abort.load(Ordering::Acquire));
        assert!(latest.is_pending());
        assert!(latest.finish(&active.abort));
    }

    #[test]
    fn path_affects_documents_under_existing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("src");
        let child = root.join("main.rs");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&child, "").unwrap();

        assert!(path_affects_document(&root, &child));
    }

    #[test]
    fn vcs_snapshot_never_falls_back_to_the_client_filesystem_for_remote_roots() {
        let remote = ExplorerPath::Remote(helix_remote::WorkspacePath::root());

        assert_eq!(local_vcs_snapshot_root(remote), None);
        assert_eq!(
            local_vcs_snapshot_root(ExplorerPath::Local(PathBuf::from("workspace"))),
            Some(PathBuf::from("workspace"))
        );
    }

    #[test]
    fn path_affects_exact_file_only_for_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("main.rs");
        let sibling = temp.path().join("main.rs.bak");
        std::fs::write(&file, "").unwrap();
        std::fs::write(&sibling, "").unwrap();

        assert!(path_affects_document(&file, &file));
        assert!(!path_affects_document(&file, &sibling));
    }

    #[test]
    fn explorer_mutation_context_protects_root_and_rejects_outside_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let child = root.join("src/main.rs");
        let outside = temp.path().join("outside.rs");

        assert!(validate_explorer_descendant(&root, &child, "delete", false).is_ok());
        assert!(validate_explorer_descendant(&root, &root, "delete", false).is_err());
        assert!(validate_explorer_descendant(&root, &outside, "delete", false).is_err());
        assert!(validate_explorer_descendant(&root, &root, "copy to", true).is_ok());
        assert!(validate_explorer_destination(
            &root,
            &helix_view::editor::FileOperationDestination::UniqueInDirectory(root.clone()),
            "move to",
        )
        .is_ok());
        assert!(validate_explorer_destination(
            &root,
            &helix_view::editor::FileOperationDestination::Exact(outside),
            "move to",
        )
        .is_err());
        assert!(same_explorer_root(&root, &root.join(".")));
    }

    #[test]
    fn save_prompt_continuation_skips_second_prompt() {
        let command = FileExplorerCommand::ApplyConfirmedDelete {
            target: PathBuf::from("target"),
            root: PathBuf::from("."),
            cursor: 0,
            modified_buffer_check: ModifiedBufferCheck::Prompt,
        };

        let FileExplorerCommand::ApplyConfirmedDelete {
            modified_buffer_check,
            ..
        } = without_modified_buffer_check(command)
        else {
            panic!("expected delete command");
        };

        assert_eq!(modified_buffer_check, ModifiedBufferCheck::Skip);
    }

    #[tokio::test]
    async fn file_explorer_confirmation_uses_notification_toast() {
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime);

        notify_file_explorer_confirmation(&mut editor, "Overwrite src/main.rs? Type y to confirm.");

        let notification = editor
            .get_notification_history()
            .last()
            .expect("confirmation should add notification");
        assert_eq!(notification.severity, Severity::Warning);
        assert_eq!(
            notification.message.as_ref(),
            "File explorer: Overwrite src/main.rs? Type y to confirm."
        );
    }

    fn test_editor(runtime: helix_runtime::Runtime) -> Editor {
        let theme_loader = theme::Loader::new(&[]);
        let syn_loader = helix_core::config::default_lang_loader();
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        Editor::new(
            Rect::new(0, 0, 100, 30),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            config,
            runtime,
            Handlers::dummy(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_modified_documents_schedules_disk_write() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("main.rs");
        std::fs::write(&path, "old").unwrap();

        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime.clone());
        let doc_id = editor.open(&path, Action::VerticalSplit).unwrap();
        let view_id = editor.focused_view_id();
        let doc = doc_mut!(editor, &doc_id);
        let transaction = Transaction::change(
            doc.text(),
            [(0, doc.text().len_chars(), Some("new".into()))].into_iter(),
        );
        doc.apply(&transaction, view_id);
        assert!(doc.is_modified());

        let (ingress, _ingress_rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
        let idle_reset = crate::runtime::IdleResetGate::new().handle();
        let mut exit_tasks = crate::runtime::ExitTaskSet::default();
        let exit_task_work = editor.work();
        let redraw = editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events: plugin_events.into(),
        };
        let mut cx = crate::compositor::Context::new(
            &mut editor,
            &mut exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            idle_reset,
            crate::plugin_registry::PluginRuntime::default(),
        );

        save_modified_documents(&mut cx, &[doc_id]).unwrap();
        cx.editor.flush_writes().await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!cx.editor.document(doc_id).unwrap().is_modified());
    }
}
