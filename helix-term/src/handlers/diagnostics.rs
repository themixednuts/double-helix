use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::pending;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use helix_core::diagnostic::DiagnosticProvider;
use helix_core::syntax::config::LanguageServerFeature;
use helix_core::Uri;
use helix_lsp::{lsp, LanguageServerId};
use helix_runtime::{Clock, PulseGate, PulseHandle, PulseReceiver, Task, Token, Work};
use helix_view::bench::log_command_phase;
use helix_view::document::Mode;
use helix_view::DocumentId;

use crate::effect::language_server::{
    queue_document_diagnostics, queue_document_diagnostics_for_language_servers,
};
use crate::runtime::ingress::RuntimeTaskSink;
use crate::runtime::RuntimeTaskEvent;

const DOCUMENT_DEBOUNCE: Duration = Duration::from_millis(250);
const INTER_FILE_SWEEP_DEBOUNCE: Duration = Duration::from_secs(1);
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(4);
const RETRY_LIMIT: u8 = 5;
const SERVER_CANCELLED: i64 = -32802;

pub(super) fn attach(editor: &helix_view::Editor, ingress: crate::runtime::RuntimeIngress) {
    editor.lifecycle().on_diagnostics_change(move |event| {
        if event.editor.mode != Mode::Insert {
            for (view, _) in event.editor.tree.views_mut() {
                view.diagnostics_handler.refresh()
            }
        }
        Ok(())
    });
    let change_ingress = ingress.clone();
    editor.lifecycle().on_document_change(move |event| {
        let hook_start = std::time::Instant::now();
        if event
            .doc
            .has_language_server_with_feature(LanguageServerFeature::PullDiagnostics)
            && !event.ghost_transaction
        {
            let document_id = event.doc.id();
            change_ingress.debounce_pull_diagnostics_document(document_id);

            let inter_file_dependencies_language_servers = event
                .doc
                .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
                .filter(|language_server| {
                    language_server
                        .capabilities()
                        .diagnostic_provider
                        .as_ref()
                        .is_some_and(|diagnostic_provider| match diagnostic_provider {
                            lsp::DiagnosticServerCapabilities::Options(options) => {
                                options.inter_file_dependencies
                            }

                            lsp::DiagnosticServerCapabilities::RegistrationOptions(options) => {
                                options.diagnostic_options.inter_file_dependencies
                            }
                        })
                })
                .map(|language_server| language_server.id())
                .collect();

            change_ingress.debounce_pull_diagnostics_inter_file_sweep(
                inter_file_dependencies_language_servers,
            );
        }
        let hook_dur = hook_start.elapsed();
        log_command_phase(
            "document_did_change_hook",
            "diagnostics_pull",
            hook_dur,
            || {
                format!(
                    "doc_id={:?} ghost={} lines={} bytes={}",
                    event.doc.id(),
                    event.ghost_transaction,
                    event.doc.text().len_lines(),
                    event.doc.text().len_bytes()
                )
            },
        );
        Ok(())
    });

    let open_ingress = ingress.clone();
    editor.lifecycle().on_document_open(move |event| {
        queue_document_diagnostics(event.editor, [event.doc], None, open_ingress.clone());
        Ok(())
    });

    let attachment_ingress = ingress.clone();
    editor
        .lifecycle()
        .on_document_language_servers_change(move |event| {
            queue_document_diagnostics(event.editor, [event.doc], None, attachment_ingress.clone());
            Ok(())
        });

    editor
        .lifecycle()
        .on_language_server_initialized(move |event| {
            let document_ids = event
                .editor
                .documents_supporting_language_server(event.server_id);
            let language_servers = HashSet::from([event.server_id]);
            queue_document_diagnostics_for_language_servers(
                event.editor,
                document_ids,
                &language_servers,
                ingress.clone(),
            );
            Ok(())
        });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PullDiagnosticsKey {
    pub server_id: LanguageServerId,
    pub document_id: DocumentId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PullDiagnosticsPriority {
    Background,
    Interactive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullDiagnosticsTarget {
    pub server_id: LanguageServerId,
    pub document_id: DocumentId,
    pub generation: u64,
    pub version: i32,
    pub uri: Uri,
    pub priority: PullDiagnosticsPriority,
}

impl PullDiagnosticsTarget {
    fn key(&self) -> PullDiagnosticsKey {
        PullDiagnosticsKey {
            server_id: self.server_id,
            document_id: self.document_id,
        }
    }

    fn is_newer_than(&self, other: &Self) -> bool {
        self.generation > other.generation
            || (self.generation == other.generation && self.version > other.version)
    }
}

#[derive(Debug)]
pub(crate) struct PullDiagnosticsResponse {
    pub result: lsp::DocumentDiagnosticReportResult,
    pub provider: DiagnosticProvider,
    pub uri: Uri,
}

#[derive(Debug)]
pub(crate) enum PullDiagnosticsRequestOutcome {
    Response(PullDiagnosticsResponse),
    Failed(helix_lsp::Error),
    Abandoned,
}

#[derive(Debug)]
enum PullDiagnosticsPulse {}

#[derive(Debug, Default)]
struct PendingPullDiagnostics {
    schedules: HashMap<PullDiagnosticsKey, PullDiagnosticsTarget>,
    completions: VecDeque<(PullDiagnosticsTarget, PullDiagnosticsRequestOutcome)>,
    exited_servers: HashSet<LanguageServerId>,
    documents: HashSet<DocumentId>,
    inter_file_servers: HashSet<LanguageServerId>,
}

#[derive(Clone, Debug)]
pub(crate) struct PullDiagnosticsSender {
    pending: Arc<Mutex<PendingPullDiagnostics>>,
    wake: PulseHandle<PullDiagnosticsPulse>,
}

#[derive(Debug)]
pub(crate) struct PullDiagnosticsInbox {
    pending: Arc<Mutex<PendingPullDiagnostics>>,
    wake: PulseReceiver<PullDiagnosticsPulse>,
}

pub(crate) fn pull_diagnostics_channel() -> (PullDiagnosticsSender, PullDiagnosticsInbox) {
    let pending = Arc::new(Mutex::new(PendingPullDiagnostics::default()));
    let mut wake = PulseGate::new();
    let sender = PullDiagnosticsSender {
        pending: pending.clone(),
        wake: wake.handle(),
    };
    let inbox = PullDiagnosticsInbox {
        pending,
        wake: wake.take_receiver(),
    };
    (sender, inbox)
}

impl PullDiagnosticsSender {
    pub(crate) fn schedule(&self, targets: Vec<PullDiagnosticsTarget>) {
        if targets.is_empty() {
            return;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for target in targets {
            let key = target.key();
            match pending.schedules.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(target);
                }
                Entry::Occupied(mut entry) if target.is_newer_than(entry.get()) => {
                    entry.insert(target);
                }
                Entry::Occupied(_) => {}
            }
        }
        drop(pending);
        self.wake.request();
    }

    pub(crate) fn finish(
        &self,
        target: PullDiagnosticsTarget,
        outcome: PullDiagnosticsRequestOutcome,
    ) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completions
            .push_back((target, outcome));
        self.wake.request();
    }

    pub(crate) fn server_exited(&self, server_id: LanguageServerId) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .exited_servers
            .insert(server_id);
        self.wake.request();
    }

    pub(crate) fn debounce_document(&self, document_id: DocumentId) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .documents
            .insert(document_id);
        self.wake.request();
    }

    pub(crate) fn debounce_inter_file_sweep(&self, language_servers: HashSet<LanguageServerId>) {
        if language_servers.is_empty() {
            return;
        }
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .inter_file_servers
            .extend(language_servers);
        self.wake.request();
    }
}

impl PullDiagnosticsInbox {
    fn take(&self) -> PendingPullDiagnostics {
        std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

#[derive(Debug)]
struct KeyState {
    latest: PullDiagnosticsTarget,
    retry_attempt: u8,
    retry_deadline: Option<Instant>,
}

impl KeyState {
    fn new(target: PullDiagnosticsTarget) -> Self {
        Self {
            latest: target,
            retry_attempt: 0,
            retry_deadline: None,
        }
    }
}

#[derive(Debug, Default)]
struct ServerState {
    in_flight: Option<InFlightPullDiagnostics>,
    trailing: Option<PullDiagnosticsTarget>,
    queue: VecDeque<PullDiagnosticsKey>,
    queued: HashSet<PullDiagnosticsKey>,
}

#[derive(Debug)]
struct InFlightPullDiagnostics {
    target: PullDiagnosticsTarget,
    cancel: Token,
}

impl ServerState {
    fn queue(&mut self, key: PullDiagnosticsKey, priority: PullDiagnosticsPriority) {
        if self.queued.insert(key) {
            match priority {
                PullDiagnosticsPriority::Interactive => self.queue.push_front(key),
                PullDiagnosticsPriority::Background => self.queue.push_back(key),
            }
        } else if priority == PullDiagnosticsPriority::Interactive {
            self.queue.retain(|queued| *queued != key);
            self.queue.push_front(key);
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PullDiagnosticsCoordinator {
    keys: HashMap<PullDiagnosticsKey, KeyState>,
    servers: HashMap<LanguageServerId, ServerState>,
    debounced_documents: HashSet<DocumentId>,
    document_deadline: Option<Instant>,
    inter_file_servers: HashSet<LanguageServerId>,
    inter_file_deadline: Option<Instant>,
}

impl PullDiagnosticsCoordinator {
    pub(crate) fn spawn(
        work: Work,
        clock: Clock,
        mut inbox: PullDiagnosticsInbox,
        task_sink: RuntimeTaskSink,
    ) {
        work.spawn(async move {
            let mut coordinator = Self::default();

            loop {
                let timer = coordinator
                    .next_deadline()
                    .map(|deadline| clock.timer_at(deadline));
                let mut events = Vec::new();
                tokio::select! {
                    wake = inbox.wake.recv() => {
                        if wake.is_none() {
                            break;
                        }
                        let now = clock.now();
                        let pending = inbox.take();
                        for server_id in pending.exited_servers {
                            coordinator.clear_server(server_id);
                        }
                        events.extend(coordinator.schedule_targets(
                            pending.schedules.into_values().collect(),
                            now,
                        ));
                        for (target, outcome) in pending.completions {
                            events.extend(coordinator.finish_request(target, outcome, now));
                        }
                        for document_id in pending.documents {
                            coordinator.debounce_document(document_id, now);
                        }
                        coordinator.debounce_inter_file_sweep(pending.inter_file_servers, now);
                    }
                    () = wait_until(timer) => {}
                }

                events.extend(coordinator.handle_deadlines(clock.now()));
                for event in events {
                    if !task_sink.send(event).await {
                        return;
                    }
                }
            }
        })
        .detach();
    }

    fn debounce_document(&mut self, document_id: DocumentId, now: Instant) {
        self.debounced_documents.insert(document_id);
        self.document_deadline = Some(now + DOCUMENT_DEBOUNCE);
    }

    fn debounce_inter_file_sweep(
        &mut self,
        language_servers: HashSet<LanguageServerId>,
        now: Instant,
    ) {
        if language_servers.is_empty() {
            return;
        }
        self.inter_file_servers.extend(language_servers);
        self.inter_file_deadline = Some(now + INTER_FILE_SWEEP_DEBOUNCE);
    }

    fn schedule_targets(
        &mut self,
        targets: Vec<PullDiagnosticsTarget>,
        now: Instant,
    ) -> Vec<RuntimeTaskEvent> {
        let mut affected_servers = HashSet::new();

        for target in targets {
            let server_id = target.server_id;
            let priority = target.priority;
            let key = target.key();
            let is_fresh = match self.keys.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(KeyState::new(target.clone()));
                    true
                }
                Entry::Occupied(mut entry) => {
                    if !target.is_newer_than(&entry.get().latest) {
                        continue;
                    }
                    let state = entry.get_mut();
                    state.latest = target.clone();
                    state.retry_attempt = 0;
                    state.retry_deadline = None;
                    true
                }
            };

            if !is_fresh {
                continue;
            }

            let server = self.servers.entry(server_id).or_default();
            if let Some(in_flight) = server
                .in_flight
                .as_ref()
                .filter(|in_flight| in_flight.target.key() == key)
            {
                in_flight.cancel.cancel();
                server.trailing = Some(target);
            } else {
                server.queue(key, priority);
            }
            affected_servers.insert(server_id);
        }

        affected_servers
            .into_iter()
            .filter_map(|server_id| self.start_next(server_id, now))
            .collect()
    }

    fn finish_request(
        &mut self,
        target: PullDiagnosticsTarget,
        outcome: PullDiagnosticsRequestOutcome,
        now: Instant,
    ) -> Vec<RuntimeTaskEvent> {
        let Some(server) = self.servers.get_mut(&target.server_id) else {
            return Vec::new();
        };
        if server
            .in_flight
            .as_ref()
            .is_none_or(|in_flight| in_flight.target != target)
        {
            return Vec::new();
        }

        server.in_flight = None;
        let trailing = server.trailing.take();
        let key = target.key();
        let is_current = self
            .keys
            .get(&key)
            .is_some_and(|state| state.latest == target);
        let has_newer_target = trailing.is_some() || !is_current;
        let mut events = Vec::new();

        match outcome {
            PullDiagnosticsRequestOutcome::Response(response) => {
                if let Some(state) = self.keys.get_mut(&key) {
                    state.retry_attempt = 0;
                    state.retry_deadline = None;
                }
                if is_current {
                    events.push(RuntimeTaskEvent::PullDiagnosticsResponse {
                        target: target.clone(),
                        uri: response.uri,
                        provider: response.provider,
                        result: response.result,
                    });
                }
                if has_newer_target {
                    self.enqueue_key(key);
                }
            }
            PullDiagnosticsRequestOutcome::Failed(error)
                if should_retry(&error) && !has_newer_target =>
            {
                let state = self
                    .keys
                    .get_mut(&key)
                    .expect("in-flight pull diagnostics key is tracked");
                state.retry_attempt = state.retry_attempt.saturating_add(1);
                if state.retry_attempt <= RETRY_LIMIT {
                    let delay = retry_delay(state.retry_attempt);
                    state.retry_deadline = Some(now + delay);
                    log::debug!(
                        "pull diagnostics retry scheduled for server {} document {:?} in {:?}: {}",
                        target.server_id,
                        target.document_id,
                        delay,
                        error,
                    );
                } else {
                    state.retry_deadline = None;
                    log::warn!(
                        "pull diagnostics retry limit reached for server {} document {:?}: {}",
                        target.server_id,
                        target.document_id,
                        error,
                    );
                }
            }
            PullDiagnosticsRequestOutcome::Failed(error) if should_retry(&error) => {
                if let Some(state) = self.keys.get_mut(&key) {
                    state.retry_attempt = 0;
                    state.retry_deadline = None;
                }
                log::debug!(
                    "superseded pull diagnostics request cancelled for server {} document {:?}: {}",
                    target.server_id,
                    target.document_id,
                    error,
                );
                self.enqueue_key(key);
            }
            PullDiagnosticsRequestOutcome::Failed(error) => {
                log::error!(
                    "pull diagnostics failed for server {} document {:?}: {}",
                    target.server_id,
                    target.document_id,
                    error,
                );
                if has_newer_target {
                    self.enqueue_key(key);
                }
            }
            PullDiagnosticsRequestOutcome::Abandoned => {
                if has_newer_target {
                    self.enqueue_key(key);
                }
            }
        }

        if let Some(event) = self.start_next(target.server_id, now) {
            events.push(event);
        }
        events
    }

    fn enqueue_key(&mut self, key: PullDiagnosticsKey) {
        let Some(target) = self.keys.get(&key).map(|state| state.latest.clone()) else {
            return;
        };
        let server = self.servers.entry(key.server_id).or_default();
        if server
            .in_flight
            .as_ref()
            .is_some_and(|in_flight| in_flight.target.key() == key)
        {
            server.trailing = Some(target);
        } else {
            server.queue(key, target.priority);
        }
    }

    fn start_next(
        &mut self,
        server_id: LanguageServerId,
        now: Instant,
    ) -> Option<RuntimeTaskEvent> {
        let server = self.servers.get_mut(&server_id)?;
        if server.in_flight.is_some() {
            return None;
        }

        let queued = server.queue.len();
        for _ in 0..queued {
            let Some(key) = server.queue.pop_front() else {
                break;
            };
            server.queued.remove(&key);
            let Some(state) = self.keys.get(&key) else {
                continue;
            };
            if state.retry_deadline.is_some_and(|deadline| deadline > now) {
                server.queue.push_back(key);
                server.queued.insert(key);
                continue;
            }
            let target = state.latest.clone();
            let cancel = Token::new();
            server.in_flight = Some(InFlightPullDiagnostics {
                target: target.clone(),
                cancel: cancel.clone(),
            });
            debug_assert!(server.trailing.is_none());
            return Some(RuntimeTaskEvent::StartPullDiagnostics { target, cancel });
        }
        None
    }

    fn handle_deadlines(&mut self, now: Instant) -> Vec<RuntimeTaskEvent> {
        let mut events = Vec::new();
        if self
            .document_deadline
            .is_some_and(|deadline| deadline <= now)
        {
            self.document_deadline = None;
            let document_ids = std::mem::take(&mut self.debounced_documents);
            if !document_ids.is_empty() {
                events.push(RuntimeTaskEvent::QueuePullDiagnosticsForDocuments { document_ids });
            }
        }
        if self
            .inter_file_deadline
            .is_some_and(|deadline| deadline <= now)
        {
            self.inter_file_deadline = None;
            let language_servers = std::mem::take(&mut self.inter_file_servers);
            if !language_servers.is_empty() {
                events.push(RuntimeTaskEvent::QueuePullDiagnosticsInterFileSweep {
                    language_servers,
                });
            }
        }

        let due_keys = self
            .keys
            .iter()
            .filter_map(|(key, state)| {
                state
                    .retry_deadline
                    .is_some_and(|deadline| deadline <= now)
                    .then_some(*key)
            })
            .collect::<Vec<_>>();
        let mut affected_servers = HashSet::new();
        for key in due_keys {
            if let Some(state) = self.keys.get_mut(&key) {
                state.retry_deadline = None;
            }
            self.enqueue_key(key);
            affected_servers.insert(key.server_id);
        }
        events.extend(
            affected_servers
                .into_iter()
                .filter_map(|server_id| self.start_next(server_id, now)),
        );
        events
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.document_deadline
            .into_iter()
            .chain(self.inter_file_deadline)
            .chain(self.keys.values().filter_map(|state| state.retry_deadline))
            .min()
    }

    fn clear_server(&mut self, server_id: LanguageServerId) {
        if let Some(server) = self.servers.remove(&server_id) {
            if let Some(in_flight) = server.in_flight {
                in_flight.cancel.cancel();
            }
        }
        self.keys.retain(|key, _| key.server_id != server_id);
        self.inter_file_servers.remove(&server_id);
        if self.inter_file_servers.is_empty() {
            self.inter_file_deadline = None;
        }
    }
}

async fn wait_until(timer: Option<Task<()>>) {
    match timer {
        Some(timer) => {
            let _ = timer.await;
        }
        None => pending().await,
    }
}

fn should_retry(error: &helix_lsp::Error) -> bool {
    match error {
        helix_lsp::Error::Rpc(error) if error.code.code() == SERVER_CANCELLED => {
            error
                .data
                .as_ref()
                .and_then(|data| {
                    serde_json::from_value::<lsp::DiagnosticServerCancellationData>(data.clone())
                        .ok()
                })
                .unwrap_or_default()
                .retrigger_request
        }
        helix_lsp::Error::Timeout(_)
        | helix_lsp::Error::StreamClosed
        | helix_lsp::Error::IO(_)
        | helix_lsp::Error::Other(_) => true,
        _ => false,
    }
}

fn retry_delay(attempt: u8) -> Duration {
    let multiplier = 1u32 << attempt.saturating_sub(1).min(15);
    RETRY_BASE_DELAY
        .checked_mul(multiplier)
        .unwrap_or(RETRY_MAX_DELAY)
        .min(RETRY_MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use helix_lsp::jsonrpc::{Error as RpcError, ErrorCode};
    use helix_runtime::test::RuntimeTest;

    use super::*;
    use crate::runtime::{RuntimeDelivery, RuntimeIngress};

    fn document_id(raw: usize) -> DocumentId {
        DocumentId::new(NonZeroUsize::new(raw).expect("non-zero document id"))
    }

    fn target(document_id: DocumentId, generation: u64) -> PullDiagnosticsTarget {
        PullDiagnosticsTarget {
            server_id: LanguageServerId::default(),
            document_id,
            generation,
            version: generation as i32,
            uri: Uri::from(std::path::PathBuf::from(format!(
                "diagnostic-test-{document_id:?}"
            ))),
            priority: PullDiagnosticsPriority::Interactive,
        }
    }

    fn server_cancelled_without_data() -> helix_lsp::Error {
        helix_lsp::Error::Rpc(RpcError {
            code: ErrorCode::ServerError(SERVER_CANCELLED),
            message: "server cancelled".to_string(),
            data: None,
        })
    }

    fn start_targets(events: &[RuntimeTaskEvent]) -> Vec<PullDiagnosticsTarget> {
        events
            .iter()
            .filter_map(|event| match event {
                RuntimeTaskEvent::StartPullDiagnostics { target, .. } => Some(target.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn paused_clock_coalesces_document_and_inter_file_deadlines() {
        let test_runtime = RuntimeTest::new_paused();
        let runtime = test_runtime.runtime();
        let (ingress, mut runtime_events) = RuntimeIngress::channel(runtime.clone());
        let server_id = LanguageServerId::default();

        for index in 0..100 {
            ingress.debounce_pull_diagnostics_document(document_id(index % 11 + 1));
            ingress.debounce_pull_diagnostics_inter_file_sweep(HashSet::from([server_id]));
        }
        test_runtime.block_on(async {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
        });

        test_runtime.advance(DOCUMENT_DEBOUNCE - Duration::from_millis(1));
        assert!(runtime_events.try_recv().is_err());

        test_runtime.advance(Duration::from_millis(1));
        let RuntimeDelivery::Task(task) =
            runtime_events.try_recv().expect("document debounce event")
        else {
            panic!("expected document debounce event");
        };
        let RuntimeTaskEvent::QueuePullDiagnosticsForDocuments { document_ids } = *task else {
            panic!("expected document debounce event");
        };
        assert_eq!(document_ids.len(), 11);
        assert!(runtime_events.try_recv().is_err());

        test_runtime.advance(INTER_FILE_SWEEP_DEBOUNCE - DOCUMENT_DEBOUNCE);
        let RuntimeDelivery::Task(task) =
            runtime_events.try_recv().expect("inter-file sweep event")
        else {
            panic!("expected inter-file sweep event");
        };
        let RuntimeTaskEvent::QueuePullDiagnosticsInterFileSweep { language_servers } = *task
        else {
            panic!("expected inter-file sweep event");
        };
        assert_eq!(language_servers, HashSet::from([server_id]));
        assert!(runtime_events.try_recv().is_err());
    }

    #[test]
    fn refresh_and_retrigger_keep_unique_queue_and_one_trailing_target() {
        let test_runtime = RuntimeTest::new_paused();
        let now = test_runtime.block_on(async { Instant::now() });
        let mut coordinator = PullDiagnosticsCoordinator::default();
        let documents = (1..=11).map(document_id).collect::<Vec<_>>();
        let events = coordinator.schedule_targets(
            documents
                .iter()
                .copied()
                .map(|doc| target(doc, 1))
                .collect(),
            now,
        );
        let in_flight = start_targets(&events)[0].clone();
        assert_eq!(start_targets(&events).len(), 1);

        let mut generations = [1u64; 11];
        for index in 0..100 {
            let document_index = index % documents.len();
            generations[document_index] += 1;
            let events = coordinator.schedule_targets(
                vec![target(
                    documents[document_index],
                    generations[document_index],
                )],
                now,
            );
            assert!(start_targets(&events).is_empty());
        }

        let server = coordinator
            .servers
            .get(&LanguageServerId::default())
            .expect("server state");
        assert_eq!(
            server.in_flight.as_ref().map(|request| &request.target),
            Some(&in_flight)
        );
        assert_eq!(server.queue.len(), 10);
        assert_eq!(server.queued.len(), 10);
        assert_eq!(
            server.trailing.as_ref().map(PullDiagnosticsTarget::key),
            Some(in_flight.key())
        );

        let events = coordinator.finish_request(
            in_flight.clone(),
            PullDiagnosticsRequestOutcome::Failed(server_cancelled_without_data()),
            now,
        );
        assert_eq!(start_targets(&events).len(), 1);
        let server = coordinator
            .servers
            .get(&LanguageServerId::default())
            .expect("server state");
        assert!(server.in_flight.is_some());
        assert!(server.trailing.is_none());
        assert_eq!(server.queue.len(), 10);
        assert_eq!(server.queued.len(), 10);
        assert_eq!(coordinator.keys[&in_flight.key()].retry_deadline, None);
        assert_eq!(coordinator.keys[&in_flight.key()].retry_attempt, 0);
    }

    #[test]
    fn superseding_an_in_flight_request_cancels_it_and_starts_latest_without_retry() {
        let test_runtime = RuntimeTest::new_paused();
        let now = test_runtime.block_on(async { Instant::now() });
        let mut coordinator = PullDiagnosticsCoordinator::default();
        let first = target(document_id(1), 1);
        let events = coordinator.schedule_targets(vec![first.clone()], now);
        let RuntimeTaskEvent::StartPullDiagnostics { cancel, .. } = &events[0] else {
            panic!("expected first pull request");
        };
        let cancel = cancel.clone();

        let latest = target(document_id(1), 2);
        assert!(coordinator
            .schedule_targets(vec![latest.clone()], now)
            .is_empty());
        assert!(cancel.is_canceled());

        let events = coordinator.finish_request(
            first.clone(),
            PullDiagnosticsRequestOutcome::Failed(server_cancelled_without_data()),
            now,
        );
        assert_eq!(start_targets(&events), vec![latest]);
        assert_eq!(coordinator.keys[&first.key()].retry_attempt, 0);
        assert!(coordinator.keys[&first.key()].retry_deadline.is_none());
    }

    #[test]
    fn interactive_target_is_promoted_ahead_of_background_sweep() {
        let test_runtime = RuntimeTest::new_paused();
        let now = test_runtime.block_on(async { Instant::now() });
        let mut coordinator = PullDiagnosticsCoordinator::default();
        let in_flight = target(document_id(1), 1);
        coordinator.schedule_targets(vec![in_flight.clone()], now);

        let mut background_a = target(document_id(2), 1);
        background_a.priority = PullDiagnosticsPriority::Background;
        let mut background_b = target(document_id(3), 1);
        background_b.priority = PullDiagnosticsPriority::Background;
        let interactive = target(document_id(4), 1);
        coordinator.schedule_targets(vec![background_a, background_b, interactive.clone()], now);

        let events =
            coordinator.finish_request(in_flight, PullDiagnosticsRequestOutcome::Abandoned, now);
        assert_eq!(start_targets(&events), vec![interactive]);
    }

    #[test]
    fn sender_coalesces_latest_schedule_and_debounce_intents() {
        let (sender, inbox) = pull_diagnostics_channel();
        let doc = document_id(1);
        let server = LanguageServerId::default();
        for generation in 1..=1_000 {
            sender.schedule(vec![target(doc, generation)]);
            sender.debounce_document(doc);
            sender.debounce_inter_file_sweep(HashSet::from([server]));
        }

        let pending = inbox.take();
        assert_eq!(pending.schedules.len(), 1);
        assert_eq!(pending.schedules.values().next().unwrap().generation, 1_000);
        assert_eq!(pending.documents, HashSet::from([doc]));
        assert_eq!(pending.inter_file_servers, HashSet::from([server]));
    }

    #[test]
    fn server_cancelled_without_data_retries_with_bounded_backoff() {
        let test_runtime = RuntimeTest::new_paused();
        let mut now = test_runtime.block_on(async { Instant::now() });
        let mut coordinator = PullDiagnosticsCoordinator::default();
        let target = target(document_id(1), 1);
        let mut events = coordinator.schedule_targets(vec![target.clone()], now);
        assert_eq!(start_targets(&events), vec![target.clone()]);

        for attempt in 1..=RETRY_LIMIT {
            events = coordinator.finish_request(
                target.clone(),
                PullDiagnosticsRequestOutcome::Failed(server_cancelled_without_data()),
                now,
            );
            assert!(start_targets(&events).is_empty());
            let deadline = now + retry_delay(attempt);
            assert_eq!(
                coordinator.keys[&target.key()].retry_deadline,
                Some(deadline)
            );

            now = deadline;
            events = coordinator.handle_deadlines(now);
            assert_eq!(start_targets(&events), vec![target.clone()]);
        }

        events = coordinator.finish_request(
            target.clone(),
            PullDiagnosticsRequestOutcome::Failed(server_cancelled_without_data()),
            now,
        );
        assert!(start_targets(&events).is_empty());
        assert!(coordinator.keys[&target.key()].retry_deadline.is_none());
        assert_eq!(
            coordinator.keys[&target.key()].retry_attempt,
            RETRY_LIMIT + 1
        );
    }

    #[test]
    fn timeout_and_transport_loss_are_retryable() {
        assert!(should_retry(&helix_lsp::Error::Timeout(
            helix_lsp::jsonrpc::Id::Num(1)
        )));
        assert!(should_retry(&helix_lsp::Error::StreamClosed));
        assert!(should_retry(&helix_lsp::Error::IO(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "transport closed"
        ))));

        let explicit_no_retrigger = helix_lsp::Error::Rpc(RpcError {
            code: ErrorCode::ServerError(SERVER_CANCELLED),
            message: "cancelled".to_string(),
            data: Some(serde_json::json!({ "retriggerRequest": false })),
        });
        assert!(!should_retry(&explicit_no_retrigger));
    }

    #[test]
    fn server_exit_clears_in_flight_queue_retry_and_sweep_state() {
        let test_runtime = RuntimeTest::new_paused();
        let now = test_runtime.block_on(async { Instant::now() });
        let server_id = LanguageServerId::default();
        let mut coordinator = PullDiagnosticsCoordinator::default();
        coordinator.debounce_inter_file_sweep(HashSet::from([server_id]), now);
        let events = coordinator.schedule_targets(
            vec![target(document_id(1), 1), target(document_id(2), 1)],
            now,
        );
        let in_flight = start_targets(&events)[0].clone();
        coordinator.finish_request(
            in_flight,
            PullDiagnosticsRequestOutcome::Failed(server_cancelled_without_data()),
            now,
        );

        coordinator.clear_server(server_id);

        assert!(!coordinator.servers.contains_key(&server_id));
        assert!(coordinator
            .keys
            .keys()
            .all(|key| key.server_id != server_id));
        assert!(!coordinator.inter_file_servers.contains(&server_id));
        assert!(coordinator.inter_file_deadline.is_none());
    }
}
