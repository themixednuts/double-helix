use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use helix_core::{Selection, Syntax, Tendril, Transaction};
use helix_view::{
    document::DocumentOpenError,
    editor::{
        DocumentOpenWork, DocumentReloadApply, DocumentReloadError, DocumentReloadWork,
        PreparedDocumentOpen,
    },
    Align, DocumentId, Editor,
};

use super::command::{
    DocumentCommand, DocumentOpenAlignment, DocumentOpenCompletion, DocumentOpenCompletionTarget,
    DocumentOpenLane, DocumentOpenRequest, DocumentOpenSelection, DocumentOpenTarget,
    DocumentReloadOrigin,
};
use super::snapshot::UiSnapshotRequest;

const MAX_CONCURRENT_DOCUMENT_RELOADS: usize = 4;

struct QueuedDocumentReload {
    generation: u64,
    origin: DocumentReloadOrigin,
    work: DocumentReloadWork,
}

#[derive(Default)]
struct DocumentReloadQueueState {
    next_generation: u64,
    pending: VecDeque<QueuedDocumentReload>,
    active: HashSet<DocumentId>,
    latest: HashMap<DocumentId, u64>,
}

impl DocumentReloadQueueState {
    fn enqueue(
        &mut self,
        work: DocumentReloadWork,
        origin: DocumentReloadOrigin,
    ) -> Vec<QueuedDocumentReload> {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        let document = work.document();
        self.latest.insert(document, generation);

        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.work.document() == document)
        {
            *pending = QueuedDocumentReload {
                generation,
                origin,
                work,
            };
        } else {
            self.pending.push_back(QueuedDocumentReload {
                generation,
                origin,
                work,
            });
        }
        self.take_ready()
    }

    fn take_ready(&mut self) -> Vec<QueuedDocumentReload> {
        let mut ready = Vec::new();
        while self.active.len() < MAX_CONCURRENT_DOCUMENT_RELOADS {
            let Some(index) = self
                .pending
                .iter()
                .position(|pending| !self.active.contains(&pending.work.document()))
            else {
                break;
            };
            let queued = self
                .pending
                .remove(index)
                .expect("pending reload index disappeared");
            self.active.insert(queued.work.document());
            ready.push(queued);
        }
        ready
    }
}

#[derive(Clone)]
pub(crate) struct DocumentReloadQueue {
    work: helix_runtime::Work,
    block: helix_runtime::Block,
    state: Arc<Mutex<DocumentReloadQueueState>>,
}

impl std::fmt::Debug for DocumentReloadQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("DocumentReloadQueue")
            .field("pending", &state.pending.len())
            .field("active", &state.active.len())
            .finish_non_exhaustive()
    }
}

impl DocumentReloadQueue {
    pub(crate) fn new(work: helix_runtime::Work, block: helix_runtime::Block) -> Self {
        Self {
            work,
            block,
            state: Arc::new(Mutex::new(DocumentReloadQueueState::default())),
        }
    }

    pub(crate) fn submit(
        &self,
        work: DocumentReloadWork,
        origin: DocumentReloadOrigin,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        let ready = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.enqueue(work, origin)
        };
        self.spawn_ready(ready, ingress);
    }

    pub(crate) fn take_current(&self, document: DocumentId, generation: u64) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.latest.get(&document).copied() != Some(generation) {
            return false;
        }
        state.latest.remove(&document);
        true
    }

    fn spawn_ready(
        &self,
        ready: Vec<QueuedDocumentReload>,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        for queued in ready {
            let queue = self.clone();
            let worker_ingress = ingress.clone();
            let block = self.block.clone();
            self.work
                .spawn(async move {
                    let document = queued.work.document();
                    let path = queued.work.path().to_path_buf();
                    let start = Instant::now();
                    log::info!(
                        "[document_reload] phase=load_start doc={document:?} path={} generation={} origin={:?}",
                        path.display(),
                        queued.generation,
                        queued.origin,
                    );
                    let result = match block.spawn(move || queued.work.execute()).await {
                        Ok(result) => result,
                        Err(error) => Err(DocumentReloadError::worker(path.clone(), error.to_string())),
                    };
                    log::info!(
                        "[document_reload] phase=load_done doc={document:?} path={} generation={} origin={:?} success={} elapsed_us={}",
                        path.display(),
                        queued.generation,
                        queued.origin,
                        result.is_ok(),
                        start.elapsed().as_micros(),
                    );
                    let _ = worker_ingress
                        .send_ui(crate::runtime::UiCommand::Document(
                            DocumentCommand::ReloadFinished {
                                document,
                                generation: queued.generation,
                                origin: queued.origin,
                                result,
                            },
                        ))
                        .await;
                    queue.finish(document, worker_ingress);
                })
                .detach();
        }
    }

    fn finish(&self, document: DocumentId, ingress: crate::runtime::RuntimeIngress) {
        let ready = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active.remove(&document);
            state.take_ready()
        };
        self.spawn_ready(ready, ingress);
    }
}

struct DocumentOpenJob {
    request: DocumentOpenRequest,
    work: DocumentOpenWork,
}

struct QueuedDocumentOpen {
    generation: u64,
    lane: DocumentOpenLane,
    jobs: Vec<DocumentOpenJob>,
    stop_on_error: bool,
}

#[derive(Default)]
struct DocumentOpenQueueState {
    next_generation: u64,
    pending: VecDeque<QueuedDocumentOpen>,
    active: HashSet<DocumentOpenLane>,
    latest: HashMap<DocumentOpenLane, u64>,
}

impl DocumentOpenQueueState {
    fn enqueue(
        &mut self,
        lane: DocumentOpenLane,
        jobs: Vec<DocumentOpenJob>,
        stop_on_error: bool,
    ) -> Vec<QueuedDocumentOpen> {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.latest.insert(lane, generation);
        if let Some(pending) = self.pending.iter_mut().find(|pending| pending.lane == lane) {
            *pending = QueuedDocumentOpen {
                generation,
                lane,
                jobs,
                stop_on_error,
            };
        } else {
            self.pending.push_back(QueuedDocumentOpen {
                generation,
                lane,
                jobs,
                stop_on_error,
            });
        }
        self.take_ready()
    }

    fn take_ready(&mut self) -> Vec<QueuedDocumentOpen> {
        let mut ready = Vec::new();
        while self.active.len() < 4 {
            let Some(index) = self
                .pending
                .iter()
                .position(|pending| !self.active.contains(&pending.lane))
            else {
                break;
            };
            let queued = self
                .pending
                .remove(index)
                .expect("pending document open index disappeared");
            self.active.insert(queued.lane);
            ready.push(queued);
        }
        ready
    }

    fn cancel(&mut self, lane: DocumentOpenLane) {
        self.pending.retain(|pending| pending.lane != lane);
        self.latest.remove(&lane);
    }
}

#[derive(Clone)]
pub(crate) struct DocumentOpenQueue {
    work: helix_runtime::Work,
    block: helix_runtime::Block,
    state: Arc<Mutex<DocumentOpenQueueState>>,
}

impl std::fmt::Debug for DocumentOpenQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("DocumentOpenQueue")
            .field("pending", &state.pending.len())
            .field("active", &state.active.len())
            .finish_non_exhaustive()
    }
}

impl DocumentOpenQueue {
    pub(crate) fn new(work: helix_runtime::Work, block: helix_runtime::Block) -> Self {
        Self {
            work,
            block,
            state: Arc::new(Mutex::new(DocumentOpenQueueState::default())),
        }
    }

    pub(crate) fn submit(
        &self,
        work: DocumentOpenWork,
        request: DocumentOpenRequest,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        let lane = request.lane;
        self.submit_batch(vec![(work, request)], lane, false, ingress);
    }

    pub(crate) fn submit_batch(
        &self,
        jobs: Vec<(DocumentOpenWork, DocumentOpenRequest)>,
        lane: DocumentOpenLane,
        stop_on_error: bool,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        if jobs.is_empty() {
            return;
        }
        let jobs = jobs
            .into_iter()
            .map(|(work, request)| DocumentOpenJob { request, work })
            .collect();
        let ready = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enqueue(lane, jobs, stop_on_error);
        self.spawn_ready(ready, ingress);
    }

    pub(crate) fn take_current(&self, lane: DocumentOpenLane, generation: u64) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.latest.get(&lane).copied() != Some(generation) {
            return false;
        }
        state.latest.remove(&lane);
        true
    }

    pub(crate) fn cancel(&self, lane: DocumentOpenLane) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel(lane);
    }

    fn spawn_ready(&self, ready: Vec<QueuedDocumentOpen>, ingress: crate::runtime::RuntimeIngress) {
        for queued in ready {
            let queue = self.clone();
            let worker_ingress = ingress.clone();
            let block = self.block.clone();
            self.work
                .spawn(async move {
                    let generation = queued.generation;
                    let lane = queued.lane;
                    let stop_on_error = queued.stop_on_error;
                    let batch_start = Instant::now();
                    let mut completions = Vec::with_capacity(queued.jobs.len());
                    for job in queued.jobs {
                        let path = job.work.path().to_path_buf();
                        let start = Instant::now();
                        log::info!(
                            "[document_open] phase=load_start path={} generation={} lane={lane:?}",
                            path.display(),
                            generation,
                        );
                        let inspect_binary = job.request.external_if_binary.is_some();
                        let worker_path = path.clone();
                        let result = match block.spawn(move || {
                            if inspect_binary
                                && file_is_binary(&worker_path).unwrap_or(false)
                            {
                                return Err(DocumentOpenError::BinaryFile);
                            }
                            job.work.execute()
                        }).await {
                                Ok(result) => result,
                                Err(error) => Err(DocumentOpenError::Worker(error.to_string())),
                            };
                        let failed = result.as_ref().is_err_and(|error| {
                            !matches!(
                                error,
                                DocumentOpenError::Directory | DocumentOpenError::BinaryFile
                            )
                        });
                        log::info!(
                            "[document_open] phase=load_done path={} generation={} lane={lane:?} success={} elapsed_us={}",
                            path.display(),
                            generation,
                            !failed,
                            start.elapsed().as_micros(),
                        );
                        completions.push(DocumentOpenCompletion {
                            request: job.request,
                            result,
                        });
                        if failed && stop_on_error {
                            break;
                        }
                    }
                    let _ = worker_ingress
                        .send_ui(crate::runtime::UiCommand::Document(
                            DocumentCommand::OpenFinished {
                                generation,
                                lane,
                                completions,
                                stop_on_error,
                            },
                        ))
                        .await;
                    log::info!(
                        "[document_open] phase=batch_done generation={} lane={lane:?} elapsed_us={}",
                        generation,
                        batch_start.elapsed().as_micros(),
                    );
                    queue.finish(lane, worker_ingress);
                })
                .detach();
        }
    }

    fn finish(&self, lane: DocumentOpenLane, ingress: crate::runtime::RuntimeIngress) {
        let ready = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active.remove(&lane);
            state.take_ready()
        };
        self.spawn_ready(ready, ingress);
    }
}

pub(crate) fn queue_document_open(
    editor: &mut Editor,
    ingress: &crate::runtime::RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
    mut request: DocumentOpenRequest,
) {
    let work = editor.prepare_document_open(
        &request.path,
        helix_view::editor::DocumentOpenRole::Interactive,
    );
    request.path = work.path().to_path_buf();
    if let Some(document) = editor.document_id_by_path(&request.path) {
        let Some(target) = resolve_document_open_target(&request, None) else {
            complete_document_open(
                editor,
                foreground,
                &request,
                Err(helix_plugin_api::ContractError::internal(
                    "document-open target is unavailable",
                )),
            );
            return;
        };
        let result = apply_document_open_at_target(
            editor,
            foreground,
            &request,
            target,
            None,
            Some(document),
        )
        .map(|(document, _)| document)
        .ok_or_else(|| {
            helix_plugin_api::ContractError::internal(
                "document-open target disappeared before apply",
            )
        });
        complete_document_open(editor, foreground, &request, result);
        return;
    }
    if let Some(prepared) = editor.take_prepared_document_open(&request.path) {
        let Some(target) = resolve_document_open_target(&request, None) else {
            complete_document_open(
                editor,
                foreground,
                &request,
                Err(helix_plugin_api::ContractError::internal(
                    "document-open target is unavailable",
                )),
            );
            return;
        };
        let result = apply_document_open_at_target(
            editor,
            foreground,
            &request,
            target,
            Some(prepared),
            None,
        )
        .map(|(document, _)| document)
        .ok_or_else(|| {
            helix_plugin_api::ContractError::internal(
                "document-open target disappeared before apply",
            )
        });
        complete_document_open(editor, foreground, &request, result);
        return;
    }
    ingress.document_open(work, request);
}

fn complete_document_open(
    _editor: &mut Editor,
    _foreground: &crate::runtime::ForegroundEvents,
    request: &DocumentOpenRequest,
    result: helix_plugin_api::ContractResult<DocumentId>,
) {
    match &request.completion {
        DocumentOpenCompletionTarget::Editor => {}
        DocumentOpenCompletionTarget::Plugin(respond_to) => {
            respond_to.send(result.map(|document| {
                helix_plugin_api::PluginTaskResult::Document(
                    helix_plugin_editor::adapt::document_handle(document),
                )
            }));
        }
    }
}

pub(crate) fn queue_document_open_batch(
    editor: &Editor,
    ingress: &crate::runtime::RuntimeIngress,
    requests: Vec<DocumentOpenRequest>,
) {
    let jobs = requests
        .into_iter()
        .map(|mut request| {
            let work = editor.prepare_document_open(
                &request.path,
                helix_view::editor::DocumentOpenRole::Interactive,
            );
            request.path = work.path().to_path_buf();
            (work, request)
        })
        .collect::<Vec<_>>();
    ingress.document_open_batch(jobs, DocumentOpenLane::Command, true);
}

fn finish_document_open(
    editor: &mut Editor,
    foreground: &crate::runtime::ForegroundEvents,
    document: DocumentId,
    view_id: helix_view::ViewId,
    request: &DocumentOpenRequest,
    was_existing: bool,
) {
    let mut selection_error = None;
    {
        let view = helix_view::view_mut!(editor, view_id);
        let doc = helix_view::doc_mut!(editor, &document);
        let selection = match &request.selection {
            DocumentOpenSelection::None => None,
            DocumentOpenSelection::Position(position) => Some(Selection::point(
                helix_core::pos_at_coords(doc.text().slice(..), *position, true),
            )),
            DocumentOpenSelection::Line(line) => {
                if *line >= doc.text().len_lines() {
                    selection_error = Some(
                        "The line you jumped to does not exist anymore because the file has changed.",
                    );
                    None
                } else {
                    let start = doc.text().line_to_char(*line);
                    let end = doc
                        .text()
                        .line_to_char((*line + 1).min(doc.text().len_lines()));
                    Some(Selection::single(start, end))
                }
            }
            DocumentOpenSelection::CharRange { start, end } => {
                if *start > doc.text().len_chars() || *end > doc.text().len_chars() {
                    selection_error = Some(
                        "The location you jumped to does not exist anymore because the file has changed.",
                    );
                    None
                } else {
                    Some(Selection::single(*start, *end))
                }
            }
            DocumentOpenSelection::LspRange {
                range,
                offset_encoding,
            } => match helix_lsp::util::lsp_range_to_range(doc.text(), *range, *offset_encoding) {
                Some(range) => Some(Selection::single(range.head, range.anchor)),
                None => {
                    selection_error = Some(
                        "The location requested by the language server does not exist anymore because the file has changed.",
                    );
                    None
                }
            },
            DocumentOpenSelection::OneBasedRange {
                line,
                column,
                end_line,
                end_column,
            } => {
                let text_end = doc.text().len_chars().saturating_sub(1);
                let start = one_based_position(doc.text(), *line, *column).unwrap_or(0);
                let end = end_line
                    .and_then(|line| {
                        one_based_position(doc.text(), line, end_column.unwrap_or_default())
                    })
                    .unwrap_or(start);
                Some(Selection::single(start.min(text_end), end.min(text_end)))
            }
        };
        if let Some(selection) = selection {
            doc.set_selection(view_id, selection);
        }
        let should_align = selection_error.is_none()
            && match request.alignment {
                DocumentOpenAlignment::None => false,
                DocumentOpenAlignment::Center => true,
                DocumentOpenAlignment::CenterIfAction => request.action.align_view(view, doc.id()),
            };
        if should_align {
            helix_view::align_view(doc, view, Align::Center);
        }
    }

    if let Some(error) = selection_error {
        editor.notify_error(error);
    }

    if request.default_folding_if_new && !was_existing {
        crate::ui::default_folding(editor);
    }
    match request.post_action {
        crate::runtime::DocumentOpenPostAction::None => {}
        crate::runtime::DocumentOpenPostAction::DetachPath => {
            if let Some(doc) = editor.document_mut(document) {
                doc.set_path(None);
            }
        }
        crate::runtime::DocumentOpenPostAction::RequestInlineValues => {
            if let Err(error) = foreground
                .task(crate::runtime::RuntimeTaskEvent::RequestInlineValues { doc_id: document })
            {
                editor.set_error(error.to_string());
            }
        }
        crate::runtime::DocumentOpenPostAction::CollaborationReveal => {
            editor.complete_location_reveal(false);
        }
        crate::runtime::DocumentOpenPostAction::AssistantFollow => {
            editor.complete_location_reveal(true);
        }
    }
    if let Some(record) = request.fff_record.clone() {
        let path = request.path.clone();
        let tracking = editor.runtime().block().spawn(move || {
            crate::fff::record_file_open(&record.root, &record.config, &record.query, &path);
        });
        editor
            .work()
            .spawn(async move {
                match tracking.await {
                    Ok(()) => {}
                    Err(error) => log::debug!("FFF file-open tracking worker failed: {error}"),
                }
            })
            .detach();
    }
}

fn resolve_document_open_target(
    request: &DocumentOpenRequest,
    previous: Option<helix_view::ViewId>,
) -> Option<helix_view::ViewId> {
    match request.target {
        DocumentOpenTarget::View(view) => Some(view),
        DocumentOpenTarget::PreviousResult => previous,
    }
}

fn apply_document_open_at_target(
    editor: &mut Editor,
    foreground: &crate::runtime::ForegroundEvents,
    request: &DocumentOpenRequest,
    target: helix_view::ViewId,
    prepared: Option<PreparedDocumentOpen>,
    existing: Option<DocumentId>,
) -> Option<(DocumentId, helix_view::ViewId)> {
    if !editor.tree.contains(target) {
        log::info!(
            "[document_open] phase=apply_skip reason=missing_target_view path={} target={target:?}",
            request.path.display(),
        );
        return None;
    }

    let current = editor.focused_view_id();
    let apply = move |editor: &mut Editor| {
        let was_existing = editor.document_id_by_path(&request.path).is_some();
        let document = if let Some(prepared) = prepared {
            let role = prepared.role();
            let document = editor.apply_prepared_document_open(prepared, request.action);
            if role.is_preview() {
                editor.promote_preview_document(document);
            }
            document
        } else {
            let document = existing.or_else(|| editor.document_id_by_path(&request.path))?;
            editor.promote_preview_document(document);
            editor.switch(document, request.action);
            document
        };
        let opened_view = editor.focused_view_id();
        finish_document_open(
            editor,
            foreground,
            document,
            opened_view,
            request,
            was_existing,
        );
        Some((document, opened_view))
    };

    if current == target {
        apply(editor)
    } else {
        editor.with_temporary_focus(target, apply)
    }
}

fn one_based_position(text: &helix_core::Rope, line: usize, column: usize) -> Option<usize> {
    let line = text.try_line_to_char(line.saturating_sub(1)).ok()?;
    Some(line + column.saturating_sub(1))
}

fn file_is_binary(path: &std::path::Path) -> std::io::Result<bool> {
    let mut read_buffer = Vec::with_capacity(1024);
    let file = std::fs::File::open(path)?;
    let read = file.take(1024).read_to_end(&mut read_buffer)?;
    Ok(content_inspector::inspect(&read_buffer[..read]).is_binary())
}

pub(crate) fn queue_document_reload(
    editor: &Editor,
    ingress: &crate::runtime::RuntimeIngress,
    document: DocumentId,
    origin: DocumentReloadOrigin,
) -> bool {
    let Some(work) = editor.prepare_document_reload(document) else {
        return false;
    };
    ingress.document_reload(work, origin);
    true
}

pub(crate) fn queue_document_reloads(
    editor: &Editor,
    ingress: &crate::runtime::RuntimeIngress,
    documents: impl IntoIterator<Item = DocumentId>,
    origin: DocumentReloadOrigin,
) -> usize {
    documents
        .into_iter()
        .filter(|document| queue_document_reload(editor, ingress, *document, origin))
        .count()
}

#[derive(Clone, Debug)]
struct RuntimeSyntaxKey {
    document: DocumentId,
    path: PathBuf,
    version: i32,
    generation: u64,
}

pub(crate) fn queue_runtime_syntax(
    editor: &Editor,
    document: DocumentId,
    generation: u64,
    ingress: crate::runtime::RuntimeIngress,
) {
    let current_generation = helix_loader::runtime_assets()
        .map(helix_loader::RuntimeAssets::generation)
        .unwrap_or_default();
    if current_generation != generation {
        return;
    }
    let Some(doc) = editor.document(document) else {
        return;
    };
    if doc.has_syntax() {
        return;
    }
    let Some(path) = doc.path().cloned() else {
        return;
    };
    let Some(language) = doc.language_configuration().cloned() else {
        return;
    };
    let text = doc.text().clone();
    let loader = editor.syn_loader.load_full();
    let key = RuntimeSyntaxKey {
        document,
        path,
        version: doc.version(),
        generation,
    };
    UiSnapshotRequest::new("[runtime_syntax]", key)
        .load_with(move |_| {
            Syntax::new_with_timeout(
                text.slice(..),
                language.language(),
                &loader,
                helix_core::syntax::BACKGROUND_PARSE_TIMEOUT,
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
        .apply_with(|key, syntax| {
            crate::runtime::UiCommand::Document(DocumentCommand::ApplyRuntimeSyntax {
                document: key.document,
                path: key.path,
                version: key.version,
                generation: key.generation,
                syntax,
            })
        })
        .spawn(editor.work(), editor.runtime().block().clone(), ingress);
}

pub(crate) fn apply_document_command(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: crate::runtime::ForegroundEvents,
    cmd: DocumentCommand,
) {
    match cmd {
        DocumentCommand::OpenRequested { request } => {
            let selection = request.range.map_or(DocumentOpenSelection::None, |range| {
                DocumentOpenSelection::CharRange {
                    start: range.anchor,
                    end: range.head,
                }
            });
            let post_action = match request.purpose {
                helix_view::handlers::NavigationPurpose::CollaborationReveal => {
                    crate::runtime::DocumentOpenPostAction::CollaborationReveal
                }
                helix_view::handlers::NavigationPurpose::AssistantFollow => {
                    crate::runtime::DocumentOpenPostAction::AssistantFollow
                }
            };
            queue_document_open(
                editor,
                &ingress,
                &foreground,
                DocumentOpenRequest {
                    path: request.path,
                    action: request.action,
                    lane: DocumentOpenLane::Navigation,
                    target: DocumentOpenTarget::View(request.target),
                    selection,
                    alignment: DocumentOpenAlignment::Center,
                    default_folding_if_new: false,
                    fff_record: None,
                    external_if_binary: None,
                    post_action,
                    completion: DocumentOpenCompletionTarget::Editor,
                },
            );
        }
        DocumentCommand::CloseView {
            view,
            check_buffers,
        } => {
            if check_buffers && editor.has_single_view() {
                if let Err(error) = crate::commands::typed::buffers_remaining_impl(editor) {
                    editor.set_error(error.to_string());
                    return;
                }
            }
            editor.close(view);
        }
        DocumentCommand::CloseDocuments { documents, policy } => {
            if let Err(error) =
                crate::commands::typed::close_documents_now(editor, &documents, policy)
            {
                editor.set_error(error.to_string());
            }
        }
        DocumentCommand::CloseAllViews { check_buffers } => {
            if check_buffers {
                if let Err(error) = crate::commands::typed::buffers_remaining_impl(editor) {
                    editor.set_error(error.to_string());
                    return;
                }
            }
            let views: Vec<_> = editor.tree.views().map(|(view, _)| view.id).collect();
            for view in views {
                editor.close(view);
            }
        }
        DocumentCommand::ApplyRuntimeSyntax {
            document,
            path,
            version,
            generation,
            syntax,
        } => {
            let current_generation = helix_loader::runtime_assets()
                .map(helix_loader::RuntimeAssets::generation)
                .unwrap_or_default();
            if current_generation != generation {
                return;
            }
            let requested_path = helix_stdx::path::canonicalize(&path);
            let retry = {
                let Some(doc) = editor.document_mut(document) else {
                    return;
                };
                let current_path = doc.path().map(helix_stdx::path::canonicalize);
                if current_path.as_deref() != Some(requested_path.as_path())
                    || doc.version() != version
                {
                    true
                } else if doc.has_syntax() {
                    false
                } else {
                    doc.set_syntax(Some(syntax));
                    editor.mark_redraw_pending();
                    editor.request_redraw();
                    false
                }
            };
            if retry {
                queue_runtime_syntax(editor, document, generation, ingress);
            }
        }
        DocumentCommand::InsertFileFinished {
            document,
            view,
            version,
            selection,
            scrolloff,
            path,
            result,
        } => {
            let contents = match result {
                Ok(contents) => contents,
                Err(error) => {
                    editor.notify_error(format!("Failed to read '{}': {error}", path.display()));
                    return;
                }
            };
            let target_is_current = editor
                .tree
                .try_get(view)
                .is_some_and(|target| target.doc == document);
            let document_is_current = editor
                .document(document)
                .is_some_and(|doc| doc.version() == version);
            if !target_is_current || !document_is_current {
                editor.notify_warning(format!(
                    "Did not insert '{}': the target document changed while it was being read",
                    path.display()
                ));
                return;
            }

            let doc = helix_view::doc_mut!(editor, &document);
            let view_state = helix_view::view_mut!(editor, view);
            let transaction = Transaction::insert(doc.text(), &selection, Tendril::from(contents));
            doc.apply(&transaction, view);
            doc.append_changes_to_history(view_state);
            view_state.ensure_cursor_in_view(doc, scrolloff);
        }
        DocumentCommand::ReloadFinished {
            document,
            generation,
            origin,
            result,
        } => {
            if !ingress.take_document_reload(document, generation) {
                log::info!(
                    "[document_reload] phase=apply_skip reason=superseded doc={document:?} generation={generation} origin={origin:?}"
                );
                return;
            }

            let prepared = match result {
                Ok(prepared) => prepared,
                Err(error) => {
                    editor.notify_error(error.to_string());
                    return;
                }
            };
            match editor.apply_prepared_document_reload(prepared) {
                DocumentReloadApply::Applied => {
                    log::info!(
                        "[document_reload] phase=apply_done doc={document:?} generation={generation} origin={origin:?}"
                    );
                }
                DocumentReloadApply::Stale(reason) => {
                    log::info!(
                        "[document_reload] phase=apply_skip reason={reason:?} doc={document:?} generation={generation} origin={origin:?}"
                    );
                }
            }
        }
        DocumentCommand::OpenFinished {
            generation,
            lane,
            completions,
            stop_on_error,
        } => {
            if !ingress.take_document_open(lane, generation) {
                log::info!(
                    "[document_open] phase=apply_skip reason=superseded generation={generation} lane={lane:?}",
                );
                return;
            }
            let mut previous_view = None;
            for completion in completions {
                let request = completion.request;
                let Some(target) = resolve_document_open_target(&request, previous_view) else {
                    let message = format!(
                        "Skipped opening '{}': the target pane is no longer available",
                        request.path.display()
                    );
                    complete_document_open(
                        editor,
                        &foreground,
                        &request,
                        Err(helix_plugin_api::ContractError::internal(message.clone())),
                    );
                    if request.completion.is_editor() {
                        editor.notify_warning(message);
                    }
                    if stop_on_error {
                        break;
                    }
                    continue;
                };
                let prepared = match completion.result {
                    Ok(prepared) => prepared,
                    Err(DocumentOpenError::BinaryFile) => {
                        if !request.completion.is_editor() {
                            complete_document_open(
                                editor,
                                &foreground,
                                &request,
                                Err(helix_plugin_api::ContractError::invalid_request(format!(
                                    "'{}' is a binary file",
                                    request.path.display()
                                ))),
                            );
                            continue;
                        }
                        if let Some(url) = request.external_if_binary.clone() {
                            crate::runtime::ingress::spawn_task_event_with_future(
                                editor.work(),
                                crate::open_external_url_task_event(url),
                                ingress.clone(),
                            );
                        }
                        previous_view = Some(target);
                        continue;
                    }
                    Err(DocumentOpenError::Directory) => {
                        if !request.completion.is_editor() {
                            complete_document_open(
                                editor,
                                &foreground,
                                &request,
                                Err(helix_plugin_api::ContractError::invalid_request(format!(
                                    "'{}' is a directory",
                                    request.path.display()
                                ))),
                            );
                            continue;
                        }
                        if let Err(error) = foreground.ui(crate::runtime::UiCommand::Layer(
                            crate::runtime::LayerCommand::PushFilePicker {
                                root: request.path.clone(),
                            },
                        )) {
                            editor.set_error(error.to_string());
                        }
                        previous_view = Some(target);
                        continue;
                    }
                    Err(error) => {
                        let message =
                            format!("Unable to open '{}': {error}", request.path.display());
                        complete_document_open(
                            editor,
                            &foreground,
                            &request,
                            Err(helix_plugin_api::ContractError::internal(message.clone())),
                        );
                        if request.completion.is_editor() {
                            editor.notify_error(message);
                        }
                        if stop_on_error {
                            break;
                        }
                        continue;
                    }
                };
                let Some((document, opened_view)) = apply_document_open_at_target(
                    editor,
                    &foreground,
                    &request,
                    target,
                    Some(prepared),
                    None,
                ) else {
                    complete_document_open(
                        editor,
                        &foreground,
                        &request,
                        Err(helix_plugin_api::ContractError::internal(
                            "document-open target disappeared before apply",
                        )),
                    );
                    if stop_on_error {
                        break;
                    }
                    continue;
                };
                previous_view = Some(opened_view);
                complete_document_open(editor, &foreground, &request, Ok(document));
                log::info!(
                    "[document_open] phase=apply_done path={} doc={document:?} generation={generation} lane={lane:?} target={target:?} opened_view={opened_view:?}",
                    request.path.display(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use helix_view::{
        editor::{Action, Config},
        graphics::Rect,
        handlers::Handlers,
        theme,
    };

    use super::*;

    fn test_editor(runtime: helix_runtime::Runtime) -> Editor {
        Editor::new(
            Rect::new(0, 0, 100, 30),
            Arc::new(theme::Loader::new(&[])),
            Arc::new(ArcSwap::from_pointee(
                helix_core::config::default_lang_loader(),
            )),
            Arc::new(ArcSwap::from_pointee(Config::default())),
            runtime,
            Handlers::dummy(),
        )
    }

    #[tokio::test]
    async fn reload_queue_keeps_only_latest_pending_work_per_document() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("main.txt");
        std::fs::write(&path, "one").unwrap();
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime);
        let document = editor.open(&path, Action::VerticalSplit).unwrap();
        let first = editor.prepare_document_reload(document).unwrap();
        let second = editor.prepare_document_reload(document).unwrap();
        let mut state = DocumentReloadQueueState::default();
        state.active.insert(document);

        assert!(state
            .enqueue(first, DocumentReloadOrigin::ReloadAll)
            .is_empty());
        assert!(state
            .enqueue(second, DocumentReloadOrigin::Explicit)
            .is_empty());

        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].origin, DocumentReloadOrigin::Explicit);
        assert_eq!(state.pending[0].generation, 2);
        assert_eq!(state.latest.get(&document), Some(&2));
    }

    fn open_request(path: PathBuf, selection: DocumentOpenSelection) -> DocumentOpenRequest {
        DocumentOpenRequest {
            path,
            action: Action::Replace,
            lane: DocumentOpenLane::Navigation,
            target: DocumentOpenTarget::View(helix_view::ViewId::default()),
            selection,
            alignment: DocumentOpenAlignment::None,
            default_folding_if_new: false,
            fff_record: None,
            external_if_binary: None,
            post_action: crate::runtime::DocumentOpenPostAction::None,
            completion: DocumentOpenCompletionTarget::Editor,
        }
    }

    #[tokio::test]
    async fn open_queue_keeps_only_latest_pending_navigation() {
        let temp = tempfile::tempdir().unwrap();
        let first_path = temp.path().join("first.txt");
        let second_path = temp.path().join("second.txt");
        std::fs::write(&first_path, "first").unwrap();
        std::fs::write(&second_path, "second").unwrap();
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let editor = test_editor(runtime);
        let first_work = editor.prepare_document_open(
            &first_path,
            helix_view::editor::DocumentOpenRole::Interactive,
        );
        let second_work = editor.prepare_document_open(
            &second_path,
            helix_view::editor::DocumentOpenRole::Interactive,
        );
        let mut state = DocumentOpenQueueState::default();
        state.active.insert(DocumentOpenLane::Navigation);

        assert!(state
            .enqueue(
                DocumentOpenLane::Navigation,
                vec![DocumentOpenJob {
                    work: first_work,
                    request: open_request(first_path, DocumentOpenSelection::None),
                }],
                false,
            )
            .is_empty());
        assert!(state
            .enqueue(
                DocumentOpenLane::Navigation,
                vec![DocumentOpenJob {
                    work: second_work,
                    request: open_request(second_path.clone(), DocumentOpenSelection::None),
                }],
                false,
            )
            .is_empty());

        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].jobs[0].request.path, second_path);
        assert_eq!(state.pending[0].generation, 2);
        assert_eq!(state.latest.get(&DocumentOpenLane::Navigation), Some(&2));
    }

    #[tokio::test]
    async fn plugin_open_lanes_do_not_supersede_each_other() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let editor = test_editor(runtime);
        let mut state = DocumentOpenQueueState::default();
        let first = 1;
        let second = 2;

        let enqueue = |state: &mut DocumentOpenQueueState, operation: u64, name: &str| {
            let path = temp.path().join(name);
            std::fs::write(&path, name).unwrap();
            let work = editor
                .prepare_document_open(&path, helix_view::editor::DocumentOpenRole::Interactive);
            let mut request = open_request(path, DocumentOpenSelection::None);
            request.lane = DocumentOpenLane::Plugin(operation);
            state.enqueue(request.lane, vec![DocumentOpenJob { request, work }], false)
        };

        let first_ready = enqueue(&mut state, first, "first.txt");
        let second_ready = enqueue(&mut state, second, "second.txt");

        assert_eq!(first_ready.len(), 1);
        assert_eq!(second_ready.len(), 1);
        assert!(state.active.contains(&DocumentOpenLane::Plugin(first)));
        assert!(state.active.contains(&DocumentOpenLane::Plugin(second)));
    }

    #[tokio::test]
    async fn canceling_plugin_open_drops_pending_and_invalidates_active_completion() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pending.txt");
        std::fs::write(&path, "pending").unwrap();
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let editor = test_editor(runtime);
        let operation = 1;
        let lane = DocumentOpenLane::Plugin(operation);
        let work =
            editor.prepare_document_open(&path, helix_view::editor::DocumentOpenRole::Interactive);
        let mut request = open_request(path, DocumentOpenSelection::None);
        request.lane = lane;
        let mut state = DocumentOpenQueueState::default();
        state.active.insert(lane);

        assert!(state
            .enqueue(lane, vec![DocumentOpenJob { request, work }], false)
            .is_empty());
        let generation = state.latest[&lane];
        state.cancel(lane);

        assert!(state.pending.is_empty());
        assert!(!state.latest.contains_key(&lane));
        assert_ne!(state.latest.get(&lane).copied(), Some(generation));
    }

    #[tokio::test]
    async fn queued_open_updates_an_existing_document_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("existing.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime.clone());
        let document = editor.open(&path, Action::VerticalSplit).unwrap();
        let (ingress, _receiver) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let foreground = crate::runtime::ForegroundEvents::new();
        let mut request = open_request(
            path,
            DocumentOpenSelection::Position(helix_core::Position::new(1, 0)),
        );
        request.target = DocumentOpenTarget::View(editor.focused_view_id());

        queue_document_open(&mut editor, &ingress, &foreground, request);

        assert_eq!(editor.focused_document_id(), document);
        let view = editor.focused_view_id();
        let doc = editor.document(document).unwrap();
        assert_eq!(
            doc.selection(view).primary().cursor(doc.text().slice(..)),
            6
        );
    }

    #[tokio::test]
    async fn late_open_applies_to_its_invocation_view_without_stealing_focus() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("target.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime);
        editor.new_file(Action::VerticalSplit);
        let target_view = editor.focused_view_id();
        editor.new_file(Action::VerticalSplit);
        let current_view = editor.focused_view_id();
        assert_ne!(target_view, current_view);
        let prepared = editor
            .prepare_document_open(&path, helix_view::editor::DocumentOpenRole::Interactive)
            .execute()
            .unwrap();
        let mut request = open_request(
            path.clone(),
            DocumentOpenSelection::Position(helix_core::Position::new(1, 0)),
        );
        request.target = DocumentOpenTarget::View(target_view);

        let foreground = crate::runtime::ForegroundEvents::new();
        let (document, opened_view) = apply_document_open_at_target(
            &mut editor,
            &foreground,
            &request,
            target_view,
            Some(prepared),
            None,
        )
        .expect("open applies");

        assert_eq!(opened_view, target_view);
        assert_eq!(editor.focused_view_id(), current_view);
        assert_eq!(editor.tree.get(target_view).doc, document);
        assert_eq!(editor.document(document).unwrap().path(), Some(&path));
        let doc = editor.document(document).unwrap();
        assert_eq!(
            doc.selection(target_view)
                .primary()
                .cursor(doc.text().slice(..)),
            6
        );
    }

    #[tokio::test]
    async fn prepared_open_applies_language_server_range() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("target.txt");
        std::fs::write(&path, "zero\nalpha beta\n").unwrap();
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime.clone());
        editor.new_file(Action::VerticalSplit);
        let target_view = editor.focused_view_id();
        let prepared = editor
            .prepare_document_open(&path, helix_view::editor::DocumentOpenRole::Interactive)
            .execute()
            .unwrap();
        let mut request = open_request(
            path,
            DocumentOpenSelection::LspRange {
                range: helix_lsp::lsp::Range::new(
                    helix_lsp::lsp::Position::new(1, 0),
                    helix_lsp::lsp::Position::new(1, 5),
                ),
                offset_encoding: helix_lsp::OffsetEncoding::Utf16,
            },
        );
        request.target = DocumentOpenTarget::View(target_view);
        request.alignment = DocumentOpenAlignment::CenterIfAction;
        let foreground = crate::runtime::ForegroundEvents::new();

        let (document, opened_view) = apply_document_open_at_target(
            &mut editor,
            &foreground,
            &request,
            target_view,
            Some(prepared),
            None,
        )
        .expect("open applies");

        let doc = editor.document(document).unwrap();
        assert_eq!(opened_view, target_view);
        assert_eq!(
            doc.selection(target_view)
                .primary()
                .fragment(doc.text().slice(..)),
            "alpha"
        );
    }

    #[tokio::test]
    async fn insert_file_completion_applies_to_the_invocation_document() {
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime.clone());
        let document = editor.new_file(Action::VerticalSplit);
        let view = editor.focused_view_id();
        let doc = editor.document(document).unwrap();
        let version = doc.version();
        let selection = doc.selection(view).clone();
        let (ingress, _receiver) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let foreground = crate::runtime::ForegroundEvents::new();

        apply_document_command(
            &mut editor,
            ingress,
            foreground,
            DocumentCommand::InsertFileFinished {
                document,
                view,
                version,
                selection,
                scrolloff: 0,
                path: PathBuf::from("insert.txt"),
                result: Ok("loaded".to_owned()),
            },
        );

        assert!(editor
            .document(document)
            .unwrap()
            .text()
            .to_string()
            .ends_with("loaded"));
    }

    #[tokio::test]
    async fn insert_file_completion_rejects_a_stale_document_version() {
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime.clone());
        let document = editor.new_file(Action::VerticalSplit);
        let view = editor.focused_view_id();
        let doc = editor.document(document).unwrap();
        let version = doc.version();
        let selection = doc.selection(view).clone();
        {
            let doc = editor.document_mut(document).unwrap();
            let transaction = Transaction::insert(doc.text(), &selection, Tendril::from("changed"));
            doc.apply(&transaction, view);
        }
        let changed_text = editor.document(document).unwrap().text().to_string();
        let (ingress, _receiver) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let foreground = crate::runtime::ForegroundEvents::new();

        apply_document_command(
            &mut editor,
            ingress,
            foreground,
            DocumentCommand::InsertFileFinished {
                document,
                view,
                version,
                selection,
                scrolloff: 0,
                path: PathBuf::from("insert.txt"),
                result: Ok("loaded".to_owned()),
            },
        );

        assert_eq!(
            editor.document(document).unwrap().text().to_string(),
            changed_text
        );
    }
}
