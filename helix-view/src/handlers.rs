use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use completion::{CompletionEvent, CompletionHandler};
use helix_runtime::{channel, PulseGate, PulseHandle, Runtime, Sender, Work};

use crate::editor::Action;
use crate::handlers::lsp::SignatureHelpInvoked;
use crate::{DocumentId, Editor, ViewId};

pub mod completion;
pub mod dap;
pub mod diagnostics;
pub mod lsp;
pub mod word_index;
pub mod workspace_edit;

#[derive(Debug)]
pub enum AutoSaveEvent {
    DocumentChanged { save_after: u64 },
    LeftInsertMode,
}

#[derive(Debug, Clone)]
pub struct BlameEvent {
    /// The path for which we request blame
    pub path: std::path::PathBuf,
    /// Document for which the blame is requested
    pub doc_id: DocumentId,
    /// If this field is set, when we obtain the blame for the file we will
    /// show blame for this line in the status line
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationPurpose {
    CollaborationReveal,
    AssistantFollow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationRequest {
    pub path: std::path::PathBuf,
    pub action: Action,
    pub target: ViewId,
    pub range: Option<helix_core::Range>,
    pub purpose: NavigationPurpose,
}

#[derive(Debug)]
pub enum AutoReloadEvent {
    /// One or more open documents may have changed on disk.
    DocumentsChanged {
        doc_ids: Vec<crate::DocumentId>,
    },
    LeftInsertMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgEvent {
    MissingLanguageServer {
        documents: BTreeSet<DocumentId>,
        server: String,
        language: String,
        command: String,
        config: helix_pkg::PkgConfig,
        config_generation: u64,
        runtime_generation: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PkgEventKey {
    server: String,
    language: String,
    command: String,
}

impl PkgEvent {
    fn key(&self) -> PkgEventKey {
        let Self::MissingLanguageServer {
            server,
            language,
            command,
            ..
        } = self;
        PkgEventKey {
            server: server.clone(),
            language: language.clone(),
            command: command.clone(),
        }
    }

    fn merge(&mut self, newer: Self) {
        let Self::MissingLanguageServer { documents, .. } = self;
        let mut documents = std::mem::take(documents);
        let Self::MissingLanguageServer {
            documents: newer_documents,
            ..
        } = &newer;
        documents.extend(newer_documents.iter().copied());
        *self = newer;
        let Self::MissingLanguageServer {
            documents: current_documents,
            ..
        } = self;
        *current_documents = documents;
    }
}

trait CoalescingState: Default + Send + 'static {
    type Event: Send + 'static;
    type Delivery: Send + 'static;

    fn push(&mut self, event: Self::Event);
    fn begin_delivery(&mut self) -> Option<(Self::Event, Self::Delivery)>;
    fn finish_delivery(&mut self, delivery: Self::Delivery);
    fn clear(&mut self);
}

enum EventRelayWake {}

struct EventRelay<S>
where
    S: CoalescingState,
{
    pending: Arc<Mutex<S>>,
    wake: PulseHandle<EventRelayWake>,
    closed: Arc<AtomicBool>,
}

impl<S> Clone for EventRelay<S>
where
    S: CoalescingState,
{
    fn clone(&self) -> Self {
        Self {
            pending: self.pending.clone(),
            wake: self.wake.clone(),
            closed: self.closed.clone(),
        }
    }
}

impl<S> EventRelay<S>
where
    S: CoalescingState,
{
    fn spawn(work: Work, destination: Sender<S::Event>) -> Self {
        let mut gate = PulseGate::<EventRelayWake>::new();
        let wake = gate.handle();
        let mut wake_rx = gate.take_receiver();
        let pending = Arc::new(Mutex::new(S::default()));
        let actor_pending = pending.clone();
        let actor_destination = destination.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let actor_closed = closed.clone();

        work.spawn(async move {
            while wake_rx.recv().await.is_some() {
                loop {
                    let next = actor_pending
                        .lock()
                        .expect("event relay state lock poisoned")
                        .begin_delivery();
                    let Some((event, delivery)) = next else {
                        break;
                    };
                    if actor_destination.send(event).await.is_err() {
                        actor_closed.store(true, Ordering::Release);
                        actor_pending
                            .lock()
                            .expect("event relay state lock poisoned")
                            .clear();
                        return;
                    }
                    actor_pending
                        .lock()
                        .expect("event relay state lock poisoned")
                        .finish_delivery(delivery);
                }
            }
            actor_closed.store(true, Ordering::Release);
            actor_pending
                .lock()
                .expect("event relay state lock poisoned")
                .clear();
        })
        .detach();

        Self {
            pending,
            wake,
            closed,
        }
    }

    fn disconnected(_destination: Sender<S::Event>) -> Self {
        let mut gate = PulseGate::<EventRelayWake>::new();
        drop(gate.take_receiver());
        Self {
            pending: Arc::new(Mutex::new(S::default())),
            wake: gate.handle(),
            closed: Arc::new(AtomicBool::new(true)),
        }
    }

    fn send(&self, event: S::Event) {
        let mut pending = self
            .pending
            .lock()
            .expect("event relay state lock poisoned");
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        pending.push(event);
        drop(pending);
        self.wake.request();
    }
}

#[derive(Default)]
struct PkgEventState {
    pending: BTreeMap<PkgEventKey, PkgEvent>,
    in_flight: Option<PkgEventKey>,
}

impl CoalescingState for PkgEventState {
    type Event = PkgEvent;
    type Delivery = PkgEventKey;

    fn push(&mut self, event: PkgEvent) {
        let key = event.key();
        match self.pending.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(event);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().merge(event);
            }
        }
    }

    fn begin_delivery(&mut self) -> Option<(Self::Event, Self::Delivery)> {
        let (key, event) = self.pending.pop_first()?;
        self.in_flight = Some(key.clone());
        Some((event, key))
    }

    fn finish_delivery(&mut self, delivery: Self::Delivery) {
        if self.in_flight.as_ref() == Some(&delivery) {
            self.in_flight = None;
        }
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.in_flight = None;
    }
}

#[derive(Clone)]
pub struct PkgEvents(EventRelay<PkgEventState>);

impl PkgEvents {
    pub fn spawn(work: Work, destination: Sender<PkgEvent>) -> Self {
        Self(EventRelay::spawn(work, destination))
    }

    fn disconnected(destination: Sender<PkgEvent>) -> Self {
        Self(EventRelay::disconnected(destination))
    }

    pub fn send(&self, event: PkgEvent) {
        self.0.send(event);
    }
}

enum AutoSaveDelivery {
    DocumentChanged,
    LeftInsertMode,
}

#[derive(Default)]
struct AutoSaveEventState {
    sequence: u64,
    document_changed: Option<(u64, u64)>,
    left_insert_mode: Option<u64>,
    in_flight: Option<AutoSaveDelivery>,
}

impl AutoSaveEventState {
    fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        self.sequence
    }
}

impl CoalescingState for AutoSaveEventState {
    type Event = AutoSaveEvent;
    type Delivery = AutoSaveDelivery;

    fn push(&mut self, event: Self::Event) {
        let sequence = self.next_sequence();
        match event {
            AutoSaveEvent::DocumentChanged { save_after } => {
                self.document_changed = Some((sequence, save_after));
            }
            AutoSaveEvent::LeftInsertMode => self.left_insert_mode = Some(sequence),
        }
    }

    fn begin_delivery(&mut self) -> Option<(Self::Event, Self::Delivery)> {
        if self.in_flight.is_some() {
            return None;
        }
        let document_sequence = self
            .document_changed
            .map_or(u64::MAX, |(sequence, _)| sequence);
        let left_sequence = self.left_insert_mode.unwrap_or(u64::MAX);
        if document_sequence <= left_sequence {
            let (_, save_after) = self.document_changed.take()?;
            self.in_flight = Some(AutoSaveDelivery::DocumentChanged);
            Some((
                AutoSaveEvent::DocumentChanged { save_after },
                AutoSaveDelivery::DocumentChanged,
            ))
        } else {
            self.left_insert_mode.take()?;
            self.in_flight = Some(AutoSaveDelivery::LeftInsertMode);
            Some((
                AutoSaveEvent::LeftInsertMode,
                AutoSaveDelivery::LeftInsertMode,
            ))
        }
    }

    fn finish_delivery(&mut self, _delivery: Self::Delivery) {
        self.in_flight = None;
    }

    fn clear(&mut self) {
        self.document_changed = None;
        self.left_insert_mode = None;
        self.in_flight = None;
    }
}

#[derive(Clone)]
pub struct AutoSaveEvents(EventRelay<AutoSaveEventState>);

impl AutoSaveEvents {
    pub fn spawn(work: Work, destination: Sender<AutoSaveEvent>) -> Self {
        Self(EventRelay::spawn(work, destination))
    }

    fn disconnected(destination: Sender<AutoSaveEvent>) -> Self {
        Self(EventRelay::disconnected(destination))
    }

    pub fn send(&self, event: AutoSaveEvent) {
        self.0.send(event);
    }
}

enum AutoReloadDelivery {
    DocumentsChanged,
    LeftInsertMode,
}

#[derive(Default)]
struct AutoReloadEventState {
    sequence: u64,
    documents_changed: Option<(u64, BTreeSet<DocumentId>)>,
    left_insert_mode: Option<u64>,
    in_flight: Option<AutoReloadDelivery>,
}

impl AutoReloadEventState {
    fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        self.sequence
    }
}

impl CoalescingState for AutoReloadEventState {
    type Event = AutoReloadEvent;
    type Delivery = AutoReloadDelivery;

    fn push(&mut self, event: Self::Event) {
        let sequence = self.next_sequence();
        match event {
            AutoReloadEvent::DocumentsChanged { doc_ids } => {
                if let Some((_, documents)) = &mut self.documents_changed {
                    documents.extend(doc_ids);
                } else {
                    self.documents_changed = Some((sequence, doc_ids.into_iter().collect()));
                }
            }
            AutoReloadEvent::LeftInsertMode => self.left_insert_mode = Some(sequence),
        }
    }

    fn begin_delivery(&mut self) -> Option<(Self::Event, Self::Delivery)> {
        if self.in_flight.is_some() {
            return None;
        }
        let file_sequence = self
            .documents_changed
            .as_ref()
            .map_or(u64::MAX, |(sequence, _)| *sequence);
        let left_sequence = self.left_insert_mode.unwrap_or(u64::MAX);
        if file_sequence <= left_sequence {
            let (_, doc_ids) = self.documents_changed.take()?;
            self.in_flight = Some(AutoReloadDelivery::DocumentsChanged);
            Some((
                AutoReloadEvent::DocumentsChanged {
                    doc_ids: doc_ids.into_iter().collect(),
                },
                AutoReloadDelivery::DocumentsChanged,
            ))
        } else {
            self.left_insert_mode.take()?;
            self.in_flight = Some(AutoReloadDelivery::LeftInsertMode);
            Some((
                AutoReloadEvent::LeftInsertMode,
                AutoReloadDelivery::LeftInsertMode,
            ))
        }
    }

    fn finish_delivery(&mut self, _delivery: Self::Delivery) {
        self.in_flight = None;
    }

    fn clear(&mut self) {
        self.documents_changed = None;
        self.left_insert_mode = None;
        self.in_flight = None;
    }
}

#[derive(Clone)]
pub struct AutoReloadEvents(EventRelay<AutoReloadEventState>);

impl AutoReloadEvents {
    pub fn spawn(work: Work, destination: Sender<AutoReloadEvent>) -> Self {
        Self(EventRelay::spawn(work, destination))
    }

    fn disconnected(destination: Sender<AutoReloadEvent>) -> Self {
        Self(EventRelay::disconnected(destination))
    }

    pub fn send(&self, event: AutoReloadEvent) {
        self.0.send(event);
    }
}

#[derive(Default)]
struct DocumentColorsEventState {
    pending: BTreeSet<DocumentId>,
    in_flight: Option<DocumentId>,
}

impl CoalescingState for DocumentColorsEventState {
    type Event = lsp::DocumentColorsEvent;
    type Delivery = DocumentId;

    fn push(&mut self, lsp::DocumentColorsEvent(document_id): Self::Event) {
        if self.in_flight != Some(document_id) {
            self.pending.insert(document_id);
        }
    }

    fn begin_delivery(&mut self) -> Option<(Self::Event, Self::Delivery)> {
        let document_id = self.pending.pop_first()?;
        self.in_flight = Some(document_id);
        Some((lsp::DocumentColorsEvent(document_id), document_id))
    }

    fn finish_delivery(&mut self, delivery: Self::Delivery) {
        if self.in_flight == Some(delivery) {
            self.in_flight = None;
        }
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.in_flight = None;
    }
}

#[derive(Clone)]
pub struct DocumentColorsEvents(EventRelay<DocumentColorsEventState>);

impl DocumentColorsEvents {
    pub fn spawn(work: Work, destination: Sender<lsp::DocumentColorsEvent>) -> Self {
        Self(EventRelay::spawn(work, destination))
    }

    fn disconnected(destination: Sender<lsp::DocumentColorsEvent>) -> Self {
        Self(EventRelay::disconnected(destination))
    }

    pub fn send(&self, event: lsp::DocumentColorsEvent) {
        self.0.send(event);
    }
}

#[derive(Clone, Copy)]
struct LspFeatureRefreshDelivery {
    document_id: DocumentId,
    kind: lsp::LspFeatureRefreshKind,
}

#[derive(Default)]
struct LspFeatureRefreshEventState {
    pending: HashMap<DocumentId, HashSet<lsp::LspFeatureRefreshKind>>,
    in_flight: Option<LspFeatureRefreshDelivery>,
}

impl CoalescingState for LspFeatureRefreshEventState {
    type Event = lsp::LspFeatureRefreshEvent;
    type Delivery = LspFeatureRefreshDelivery;

    fn push(&mut self, event: Self::Event) {
        if self.in_flight.is_some_and(|delivery| {
            delivery.document_id == event.doc_id && delivery.kind == event.kind
        }) {
            return;
        }
        self.pending
            .entry(event.doc_id)
            .or_default()
            .insert(event.kind);
    }

    fn begin_delivery(&mut self) -> Option<(Self::Event, Self::Delivery)> {
        let document_id = *self.pending.keys().next()?;
        let kinds = self.pending.get_mut(&document_id)?;
        let kind = *kinds.iter().next()?;
        kinds.remove(&kind);
        if kinds.is_empty() {
            self.pending.remove(&document_id);
        }
        let delivery = LspFeatureRefreshDelivery { document_id, kind };
        self.in_flight = Some(delivery);
        Some((
            lsp::LspFeatureRefreshEvent {
                doc_id: document_id,
                kind,
            },
            delivery,
        ))
    }

    fn finish_delivery(&mut self, delivery: Self::Delivery) {
        if self.in_flight.is_some_and(|current| {
            current.document_id == delivery.document_id && current.kind == delivery.kind
        }) {
            self.in_flight = None;
        }
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.in_flight = None;
    }
}

#[derive(Clone)]
pub struct LspFeatureRefreshEvents(EventRelay<LspFeatureRefreshEventState>);

impl LspFeatureRefreshEvents {
    pub fn spawn(work: Work, destination: Sender<lsp::LspFeatureRefreshEvent>) -> Self {
        Self(EventRelay::spawn(work, destination))
    }

    fn disconnected(destination: Sender<lsp::LspFeatureRefreshEvent>) -> Self {
        Self(EventRelay::disconnected(destination))
    }

    pub fn send(&self, event: lsp::LspFeatureRefreshEvent) {
        self.0.send(event);
    }
}

#[derive(Clone)]
struct BlameDelivery {
    doc_id: DocumentId,
    path: std::path::PathBuf,
    line: Option<u32>,
}

#[derive(Default)]
struct BlameEventState {
    pending: HashMap<DocumentId, BlameEvent>,
    in_flight: Option<BlameDelivery>,
}

impl BlameEventState {
    fn merge(&mut self, event: BlameEvent) {
        match self.pending.entry(event.doc_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                if self.in_flight.as_ref().is_some_and(|in_flight| {
                    in_flight.doc_id == event.doc_id
                        && (in_flight.line.is_some() && event.line.is_none()
                            || in_flight.path == event.path && in_flight.line == event.line)
                }) {
                    return;
                }
                entry.insert(event);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().line.is_none() || event.line.is_some() {
                    entry.insert(event);
                }
            }
        }
    }
}

impl CoalescingState for BlameEventState {
    type Event = BlameEvent;
    type Delivery = BlameDelivery;

    fn push(&mut self, event: Self::Event) {
        self.merge(event);
    }

    fn begin_delivery(&mut self) -> Option<(Self::Event, Self::Delivery)> {
        let doc_id = *self.pending.keys().next()?;
        let event = self.pending.remove(&doc_id)?;
        let delivery = BlameDelivery {
            doc_id,
            path: event.path.clone(),
            line: event.line,
        };
        self.in_flight = Some(delivery.clone());
        Some((event, delivery))
    }

    fn finish_delivery(&mut self, delivery: Self::Delivery) {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|current| current.doc_id == delivery.doc_id)
        {
            self.in_flight = None;
        }
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.in_flight = None;
    }
}

#[derive(Clone)]
pub struct BlameEvents(EventRelay<BlameEventState>);

impl BlameEvents {
    pub fn spawn(work: Work, destination: Sender<BlameEvent>) -> Self {
        Self(EventRelay::spawn(work, destination))
    }

    fn disconnected(destination: Sender<BlameEvent>) -> Self {
        Self(EventRelay::disconnected(destination))
    }

    pub fn send(&self, event: BlameEvent) {
        self.0.send(event);
    }
}

#[derive(Clone, Copy)]
enum SignatureIntent {
    Invoked,
    Trigger,
    ReTrigger,
}

impl SignatureIntent {
    fn into_event(self) -> lsp::SignatureHelpEvent {
        match self {
            Self::Invoked => lsp::SignatureHelpEvent::Invoked,
            Self::Trigger => lsp::SignatureHelpEvent::Trigger,
            Self::ReTrigger => lsp::SignatureHelpEvent::ReTrigger,
        }
    }
}

enum SignatureDelivery {
    Cancel,
    Intent,
    Completion(lsp::SignatureHelpRequestId),
}

#[derive(Default)]
struct SignatureHelpEventState {
    sequence: u64,
    cancel: Option<u64>,
    intent: Option<(u64, SignatureIntent)>,
    completion: Option<(lsp::SignatureHelpRequestId, u64, bool)>,
    in_flight: Option<SignatureDelivery>,
}

impl SignatureHelpEventState {
    fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        self.sequence
    }
}

impl CoalescingState for SignatureHelpEventState {
    type Event = lsp::SignatureHelpEvent;
    type Delivery = SignatureDelivery;

    fn push(&mut self, event: Self::Event) {
        let sequence = self.next_sequence();
        match event {
            lsp::SignatureHelpEvent::Cancel => {
                self.cancel = Some(sequence);
                self.intent = None;
                self.completion = None;
            }
            lsp::SignatureHelpEvent::Invoked => {
                if !matches!(self.in_flight, Some(SignatureDelivery::Intent))
                    || !matches!(self.intent, Some((_, SignatureIntent::Invoked)))
                {
                    self.intent = Some((sequence, SignatureIntent::Invoked));
                }
            }
            lsp::SignatureHelpEvent::Trigger => {
                if !matches!(self.intent, Some((_, SignatureIntent::Invoked))) {
                    self.intent = Some((sequence, SignatureIntent::Trigger));
                }
            }
            lsp::SignatureHelpEvent::ReTrigger => {
                if !matches!(self.intent, Some((_, SignatureIntent::Invoked))) {
                    self.intent = Some((sequence, SignatureIntent::ReTrigger));
                }
            }
            lsp::SignatureHelpEvent::RequestComplete { request, open } => {
                if self.cancel.is_none()
                    && !matches!(
                        self.in_flight,
                        Some(SignatureDelivery::Completion(current)) if current == request
                    )
                    && self
                        .completion
                        .is_none_or(|(current, _, _)| request >= current)
                {
                    self.completion = Some((request, sequence, open));
                }
            }
        }
    }

    fn begin_delivery(&mut self) -> Option<(Self::Event, Self::Delivery)> {
        if self.in_flight.is_some() {
            return None;
        }

        let cancel_sequence = self.cancel.unwrap_or(u64::MAX);
        let intent_sequence = self
            .intent
            .as_ref()
            .map_or(u64::MAX, |(sequence, _)| *sequence);
        let completion_sequence = self
            .completion
            .map_or(u64::MAX, |(_, sequence, _)| sequence);

        if cancel_sequence <= intent_sequence && cancel_sequence <= completion_sequence {
            self.cancel = None;
            self.in_flight = Some(SignatureDelivery::Cancel);
            return Some((lsp::SignatureHelpEvent::Cancel, SignatureDelivery::Cancel));
        }
        if intent_sequence <= completion_sequence {
            let (_, intent) = self.intent.take()?;
            self.in_flight = Some(SignatureDelivery::Intent);
            return Some((intent.into_event(), SignatureDelivery::Intent));
        }

        let (request, _, open) = self.completion.take()?;
        self.in_flight = Some(SignatureDelivery::Completion(request));
        Some((
            lsp::SignatureHelpEvent::RequestComplete { request, open },
            SignatureDelivery::Completion(request),
        ))
    }

    fn finish_delivery(&mut self, _delivery: Self::Delivery) {
        self.in_flight = None;
    }

    fn clear(&mut self) {
        self.cancel = None;
        self.intent = None;
        self.completion = None;
        self.in_flight = None;
    }
}

#[derive(Clone)]
pub struct SignatureHelpEvents(EventRelay<SignatureHelpEventState>);

impl SignatureHelpEvents {
    pub fn spawn(work: Work, destination: Sender<lsp::SignatureHelpEvent>) -> Self {
        Self(EventRelay::spawn(work, destination))
    }

    fn disconnected(destination: Sender<lsp::SignatureHelpEvent>) -> Self {
        Self(EventRelay::disconnected(destination))
    }

    pub fn send(&self, event: lsp::SignatureHelpEvent) {
        self.0.send(event);
    }
}

pub struct Handlers {
    // only public because most of the actual implementation is in helix-term right now :/
    pub completions: CompletionHandler,
    pub signature_hints: SignatureHelpEvents,
    pub auto_save: AutoSaveEvents,
    pub auto_reload: AutoReloadEvents,
    pub pkg: PkgEvents,
    pub document_colors: DocumentColorsEvents,
    pub lsp_feature_refresh: LspFeatureRefreshEvents,
    pub selection_ranges: Sender<lsp::SelectionRangeResponse>,
    pub blame: BlameEvents,
    pub navigation: Sender<NavigationRequest>,
    pub word_index: word_index::Handler,
}

impl Handlers {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runtime: &Runtime,
        completions: CompletionHandler,
        signature_hints: Sender<lsp::SignatureHelpEvent>,
        auto_save: Sender<AutoSaveEvent>,
        auto_reload: Sender<AutoReloadEvent>,
        pkg: Sender<PkgEvent>,
        document_colors: Sender<lsp::DocumentColorsEvent>,
        lsp_feature_refresh: Sender<lsp::LspFeatureRefreshEvent>,
        selection_ranges: Sender<lsp::SelectionRangeResponse>,
        blame: Sender<BlameEvent>,
        navigation: Sender<NavigationRequest>,
        word_index: word_index::Handler,
    ) -> Self {
        let work = runtime.work().clone();
        Self {
            completions,
            signature_hints: SignatureHelpEvents::spawn(work.clone(), signature_hints),
            auto_save: AutoSaveEvents::spawn(work.clone(), auto_save),
            auto_reload: AutoReloadEvents::spawn(work.clone(), auto_reload),
            pkg: PkgEvents::spawn(work.clone(), pkg),
            document_colors: DocumentColorsEvents::spawn(work.clone(), document_colors),
            lsp_feature_refresh: LspFeatureRefreshEvents::spawn(work.clone(), lsp_feature_refresh),
            selection_ranges,
            blame: BlameEvents::spawn(work, blame),
            navigation,
            word_index,
        }
    }

    /// Create a dummy `Handlers` for headless testing.
    ///
    /// All senders point to immediately-dropped receivers, so any send will
    /// fail silently.  This is fine for tests that don't exercise async
    /// handler behaviour.
    pub fn dummy() -> Self {
        let (comp_tx, _) = channel(1);
        let (sig_tx, _) = channel(1);
        let (auto_save_tx, _) = channel(1);
        let (auto_reload_tx, _) = channel(1);
        let (pkg_tx, _) = channel(1);
        let (doc_colors_tx, _) = channel(1);
        let (lsp_feature_refresh_tx, _) = channel(1);
        let (selection_ranges_tx, _) = channel(1);
        let (blame_tx, _) = channel(1);
        let (navigation_tx, _) = channel(1);
        Self {
            completions: CompletionHandler::disconnected(comp_tx),
            signature_hints: SignatureHelpEvents::disconnected(sig_tx),
            auto_save: AutoSaveEvents::disconnected(auto_save_tx),
            auto_reload: AutoReloadEvents::disconnected(auto_reload_tx),
            pkg: PkgEvents::disconnected(pkg_tx),
            document_colors: DocumentColorsEvents::disconnected(doc_colors_tx),
            lsp_feature_refresh: LspFeatureRefreshEvents::disconnected(lsp_feature_refresh_tx),
            selection_ranges: selection_ranges_tx,
            blame: BlameEvents::disconnected(blame_tx),
            navigation: navigation_tx,
            word_index: word_index::Handler::dummy(),
        }
    }

    /// Manually trigger completion (c-x)
    pub fn trigger_completions(&self, trigger_pos: usize, doc: DocumentId, view: ViewId) {
        self.completions.event(CompletionEvent::ManualTrigger {
            cursor: trigger_pos,
            doc,
            view,
        });
    }

    pub fn trigger_signature_help(&self, invocation: SignatureHelpInvoked, editor: &Editor) {
        let event = match invocation {
            SignatureHelpInvoked::Automatic => {
                if !editor.config().lsp.auto_signature_help {
                    return;
                }
                lsp::SignatureHelpEvent::Trigger
            }
            SignatureHelpInvoked::Manual => lsp::SignatureHelpEvent::Invoked,
        };
        self.signature_hints.send(event);
    }

    pub fn word_index(&self) -> &word_index::WordIndex {
        &self.word_index.index
    }
}

pub fn attach(editor: &Editor, handlers: &Handlers) {
    lsp::attach(editor, handlers);
    word_index::attach(editor, handlers);
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_runtime::test::RuntimeTest;

    #[test]
    fn package_events_coalesce_duplicates_while_destination_is_saturated() {
        fn missing(server: &str) -> PkgEvent {
            PkgEvent::MissingLanguageServer {
                documents: BTreeSet::from([DocumentId::default()]),
                server: server.into(),
                language: "test".into(),
                command: server.into(),
                config: helix_pkg::PkgConfig {
                    auto_install: true,
                    ..Default::default()
                },
                config_generation: 1,
                runtime_generation: 1,
            }
        }

        let rt = RuntimeTest::default();
        let (tx, mut rx) = channel(1);
        tx.try_send(missing("occupied")).unwrap();
        let events = PkgEvents::spawn(rt.runtime().work().clone(), tx);

        for _ in 0..10_000 {
            events.send(missing("rust-analyzer"));
        }
        events.send(missing("typescript-language-server"));

        rt.block_on(async {
            assert_eq!(rx.recv().await, Some(missing("occupied")));
            assert_eq!(rx.recv().await, Some(missing("rust-analyzer")));
            assert_eq!(rx.recv().await, Some(missing("typescript-language-server")));
        });
        assert!(matches!(
            rx.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
    }

    #[test]
    fn package_state_replaces_generations_under_stable_identity() {
        let first = DocumentId::default();
        let second = DocumentId::new(std::num::NonZeroUsize::new(2).unwrap());
        let event = |document, generation| PkgEvent::MissingLanguageServer {
            documents: BTreeSet::from([document]),
            server: "rust-analyzer".into(),
            language: "rust".into(),
            command: "rust-analyzer".into(),
            config: helix_pkg::PkgConfig::default(),
            config_generation: generation,
            runtime_generation: generation,
        };
        let mut state = PkgEventState::default();
        state.push(event(first, 1));
        state.push(event(second, 2));

        assert_eq!(state.pending.len(), 1);
        let (event, _) = state.begin_delivery().expect("coalesced package state");
        let PkgEvent::MissingLanguageServer {
            documents,
            config_generation,
            runtime_generation,
            ..
        } = event;
        assert_eq!(documents, BTreeSet::from([first, second]));
        assert_eq!(config_generation, 2);
        assert_eq!(runtime_generation, 2);
    }

    #[test]
    fn auto_save_state_keeps_latest_change_and_orders_insert_exit() {
        let mut state = AutoSaveEventState::default();
        state.push(AutoSaveEvent::DocumentChanged { save_after: 10 });
        state.push(AutoSaveEvent::DocumentChanged { save_after: 20 });
        state.push(AutoSaveEvent::LeftInsertMode);

        let (event, delivery) = state.begin_delivery().expect("document change");
        assert!(matches!(
            event,
            AutoSaveEvent::DocumentChanged { save_after: 20 }
        ));
        state.finish_delivery(delivery);
        let (event, _) = state.begin_delivery().expect("insert-mode exit");
        assert!(matches!(event, AutoSaveEvent::LeftInsertMode));
    }

    #[test]
    fn auto_reload_state_coalesces_file_bursts_without_losing_insert_exit() {
        let mut state = AutoReloadEventState::default();
        let first = DocumentId::default();
        let second = DocumentId::new(std::num::NonZeroUsize::new(2).unwrap());
        state.push(AutoReloadEvent::DocumentsChanged {
            doc_ids: vec![first],
        });
        state.push(AutoReloadEvent::DocumentsChanged {
            doc_ids: vec![second],
        });
        state.push(AutoReloadEvent::LeftInsertMode);

        let (event, delivery) = state.begin_delivery().expect("file change");
        let AutoReloadEvent::DocumentsChanged { doc_ids } = event else {
            panic!("expected coalesced file change");
        };
        assert_eq!(doc_ids, vec![first, second]);
        state.finish_delivery(delivery);
        let (event, _) = state.begin_delivery().expect("insert-mode exit");
        assert!(matches!(event, AutoReloadEvent::LeftInsertMode));
    }

    #[test]
    fn signature_help_keeps_only_the_newest_completed_request() {
        let mut state = SignatureHelpEventState::default();
        let older = lsp::SignatureHelpRequestId::new(std::num::NonZeroU64::new(1).unwrap());
        let newer = lsp::SignatureHelpRequestId::new(std::num::NonZeroU64::new(2).unwrap());
        state.push(lsp::SignatureHelpEvent::RequestComplete {
            request: newer,
            open: true,
        });
        state.push(lsp::SignatureHelpEvent::RequestComplete {
            request: older,
            open: false,
        });

        let (event, _) = state.begin_delivery().expect("latest completion");
        assert!(matches!(
            event,
            lsp::SignatureHelpEvent::RequestComplete {
                request,
                open: true,
            } if request == newer
        ));
        assert!(state.completion.is_none());
    }

    #[test]
    fn document_color_state_coalesces_repeated_document_changes() {
        let document_id = DocumentId::default();
        let mut state = DocumentColorsEventState::default();
        state.push(lsp::DocumentColorsEvent(document_id));
        state.push(lsp::DocumentColorsEvent(document_id));

        let (lsp::DocumentColorsEvent(received), delivery) =
            state.begin_delivery().expect("document color refresh");
        assert_eq!(received, document_id);
        state.finish_delivery(delivery);
        assert!(state.begin_delivery().is_none());
    }

    #[test]
    fn lsp_feature_state_keeps_one_refresh_per_document_and_kind() {
        let document_id = DocumentId::default();
        let kinds = [
            lsp::LspFeatureRefreshKind::CodeLens,
            lsp::LspFeatureRefreshKind::DocumentLinks,
            lsp::LspFeatureRefreshKind::FoldingRanges,
            lsp::LspFeatureRefreshKind::SemanticTokens,
            lsp::LspFeatureRefreshKind::InlineCompletion,
        ];
        let mut state = LspFeatureRefreshEventState::default();
        for _ in 0..10_000 {
            for kind in kinds {
                state.push(lsp::LspFeatureRefreshEvent {
                    doc_id: document_id,
                    kind,
                });
            }
        }

        let mut delivered = HashSet::new();
        while let Some((event, delivery)) = state.begin_delivery() {
            assert_eq!(event.doc_id, document_id);
            delivered.insert(event.kind);
            state.finish_delivery(delivery);
        }
        assert_eq!(delivered, HashSet::from(kinds));
    }

    #[test]
    fn blame_events_keep_explicit_line_request_under_saturation() {
        let rt = RuntimeTest::default();
        let (tx, mut rx) = channel(1);
        let occupied_doc = DocumentId::default();
        tx.try_send(BlameEvent {
            path: "occupied".into(),
            doc_id: occupied_doc,
            line: None,
        })
        .unwrap();
        let events = BlameEvents::spawn(rt.runtime().work().clone(), tx);
        let doc_id = DocumentId::default();

        events.send(BlameEvent {
            path: "automatic-before".into(),
            doc_id,
            line: None,
        });
        events.send(BlameEvent {
            path: "manual".into(),
            doc_id,
            line: Some(17),
        });
        events.send(BlameEvent {
            path: "automatic-after".into(),
            doc_id,
            line: None,
        });

        rt.block_on(async {
            rx.recv().await.expect("occupied event");
            let event = rx.recv().await.expect("coalesced blame event");
            assert_eq!(event.path, std::path::PathBuf::from("manual"));
            assert_eq!(event.line, Some(17));
        });
        assert!(matches!(
            rx.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
    }

    #[test]
    fn signature_cancel_is_ordered_before_latest_manual_intent() {
        let rt = RuntimeTest::default();
        let (tx, mut rx) = channel(1);
        assert!(tx.try_send(lsp::SignatureHelpEvent::Trigger).is_ok());
        let events = SignatureHelpEvents::spawn(rt.runtime().work().clone(), tx);

        for _ in 0..10_000 {
            events.send(lsp::SignatureHelpEvent::ReTrigger);
        }
        events.send(lsp::SignatureHelpEvent::Cancel);
        events.send(lsp::SignatureHelpEvent::Trigger);
        events.send(lsp::SignatureHelpEvent::Invoked);
        events.send(lsp::SignatureHelpEvent::Trigger);

        rt.block_on(async {
            assert!(matches!(
                rx.recv().await,
                Some(lsp::SignatureHelpEvent::Trigger)
            ));
            assert!(matches!(
                rx.recv().await,
                Some(lsp::SignatureHelpEvent::Cancel)
            ));
            assert!(matches!(
                rx.recv().await,
                Some(lsp::SignatureHelpEvent::Invoked)
            ));
        });
        assert!(matches!(
            rx.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
    }
}
