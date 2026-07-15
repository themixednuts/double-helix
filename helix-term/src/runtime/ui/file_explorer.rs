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
        Confirmation, FileExplorerPanel, FileExplorerTreeWork, PreparedFileExplorerTree, Prompt,
        PromptEvent, FILE_EXPLORER_ID,
    },
};
use helix_view::{
    editor::{DocumentOpenRole, DocumentOpenWork, PreparedDocumentOpen, SavePolicy},
    DocumentId, Editor,
};

struct FileExplorerTreeJob {
    work: FileExplorerTreeWork,
    ingress: crate::runtime::RuntimeIngress,
}

#[derive(Default)]
struct FileExplorerTreeQueueState {
    pending: Option<FileExplorerTreeJob>,
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
                    let root = work.root().to_path_buf();
                    let result = block.spawn(move || work.execute()).await;
                    let outcome = {
                        let mut state = actor_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        state.pending = Some(FileExplorerTreeJob { work, ingress });
        state.prepared = None;
        drop(state);
        self.wake.request();
    }

    pub(crate) fn take(&self, root: &Path, generation: u64) -> Option<PreparedFileExplorerTree> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.root == root && prepared.generation == generation)
        {
            state.prepared.take()
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileExplorerPreviewRequest {
    pub(crate) root: PathBuf,
    pub(crate) path: PathBuf,
    pub(crate) cursor: u32,
    pub(crate) generation: u64,
}

pub(crate) struct PreparedFileExplorerPreview {
    pub(crate) request: FileExplorerPreviewRequest,
    pub(crate) result: Result<PreparedDocumentOpen, String>,
}

pub(crate) struct FileExplorerPreviewLoadRequest {
    request: FileExplorerPreviewRequest,
    work: DocumentOpenWork,
}

struct FileExplorerPreviewJob {
    load: FileExplorerPreviewLoadRequest,
    ingress: crate::runtime::RuntimeIngress,
}

#[derive(Default)]
struct FileExplorerPreviewQueueState {
    generation: Option<u64>,
    pending: Option<FileExplorerPreviewJob>,
    active: Option<(u64, helix_runtime::Token)>,
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
                    let token = helix_runtime::Token::new();
                    {
                        let mut queue = actor_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        queue.active = Some((generation, token.clone()));
                    }

                    let worker_token = token.clone();
                    let result = block
                        .spawn(move || prepare_file_explorer_preview(job.load, &worker_token))
                        .await
                        .unwrap_or_else(|error| Err(format!("preview worker failed: {error}")));

                    let should_notify = {
                        let mut queue = actor_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        queue.active = None;
                        let current = !token.is_canceled()
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

fn prepare_file_explorer_preview(
    load: FileExplorerPreviewLoadRequest,
    token: &helix_runtime::Token,
) -> Result<PreparedDocumentOpen, String> {
    let start = Instant::now();
    let prepared = load.work.execute().map_err(|error| error.to_string())?;
    if token.is_canceled() {
        return Err(String::from("preview request canceled"));
    }
    log::info!(
        "[file_explorer] preview prepared path={} generation={} elapsed_us={}",
        prepared.path().display(),
        load.request.generation,
        start.elapsed().as_micros(),
    );
    Ok(prepared)
}

pub(crate) fn queue_file_explorer_preview(
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
    request: FileExplorerPreviewRequest,
) {
    let work = editor.prepare_document_open(&request.path, DocumentOpenRole::Preview);
    ingress.file_explorer_preview(FileExplorerPreviewLoadRequest { request, work });
}

#[derive(Clone, Debug)]
pub(crate) struct FileExplorerSearchRequest {
    root: PathBuf,
    query: String,
    generation: u64,
    config: helix_view::editor::FileExplorerConfig,
}

struct FileExplorerSearchJob {
    request: FileExplorerSearchRequest,
    ingress: crate::runtime::RuntimeIngress,
    abort: Arc<AtomicBool>,
}

#[derive(Default)]
struct FileExplorerSearchState {
    pending: Option<FileExplorerSearchJob>,
    active_abort: Option<Arc<AtomicBool>>,
}

impl FileExplorerSearchState {
    fn replace(&mut self, job: FileExplorerSearchJob) {
        if let Some(active) = &self.active_abort {
            active.store(true, Ordering::Release);
        }
        if let Some(pending) = &self.pending {
            pending.abort.store(true, Ordering::Release);
        }
        self.pending = Some(job);
    }

    fn take(&mut self) -> Option<FileExplorerSearchJob> {
        let job = self.pending.take()?;
        self.active_abort = Some(job.abort.clone());
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
                    let worker_abort = abort.clone();
                    let result = block
                        .spawn(move || execute_file_explorer_search(request, &worker_abort))
                        .await;
                    let superseded = actor_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .finish(&abort);
                    if superseded {
                        continue;
                    }
                    match result {
                        Ok(matches) => {
                            let _ = job
                                .ingress
                                .send_ui(UiCommand::FileExplorer(
                                    FileExplorerCommand::ApplySearchResults {
                                        root: job.request.root,
                                        query: job.request.query,
                                        generation: job.request.generation,
                                        matches,
                                    },
                                ))
                                .await;
                        }
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
            });
        self.wake.request();
    }
}

fn execute_file_explorer_search(
    request: FileExplorerSearchRequest,
    abort: &AtomicBool,
) -> Vec<PathBuf> {
    let start = Instant::now();
    match crate::fff::search_file_explorer_available_cancellable(
        &request.root,
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
            matches
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

struct FileExplorerApplyContext<'a> {
    editor: &'a mut Editor,
    ingress: crate::runtime::RuntimeIngress,
}

fn file_explorer_command_name(cmd: &FileExplorerCommand) -> &'static str {
    match cmd {
        FileExplorerCommand::ToggleSourceOption { .. } => "ToggleSourceOption",
        FileExplorerCommand::FileOperationCompleted { .. } => "FileOperationCompleted",
        FileExplorerCommand::ApplyTree { .. } => "ApplyTree",
        FileExplorerCommand::PreviewSelection { .. } => "PreviewSelection",
        FileExplorerCommand::ApplyPreview { .. } => "ApplyPreview",
        FileExplorerCommand::ApplyVcsSnapshot { .. } => "ApplyVcsSnapshot",
        FileExplorerCommand::StartSearch { .. } => "StartSearch",
        FileExplorerCommand::ApplySearchResults { .. } => "ApplySearchResults",
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

pub(crate) fn queue_file_explorer_vcs_snapshot(
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
    root: PathBuf,
) {
    if !editor.config().file_explorer.vcs {
        return;
    }

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
    root: PathBuf,
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
    root: Option<PathBuf>,
    cursor: Option<usize>,
    select_path: Option<PathBuf>,
    follow_current_file: bool,
    clear_cache: bool,
) {
    let work = panel.prepare_tree_refresh(
        editor,
        root,
        cursor,
        select_path,
        follow_current_file,
        clear_cache,
    );
    ingress.file_explorer_tree(work);
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
            Some(root),
            Some(cursor),
            None,
            false,
            true,
        );
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=true root={} cursor={} elapsed_us={}",
            requested_root.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, requested_root);
    } else {
        let mut panel = FileExplorerPanel::new_deferred(root, editor);
        queue_file_explorer_tree_refresh(
            &mut panel,
            editor,
            ingress.clone(),
            None,
            Some(cursor),
            None,
            false,
            false,
        );
        compositor.push(Box::new(panel));
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=false root={} cursor={} elapsed_us={}",
            requested_root.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, requested_root);
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
            Some(root),
            Some(cursor),
            Some(path),
            false,
            true,
        );
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=true root={} select_path={} fallback_cursor={} elapsed_us={}",
            requested_root.display(),
            requested_path.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, requested_root);
    } else {
        let mut panel = FileExplorerPanel::new_deferred(root, editor);
        queue_file_explorer_tree_refresh(
            &mut panel,
            editor,
            ingress.clone(),
            None,
            Some(cursor),
            Some(path),
            false,
            false,
        );
        compositor.push(Box::new(panel));
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=false root={} select_path={} fallback_cursor={} elapsed_us={}",
            requested_root.display(),
            requested_path.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, requested_root);
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
        cx.editor.save(doc_id, None::<PathBuf>, SavePolicy::Safe)?;
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
                    None,
                    Some(cursor),
                    None,
                    false,
                    true,
                );
                panel.queue_current_search(editor, ingress);
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
                .is_some_and(|panel| same_explorer_root(panel.root_for_context(), &root));
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
                        root,
                        path,
                        cursor,
                        generation,
                    },
                );
            }
        }
        FileExplorerCommand::ApplyPreview {
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
                        root,
                        path,
                        cursor,
                        generation,
                    },
                );
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
                        None,
                        Some(cursor),
                        None,
                        false,
                        false,
                    );
                }
            }
        }
        FileExplorerCommand::StartSearch {
            root,
            query,
            generation,
            config,
        } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                let accepted = panel.accepts_search_request(&root, &query, generation);
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
                    queue_file_explorer_search(ingress, root, query, generation, config);
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
            root,
            query,
            generation,
            matches,
        } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                let applied = panel.apply_search_results(editor, root, query, generation, matches);
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
        let request = |generation, query: &str| FileExplorerSearchRequest {
            root: PathBuf::from("workspace"),
            query: query.to_owned(),
            generation,
            config: helix_view::editor::FileExplorerConfig::default(),
        };
        let mut latest = FileExplorerSearchState::default();

        latest.replace(FileExplorerSearchJob {
            request: request(1, "s"),
            ingress: ingress.clone(),
            abort: Arc::new(AtomicBool::new(false)),
        });
        latest.replace(FileExplorerSearchJob {
            request: request(2, "sr"),
            ingress: ingress.clone(),
            abort: Arc::new(AtomicBool::new(false)),
        });
        latest.replace(FileExplorerSearchJob {
            request: request(3, "src"),
            ingress,
            abort: Arc::new(AtomicBool::new(false)),
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
        let request = |generation, query: &str| FileExplorerSearchRequest {
            root: PathBuf::from("workspace"),
            query: query.to_owned(),
            generation,
            config: helix_view::editor::FileExplorerConfig::default(),
        };
        let mut latest = FileExplorerSearchState::default();
        latest.replace(FileExplorerSearchJob {
            request: request(1, "s"),
            ingress: ingress.clone(),
            abort: Arc::new(AtomicBool::new(false)),
        });
        let active = latest.take().expect("active search");

        latest.replace(FileExplorerSearchJob {
            request: request(2, "src"),
            ingress,
            abort: Arc::new(AtomicBool::new(false)),
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
