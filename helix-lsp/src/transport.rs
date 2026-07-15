use crate::{
    jsonrpc,
    lsp::{self, notification::Notification as _},
    Error, LanguageServerId, Notification, Result, ServerEvent,
};
use anyhow::Context;
use helix_core::Rope;
use helix_runtime::{channel, Receiver, Sender};
use log::{error, info};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
        BufWriter,
    },
    sync::{oneshot, watch, Notify},
};

pub(crate) struct DeferredNotification {
    method: &'static str,
    serialize: Box<dyn FnOnce() -> serde_json::Result<String> + Send>,
}

pub(crate) struct DeferredRequest {
    id: jsonrpc::Id,
    method: &'static str,
    serialize: Box<dyn FnOnce() -> serde_json::Result<String> + Send>,
}

#[derive(Debug)]
pub(crate) struct DocumentChange {
    pub text_document: lsp::VersionedTextDocumentIdentifier,
    pub old_text: Rope,
    pub new_text: Rope,
    pub sync_kind: lsp::TextDocumentSyncKind,
    pub offset_encoding: crate::OffsetEncoding,
}

impl DocumentChange {
    fn serialize(self) -> Result<String> {
        let content_changes = match self.sync_kind {
            lsp::TextDocumentSyncKind::FULL => vec![lsp::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: self.new_text.to_string(),
            }],
            lsp::TextDocumentSyncKind::INCREMENTAL => {
                let changes = helix_core::diff::compare_ropes(&self.old_text, &self.new_text);
                crate::Client::changeset_to_changes(
                    &self.old_text,
                    &self.new_text,
                    changes.changes(),
                    self.offset_encoding,
                )
            }
            lsp::TextDocumentSyncKind::NONE => return Ok(String::new()),
            kind => {
                return Err(Error::Other(anyhow::anyhow!(
                    "unsupported sync kind {kind:?}"
                )))
            }
        };
        let notification = jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: lsp::notification::DidChangeTextDocument::METHOD.to_owned(),
            params: jsonrpc::Params::Map(
                serde_json::to_value(lsp::DidChangeTextDocumentParams {
                    text_document: self.text_document,
                    content_changes,
                })?
                .as_object()
                .cloned()
                .context("didChange params must serialize to an object")?,
            ),
        };
        Ok(serde_json::to_string(&notification)?)
    }
}

#[derive(Debug)]
pub(crate) enum DocumentUpdate {
    Open {
        uri: lsp::Url,
        version: i32,
        text: Rope,
        language_id: String,
    },
    Change(DocumentChangeTarget),
    Save {
        uri: lsp::Url,
        text: Rope,
        include_text: bool,
    },
    Close {
        uri: lsp::Url,
    },
}

#[derive(Debug)]
pub(crate) struct DocumentChangeTarget {
    pub text_document: lsp::VersionedTextDocumentIdentifier,
    pub new_text: Rope,
    pub sync_kind: lsp::TextDocumentSyncKind,
    pub offset_encoding: crate::OffsetEncoding,
}

#[derive(Clone, Debug)]
struct SavedDocument {
    sequence: u64,
    version: i32,
    text: Rope,
    include_text: bool,
}

#[derive(Clone, Debug)]
struct DocumentTarget {
    uri: lsp::Url,
    lifecycle: u64,
    open: bool,
    version: i32,
    text: Rope,
    language_id: String,
    sync_kind: lsp::TextDocumentSyncKind,
    offset_encoding: crate::OffsetEncoding,
    save: Option<SavedDocument>,
}

#[derive(Debug, Default)]
pub(crate) struct DocumentBatch {
    order: VecDeque<lsp::Url>,
    targets: HashMap<lsp::Url, DocumentTarget>,
}

impl DocumentBatch {
    fn replace(&mut self, target: DocumentTarget) {
        let uri = target.uri.clone();
        if !self.targets.contains_key(&uri) {
            self.order.push_back(uri.clone());
        }
        self.targets.insert(uri, target);
    }

    fn pop_front(&mut self) -> Option<DocumentTarget> {
        while let Some(uri) = self.order.pop_front() {
            if let Some(target) = self.targets.remove(&uri) {
                return Some(target);
            }
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

#[derive(Clone, Debug)]
struct WrittenDocument {
    lifecycle: u64,
    version: i32,
    text: Rope,
    save_sequence: u64,
}

#[derive(Debug)]
pub(crate) enum DocumentWire {
    Open {
        uri: lsp::Url,
        version: i32,
        text: Rope,
        language_id: String,
    },
    Change(DocumentChange),
    Save {
        uri: lsp::Url,
        text: Option<Rope>,
    },
    Close {
        uri: lsp::Url,
    },
}

impl DocumentWire {
    fn method(&self) -> &'static str {
        match self {
            Self::Open { .. } => lsp::notification::DidOpenTextDocument::METHOD,
            Self::Change(_) => lsp::notification::DidChangeTextDocument::METHOD,
            Self::Save { .. } => lsp::notification::DidSaveTextDocument::METHOD,
            Self::Close { .. } => lsp::notification::DidCloseTextDocument::METHOD,
        }
    }

    fn serialize(self) -> Result<String> {
        match self {
            Self::Change(change) => change.serialize(),
            Self::Open {
                uri,
                version,
                text,
                language_id,
            } => serialize_notification::<lsp::notification::DidOpenTextDocument>(
                lsp::DidOpenTextDocumentParams {
                    text_document: lsp::TextDocumentItem {
                        uri,
                        language_id,
                        version,
                        text: text.to_string(),
                    },
                },
            ),
            Self::Save { uri, text } => serialize_notification::<
                lsp::notification::DidSaveTextDocument,
            >(lsp::DidSaveTextDocumentParams {
                text_document: lsp::TextDocumentIdentifier { uri },
                text: text.map(|text| text.to_string()),
            }),
            Self::Close { uri } => {
                serialize_notification::<lsp::notification::DidCloseTextDocument>(
                    lsp::DidCloseTextDocumentParams {
                        text_document: lsp::TextDocumentIdentifier { uri },
                    },
                )
            }
        }
    }
}

fn serialize_notification<N>(params: N::Params) -> Result<String>
where
    N: lsp::notification::Notification,
    N::Params: serde::Serialize,
{
    let params = serde_json::to_value(params)?;
    let notification = jsonrpc::Notification {
        jsonrpc: Some(jsonrpc::Version::V2),
        method: N::METHOD.to_owned(),
        params: match params {
            Value::Null => jsonrpc::Params::None,
            Value::Array(values) => jsonrpc::Params::Array(values),
            Value::Object(values) => jsonrpc::Params::Map(values),
            _ => {
                return Err(Error::Other(anyhow::anyhow!(
                    "notification params must be structured"
                )))
            }
        },
    };
    Ok(serde_json::to_string(&notification)?)
}

impl fmt::Debug for DeferredNotification {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeferredNotification")
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for DeferredRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeferredRequest")
            .field("id", &self.id)
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) enum Payload {
    Request { value: jsonrpc::MethodCall },
    DeferredRequest(DeferredRequest),
    CancelRequest { id: jsonrpc::Id },
    Notification(jsonrpc::Notification),
    DeferredNotification(DeferredNotification),
    DocumentUpdate(DocumentUpdate),
    DocumentBatch(DocumentBatch),
    DocumentWire(DocumentWire),
    Response(jsonrpc::Output),
}

impl Payload {
    pub(crate) fn deferred_notification(
        method: &'static str,
        serialize: impl FnOnce() -> serde_json::Result<String> + Send + 'static,
    ) -> Self {
        Self::DeferredNotification(DeferredNotification {
            method,
            serialize: Box::new(serialize),
        })
    }

    pub(crate) fn deferred_request(
        id: jsonrpc::Id,
        method: &'static str,
        serialize: impl FnOnce() -> serde_json::Result<String> + Send + 'static,
    ) -> Self {
        Self::DeferredRequest(DeferredRequest {
            id,
            method,
            serialize: Box::new(serialize),
        })
    }
}

type PendingSender = oneshot::Sender<Result<Value>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestWireState {
    Queued,
    Sent,
}

#[derive(Debug)]
struct PendingRequest {
    sender: PendingSender,
    wire_state: RequestWireState,
}

#[derive(Debug, Default)]
struct PendingState {
    requests: HashMap<jsonrpc::Id, PendingRequest>,
    closed: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PendingRegistry {
    state: Arc<Mutex<PendingState>>,
}

impl PendingRegistry {
    fn register(&self, id: jsonrpc::Id, sender: PendingSender) -> Result<()> {
        let mut state = self.state.lock();
        if state.closed {
            return Err(Error::StreamClosed);
        }

        match state.requests.entry(id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(PendingRequest {
                    sender,
                    wire_state: RequestWireState::Queued,
                });
                Ok(())
            }
            Entry::Occupied(_) => Err(Error::Other(anyhow::anyhow!(
                "duplicate language-server request id {id:?}"
            ))),
        }
    }

    fn remove(&self, id: &jsonrpc::Id) -> Option<PendingRequest> {
        self.state.lock().requests.remove(id)
    }

    fn mark_sent(&self, id: &jsonrpc::Id) -> bool {
        let mut state = self.state.lock();
        let Some(request) = state.requests.get_mut(id) else {
            return false;
        };
        request.wire_state = RequestWireState::Sent;
        true
    }

    fn remove_sent(&self, id: &jsonrpc::Id) -> Option<PendingSender> {
        let mut state = self.state.lock();
        if !matches!(
            state.requests.get(id),
            Some(PendingRequest {
                wire_state: RequestWireState::Sent,
                ..
            })
        ) {
            return None;
        }
        state.requests.remove(id).map(|request| request.sender)
    }

    /// Atomically prevents new requests and removes every existing request.
    fn close(&self) -> bool {
        let requests = {
            let mut state = self.state.lock();
            if state.closed {
                return false;
            }
            state.closed = true;
            std::mem::take(&mut state.requests)
        };

        for (id, request) in requests {
            if request.sender.send(Err(Error::StreamClosed)).is_err() {
                log::trace!(
                    "Request receiver already closed while transport was shutting down (id={id:?})"
                );
            }
        }
        true
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.state.lock().requests.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InitializationState {
    Pending,
    Initialized,
    Failed(Arc<str>),
    Closed,
}

#[derive(Clone, Debug)]
pub(crate) struct Initialization {
    state_tx: watch::Sender<InitializationState>,
}

impl Initialization {
    fn new() -> Self {
        let (state_tx, _) = watch::channel(InitializationState::Pending);
        Self { state_tx }
    }

    fn subscribe(&self) -> watch::Receiver<InitializationState> {
        self.state_tx.subscribe()
    }

    fn initialized(&self) -> bool {
        self.state_tx.send_if_modified(|state| {
            if matches!(state, InitializationState::Pending) {
                *state = InitializationState::Initialized;
                true
            } else {
                false
            }
        })
    }

    fn current(&self) -> InitializationState {
        self.state_tx.borrow().clone()
    }

    fn fail(&self, message: impl Into<Arc<str>>) {
        let message = message.into();
        self.state_tx.send_if_modified(move |state| {
            if matches!(state, InitializationState::Pending) {
                *state = InitializationState::Failed(message.clone());
                true
            } else {
                false
            }
        });
    }

    fn close(&self) {
        self.state_tx.send_if_modified(|state| {
            if matches!(
                state,
                InitializationState::Pending | InitializationState::Initialized
            ) {
                *state = InitializationState::Closed;
                true
            } else {
                false
            }
        });
    }

    pub(crate) async fn wait(&self) -> InitializationState {
        let mut state_rx = self.subscribe();
        loop {
            let state = state_rx.borrow().clone();
            if !matches!(state, InitializationState::Pending) {
                return state;
            }
            if state_rx.changed().await.is_err() {
                return InitializationState::Closed;
            }
        }
    }
}

#[derive(Debug)]
enum ControlMessage {
    CancelRequest {
        id: jsonrpc::Id,
    },
    Response {
        output: jsonrpc::Output,
        sent: Option<oneshot::Sender<()>>,
    },
    Initialized {
        notification: jsonrpc::Notification,
        sent: oneshot::Sender<()>,
    },
}

const DEFAULT_OUTBOUND_PRIORITY_CAPACITY: usize = 64;
const DEFAULT_OUTBOUND_CAPACITY: usize = 1024;
const OUTBOUND_PRIORITY_BURST: usize = 8;
const DEFAULT_INBOUND_CAPACITY: usize = 256;
const DEFAULT_CLIENT_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug)]
struct TransportCapacities {
    outbound: usize,
    priority: usize,
    inbound: usize,
    client: usize,
}

const DEFAULT_TRANSPORT_CAPACITIES: TransportCapacities = TransportCapacities {
    outbound: DEFAULT_OUTBOUND_CAPACITY,
    priority: DEFAULT_OUTBOUND_PRIORITY_CAPACITY,
    inbound: DEFAULT_INBOUND_CAPACITY,
    client: DEFAULT_CLIENT_CAPACITY,
};

#[derive(Debug)]
enum OutboundMessage {
    Payload(Payload),
    Control(ControlMessage),
}

#[derive(Clone, Copy, Debug)]
enum NormalReceiveMode {
    Any,
    InitializeOnly,
    None,
}

fn payload_is_initialize(payload: &Payload) -> bool {
    use lsp::request::{Initialize, Request};
    matches!(
        payload,
        Payload::Request {
            value: jsonrpc::MethodCall { method, .. },
        } if method == Initialize::METHOD
    ) || matches!(
        payload,
        Payload::DeferredRequest(DeferredRequest { method, .. })
            if *method == Initialize::METHOD
    )
}

#[derive(Debug, Default)]
struct OutboundState {
    normal: VecDeque<Payload>,
    priority: VecDeque<ControlMessage>,
    desired_documents: HashMap<lsp::Url, DocumentTarget>,
    written_documents: HashMap<lsp::Url, WrittenDocument>,
    document_sequence: u64,
    closed: bool,
}

impl OutboundState {
    fn next_document_sequence(&mut self) -> u64 {
        self.document_sequence = self.document_sequence.wrapping_add(1).max(1);
        self.document_sequence
    }

    fn enqueue_document_update(&mut self, update: DocumentUpdate) {
        let target = match update {
            DocumentUpdate::Open {
                uri,
                version,
                text,
                language_id,
            } => {
                let lifecycle = self.next_document_sequence();
                let target = DocumentTarget {
                    uri: uri.clone(),
                    lifecycle,
                    open: true,
                    version,
                    text,
                    language_id,
                    sync_kind: lsp::TextDocumentSyncKind::NONE,
                    offset_encoding: crate::OffsetEncoding::default(),
                    save: None,
                };
                self.desired_documents.insert(uri, target.clone());
                target
            }
            DocumentUpdate::Change(change) => {
                let Some(target) = self.desired_documents.get_mut(&change.text_document.uri) else {
                    log::error!(
                        "discarding didChange for document that is not open: {}",
                        change.text_document.uri
                    );
                    return;
                };
                if change.text_document.version < target.version {
                    log::debug!(
                        "discarding stale didChange uri={} version={} desired_version={}",
                        change.text_document.uri,
                        change.text_document.version,
                        target.version,
                    );
                    return;
                }
                target.version = change.text_document.version;
                target.text = change.new_text;
                target.sync_kind = change.sync_kind;
                target.offset_encoding = change.offset_encoding;
                target.clone()
            }
            DocumentUpdate::Save {
                uri,
                text,
                include_text,
            } => {
                let sequence = self.next_document_sequence();
                let Some(target) = self.desired_documents.get_mut(&uri) else {
                    log::debug!("discarding didSave for document that is not open: {uri}");
                    return;
                };
                target.save = Some(SavedDocument {
                    sequence,
                    version: target.version,
                    text,
                    include_text,
                });
                target.clone()
            }
            DocumentUpdate::Close { uri } => {
                let Some(mut target) = self.desired_documents.remove(&uri) else {
                    log::debug!("discarding duplicate didClose for document: {uri}");
                    return;
                };
                target.open = false;
                target
            }
        };

        match self.normal.back_mut() {
            Some(Payload::DocumentBatch(batch)) => batch.replace(target),
            _ => {
                let mut batch = DocumentBatch::default();
                batch.replace(target);
                self.normal.push_back(Payload::DocumentBatch(batch));
            }
        }
    }

    fn document_wires(&mut self, target: DocumentTarget) -> VecDeque<DocumentWire> {
        let mut wires = VecDeque::new();
        let mut written = self.written_documents.remove(&target.uri);
        let save = target.save.as_ref().filter(|save| {
            written
                .as_ref()
                .is_none_or(|written| save.sequence > written.save_sequence)
        });

        if written.is_none() && !target.open && save.is_none() {
            return wires;
        }

        let initial_version = save.map_or(target.version, |save| save.version);
        let initial_text = save.map_or_else(|| target.text.clone(), |save| save.text.clone());
        let must_reopen = written
            .as_ref()
            .is_some_and(|written| written.lifecycle != target.lifecycle);

        if must_reopen {
            wires.push_back(DocumentWire::Close {
                uri: target.uri.clone(),
            });
            written = None;
        }

        if written.is_none() {
            wires.push_back(DocumentWire::Open {
                uri: target.uri.clone(),
                version: initial_version,
                text: initial_text.clone(),
                language_id: target.language_id.clone(),
            });
            written = Some(WrittenDocument {
                lifecycle: target.lifecycle,
                version: initial_version,
                text: initial_text,
                save_sequence: 0,
            });
        }

        let written = written
            .as_mut()
            .expect("document must be open before synchronization");
        if let Some(save) = save {
            Self::push_change_if_needed(&mut wires, written, &target, save.version, &save.text);
            wires.push_back(DocumentWire::Save {
                uri: target.uri.clone(),
                text: save.include_text.then(|| save.text.clone()),
            });
            written.save_sequence = save.sequence;
        }

        Self::push_change_if_needed(&mut wires, written, &target, target.version, &target.text);

        if target.open {
            self.written_documents.insert(target.uri, written.clone());
        } else {
            wires.push_back(DocumentWire::Close { uri: target.uri });
        }
        wires
    }

    fn push_change_if_needed(
        wires: &mut VecDeque<DocumentWire>,
        written: &mut WrittenDocument,
        target: &DocumentTarget,
        version: i32,
        text: &Rope,
    ) {
        if written.version == version && &written.text == text {
            return;
        }
        if version < written.version {
            log::warn!(
                "refusing to move language-server document backwards uri={} written_version={} target_version={}",
                target.uri,
                written.version,
                version,
            );
            return;
        }
        wires.push_back(DocumentWire::Change(DocumentChange {
            text_document: lsp::VersionedTextDocumentIdentifier {
                uri: target.uri.clone(),
                version,
            },
            old_text: written.text.clone(),
            new_text: text.clone(),
            sync_kind: target.sync_kind,
            offset_encoding: target.offset_encoding,
        }));
        written.version = version;
        written.text = text.clone();
    }

    fn pop_normal(&mut self, mode: NormalReceiveMode) -> Option<Payload> {
        if matches!(mode, NormalReceiveMode::InitializeOnly) {
            return self
                .normal
                .iter()
                .position(payload_is_initialize)
                .and_then(|index| self.normal.remove(index));
        }
        if matches!(mode, NormalReceiveMode::None) {
            return None;
        }

        loop {
            let payload = self.normal.pop_front()?;
            let Payload::DocumentBatch(mut batch) = payload else {
                return Some(payload);
            };
            let Some(target) = batch.pop_front() else {
                continue;
            };
            if !batch.is_empty() {
                self.normal.push_front(Payload::DocumentBatch(batch));
            }
            let wires = self.document_wires(target);
            for wire in wires.into_iter().rev() {
                self.normal.push_front(Payload::DocumentWire(wire));
            }
        }
    }
}

/// A single-owner outbound mailbox.
///
/// Normal messages are kept in one FIFO so document notifications cannot be
/// overtaken by dependent requests. Document updates fold into reconstructable
/// per-URI targets between FIFO barriers; the writer derives protocol messages
/// from the last written state to the latest desired state.
/// The priority lane is bounded; asynchronous producers wait for capacity while
/// synchronous producers receive an explicit saturation error. The writer
/// services normal traffic after a bounded priority burst.
#[derive(Clone, Debug)]
struct OutboundMailbox {
    state: Arc<Mutex<OutboundState>>,
    ready: Arc<Notify>,
    space_ready: Arc<Notify>,
    normal_capacity: usize,
    priority_capacity: usize,
}

impl OutboundMailbox {
    fn new(normal_capacity: usize, priority_capacity: usize) -> Self {
        Self {
            state: Arc::default(),
            ready: Arc::default(),
            space_ready: Arc::default(),
            normal_capacity: normal_capacity.max(1),
            priority_capacity: priority_capacity.max(1),
        }
    }

    fn send_payload(&self, payload: Payload) -> Result<()> {
        let mut state = self.state.lock();
        if state.closed {
            return Err(Error::StreamClosed);
        }
        if let Payload::DocumentUpdate(update) = payload {
            state.enqueue_document_update(update);
            drop(state);
            self.ready.notify_one();
            return Ok(());
        }
        if state.normal.len() >= self.normal_capacity {
            return Err(Error::OutboundQueueFull);
        }
        state.normal.push_back(payload);
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    async fn send_payload_async(&self, payload: Payload) -> Result<()> {
        let mut payload = Some(payload);
        loop {
            let wait_for_space = self.space_ready.notified();
            tokio::pin!(wait_for_space);
            wait_for_space.as_mut().enable();
            let mut state = self.state.lock();
            if state.closed {
                return Err(Error::StreamClosed);
            }
            let current = payload.take().expect("outbound payload disappeared");
            if let Payload::DocumentUpdate(update) = current {
                state.enqueue_document_update(update);
                drop(state);
                self.ready.notify_one();
                return Ok(());
            }
            if state.normal.len() < self.normal_capacity {
                state.normal.push_back(current);
                drop(state);
                self.ready.notify_one();
                return Ok(());
            }
            if payload_is_initialize(&current) {
                state.normal.push_back(current);
                drop(state);
                self.ready.notify_one();
                return Ok(());
            }
            payload = Some(current);
            drop(state);
            wait_for_space.await;
        }
    }

    fn send_control(&self, control: ControlMessage) -> Result<()> {
        let mut control = Some(control);
        let mut state = self.state.lock();
        if state.closed {
            return Err(Error::StreamClosed);
        }
        if !Self::enqueue_control(&mut state, self.priority_capacity, &mut control) {
            return Err(Error::OutboundControlQueueFull);
        }
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    async fn send_control_async(&self, control: ControlMessage) -> Result<()> {
        let mut control = Some(control);
        loop {
            let wait_for_space = self.space_ready.notified();
            tokio::pin!(wait_for_space);
            wait_for_space.as_mut().enable();
            let enqueued = {
                let mut state = self.state.lock();
                if state.closed {
                    return Err(Error::StreamClosed);
                }
                Self::enqueue_control(&mut state, self.priority_capacity, &mut control)
            };
            if enqueued {
                self.ready.notify_one();
                return Ok(());
            }
            wait_for_space.await;
        }
    }

    fn enqueue_control(
        state: &mut OutboundState,
        capacity: usize,
        control: &mut Option<ControlMessage>,
    ) -> bool {
        if state.priority.len() < capacity {
            state
                .priority
                .push_back(control.take().expect("control message must be present"));
            return true;
        }
        if matches!(control, Some(ControlMessage::CancelRequest { .. })) {
            return false;
        }
        let Some(index) = state
            .priority
            .iter()
            .position(|queued| matches!(queued, ControlMessage::CancelRequest { .. }))
        else {
            return false;
        };
        if let Some(ControlMessage::CancelRequest { id }) = state.priority.remove(index) {
            log::debug!(
                "Discarded queued {} for request {:?} to keep a language-server response responsive",
                lsp::notification::Cancel::METHOD,
                id,
            );
        }
        state
            .priority
            .push_back(control.take().expect("control message must be present"));
        true
    }

    async fn recv(
        &self,
        priority_streak: &mut usize,
        normal_mode: NormalReceiveMode,
    ) -> Option<OutboundMessage> {
        loop {
            let message = {
                let mut state = self.state.lock();
                if state.closed {
                    return None;
                }

                if !state.priority.is_empty()
                    && (*priority_streak < OUTBOUND_PRIORITY_BURST || state.normal.is_empty())
                {
                    *priority_streak = (*priority_streak).saturating_add(1);
                    state.priority.pop_front().map(OutboundMessage::Control)
                } else if !matches!(normal_mode, NormalReceiveMode::None) {
                    let payload = state.pop_normal(normal_mode);
                    let Some(payload) = payload else {
                        drop(state);
                        self.ready.notified().await;
                        continue;
                    };
                    *priority_streak = 0;
                    Some(OutboundMessage::Payload(payload))
                } else {
                    None
                }
            };

            if matches!(&message, Some(OutboundMessage::Control(_))) {
                self.space_ready.notify_one();
            }
            if matches!(&message, Some(OutboundMessage::Payload(_))) {
                self.space_ready.notify_waiters();
            }
            if message.is_some() {
                return message;
            }
            self.ready.notified().await;
        }
    }

    fn close(&self) {
        let mut state = self.state.lock();
        if state.closed {
            return;
        }
        state.closed = true;
        state.normal.clear();
        state.priority.clear();
        state.desired_documents.clear();
        state.written_documents.clear();
        drop(state);
        self.ready.notify_one();
        self.space_ready.notify_waiters();
    }

    #[cfg(test)]
    fn queue_lengths(&self) -> (usize, usize) {
        let state = self.state.lock();
        (state.normal.len(), state.priority.len())
    }
}

#[derive(Debug)]
enum InboundQueueItem {
    Reliable(ServerEvent),
    Diagnostics(String),
    Progress(lsp::ProgressToken),
}

#[derive(Debug)]
enum InboundEvent {
    Reliable(ServerEvent),
    Diagnostics(lsp::PublishDiagnosticsParams),
    Progress(lsp::ProgressParams),
}

impl From<ServerEvent> for InboundEvent {
    fn from(event: ServerEvent) -> Self {
        match event {
            ServerEvent::Notification(Notification::PublishDiagnostics(params)) => {
                Self::Diagnostics(params)
            }
            ServerEvent::Notification(Notification::ProgressMessage(params)) => {
                Self::Progress(params)
            }
            event => Self::Reliable(event),
        }
    }
}

#[derive(Debug, Default)]
struct InboundState {
    queue: VecDeque<InboundQueueItem>,
    diagnostics: HashMap<String, lsp::PublishDiagnosticsParams>,
    progress: HashMap<lsp::ProgressToken, VecDeque<lsp::WorkDoneProgress>>,
    initialized: bool,
    exited: bool,
    high_watermark: usize,
    coalesced_diagnostics: u64,
    coalesced_progress: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct InboundQueueProfile {
    diagnostics: usize,
    progress: usize,
    log_messages: usize,
    requests: usize,
    notifications: usize,
    invalid: usize,
}

fn inbound_queue_profile(state: &InboundState) -> InboundQueueProfile {
    let mut profile = InboundQueueProfile::default();
    for item in &state.queue {
        match item {
            InboundQueueItem::Diagnostics(_) => profile.diagnostics += 1,
            InboundQueueItem::Progress(_) => profile.progress += 1,
            InboundQueueItem::Reliable(ServerEvent::Request(_)) => profile.requests += 1,
            InboundQueueItem::Reliable(ServerEvent::Notification(Notification::LogMessage(_))) => {
                profile.log_messages += 1
            }
            InboundQueueItem::Reliable(ServerEvent::Notification(_)) => profile.notifications += 1,
            InboundQueueItem::Reliable(ServerEvent::Invalid { .. }) => profile.invalid += 1,
        }
    }
    profile
}

#[derive(Clone, Debug)]
struct InboundDispatcher {
    state: Arc<Mutex<InboundState>>,
    ready: Arc<Notify>,
    space_ready: Arc<Notify>,
    exit_ready: Arc<Notify>,
    capacity: usize,
    server_name: Arc<str>,
}

impl InboundDispatcher {
    fn start(
        id: LanguageServerId,
        server_name: Arc<str>,
        client_tx: Sender<(LanguageServerId, ServerEvent)>,
        capacity: usize,
    ) -> Self {
        let dispatcher = Self {
            state: Arc::default(),
            ready: Arc::default(),
            space_ready: Arc::default(),
            exit_ready: Arc::default(),
            capacity: capacity.max(1),
            server_name,
        };
        tokio::spawn(Self::run(
            id,
            client_tx,
            dispatcher.state.clone(),
            dispatcher.ready.clone(),
            dispatcher.space_ready.clone(),
            dispatcher.exit_ready.clone(),
        ));
        dispatcher
    }

    async fn send(&self, call: jsonrpc::Call) -> Result<()> {
        let Some(event) = ServerEvent::from_call(call) else {
            return Ok(());
        };
        let mut event = Some(InboundEvent::from(event));
        let mut reported_backpressure = false;

        loop {
            let space_wait = self.space_ready.notified();
            tokio::pin!(space_wait);
            space_wait.as_mut().enable();

            let mut state = self.state.lock();
            if state.exited {
                return Err(Error::StreamClosed);
            }

            if coalesce_inbound(&mut state, &mut event) {
                return Ok(());
            }

            if state.queue.len() < self.capacity {
                admit_inbound(&mut state, event.take().expect("inbound event disappeared"));
                let depth = state.queue.len();
                state.high_watermark = state.high_watermark.max(depth);
                drop(state);
                self.ready.notify_one();
                return Ok(());
            }

            if !reported_backpressure {
                let profile = inbound_queue_profile(&state);
                log::warn!(
                    "lsp_rpc direction=in server={} outcome=backpressure queued={} hard_capacity={} diagnostics_coalesced={} progress_coalesced={} diagnostics={} progress={} log_messages={} requests={} notifications={} invalid={}",
                    self.server_name,
                    state.queue.len(),
                    self.capacity,
                    state.coalesced_diagnostics,
                    state.coalesced_progress,
                    profile.diagnostics,
                    profile.progress,
                    profile.log_messages,
                    profile.requests,
                    profile.notifications,
                    profile.invalid,
                );
                reported_backpressure = true;
            }
            drop(state);
            space_wait.await;
        }
    }

    fn mark_initialized(&self) {
        let mut state = self.state.lock();
        if state.exited || state.initialized {
            return;
        }
        state.initialized = true;
        drop(state);
        self.ready.notify_one();
    }

    fn terminate(&self) {
        let mut state = self.state.lock();
        if state.exited {
            return;
        }
        state.exited = true;
        state.queue.clear();
        state.diagnostics.clear();
        state.progress.clear();
        drop(state);
        self.ready.notify_one();
        self.space_ready.notify_waiters();
        self.exit_ready.notify_one();
    }

    async fn run(
        id: LanguageServerId,
        client_tx: Sender<(LanguageServerId, ServerEvent)>,
        state: Arc<Mutex<InboundState>>,
        ready: Arc<Notify>,
        space_ready: Arc<Notify>,
        exit_ready: Arc<Notify>,
    ) {
        let mut initialized_sent = false;
        loop {
            let ready_wait = ready.notified();
            tokio::pin!(ready_wait);
            ready_wait.as_mut().enable();

            let (initialized, exited, event, freed_slot) = {
                let mut state = state.lock();
                let mut freed_slot = false;
                let event = loop {
                    match state.queue.pop_front() {
                        Some(InboundQueueItem::Reliable(event)) => {
                            freed_slot = true;
                            break Some(event);
                        }
                        Some(InboundQueueItem::Diagnostics(key)) => {
                            if let Some(params) = state.diagnostics.remove(&key) {
                                freed_slot = true;
                                break Some(ServerEvent::Notification(
                                    Notification::PublishDiagnostics(params),
                                ));
                            }
                        }
                        Some(InboundQueueItem::Progress(token)) => {
                            let mut remove = false;
                            let work = state.progress.get_mut(&token).and_then(|pending| {
                                let work = pending.pop_front();
                                remove = pending.is_empty();
                                work
                            });
                            if remove {
                                state.progress.remove(&token);
                                freed_slot = true;
                            } else if work.is_some() {
                                state
                                    .queue
                                    .push_back(InboundQueueItem::Progress(token.clone()));
                            }
                            if let Some(work) = work {
                                break Some(ServerEvent::Notification(
                                    Notification::ProgressMessage(lsp::ProgressParams {
                                        token,
                                        value: lsp::ProgressParamsValue::WorkDone(work),
                                    }),
                                ));
                            }
                        }
                        None => break None,
                    }
                };
                (state.initialized, state.exited, event, freed_slot)
            };

            if freed_slot {
                space_ready.notify_waiters();
            }

            if exited {
                let _ = client_tx
                    .send((id, ServerEvent::Notification(Notification::Exit)))
                    .await;
                return;
            }

            if initialized && !initialized_sent {
                tokio::select! {
                    result = client_tx.send((
                        id,
                        ServerEvent::Notification(Notification::Initialized),
                    )) => {
                        if result.is_err() {
                            return;
                        }
                        initialized_sent = true;
                    }
                    _ = exit_ready.notified() => continue,
                }
                continue;
            }

            let Some(event) = event else {
                ready_wait.await;
                continue;
            };

            tokio::select! {
                result = client_tx.send((id, event)) => {
                    if result.is_err() {
                        return;
                    }
                }
                _ = exit_ready.notified() => {}
            }
        }
    }

    #[cfg(test)]
    fn queue_len(&self) -> usize {
        self.state.lock().queue.len()
    }
}

fn coalesce_inbound(state: &mut InboundState, event: &mut Option<InboundEvent>) -> bool {
    match event.as_ref().expect("inbound event disappeared") {
        InboundEvent::Diagnostics(params)
            if state.diagnostics.contains_key(params.uri.as_str()) =>
        {
            let InboundEvent::Diagnostics(incoming) =
                event.take().expect("inbound event disappeared")
            else {
                unreachable!()
            };
            let key = incoming.uri.to_string();
            let existing = state
                .diagnostics
                .get_mut(&key)
                .expect("diagnostics key disappeared");
            if incoming
                .version
                .zip(existing.version)
                .is_none_or(|(new, old)| new >= old)
            {
                *existing = incoming;
            }
            state.coalesced_diagnostics += 1;
            true
        }
        InboundEvent::Progress(params) if state.progress.contains_key(&params.token) => {
            let InboundEvent::Progress(params) = event.take().expect("inbound event disappeared")
            else {
                unreachable!()
            };
            let lsp::ProgressParamsValue::WorkDone(work) = params.value;
            fold_progress(
                state
                    .progress
                    .get_mut(&params.token)
                    .expect("progress key disappeared"),
                work,
            );
            state.coalesced_progress += 1;
            true
        }
        _ => false,
    }
}

fn admit_inbound(state: &mut InboundState, event: InboundEvent) {
    match event {
        InboundEvent::Reliable(event) => {
            state.queue.push_back(InboundQueueItem::Reliable(event));
        }
        InboundEvent::Diagnostics(params) => {
            let key = params.uri.to_string();
            state.diagnostics.insert(key.clone(), params);
            state.queue.push_back(InboundQueueItem::Diagnostics(key));
        }
        InboundEvent::Progress(params) => {
            let lsp::ProgressParamsValue::WorkDone(work) = params.value;
            let token = params.token;
            let mut pending = VecDeque::with_capacity(2);
            fold_progress(&mut pending, work);
            state.progress.insert(token.clone(), pending);
            state.queue.push_back(InboundQueueItem::Progress(token));
        }
    }
}

fn fold_progress(pending: &mut VecDeque<lsp::WorkDoneProgress>, work: lsp::WorkDoneProgress) {
    match work {
        lsp::WorkDoneProgress::Begin(_) => {
            pending.clear();
            pending.push_back(work);
        }
        lsp::WorkDoneProgress::Report(_) => match pending.back_mut() {
            Some(current @ lsp::WorkDoneProgress::Report(_)) => *current = work,
            Some(lsp::WorkDoneProgress::End(_)) => {}
            _ => pending.push_back(work),
        },
        lsp::WorkDoneProgress::End(_) => {
            pending.retain(|event| matches!(event, lsp::WorkDoneProgress::Begin(_)));
            pending.push_back(work);
        }
    }
}

#[derive(Debug)]
struct RequestLease {
    pending_requests: PendingRegistry,
    outbound: OutboundMailbox,
    id: Option<jsonrpc::Id>,
}

impl RequestLease {
    fn new(
        pending_requests: PendingRegistry,
        outbound: OutboundMailbox,
        id: jsonrpc::Id,
        sender: PendingSender,
    ) -> Result<Self> {
        pending_requests.register(id.clone(), sender)?;
        Ok(Self {
            pending_requests,
            outbound,
            id: Some(id),
        })
    }

    fn disarm(&mut self) {
        self.id = None;
    }
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        if matches!(
            self.pending_requests.remove(&id),
            Some(PendingRequest {
                wire_state: RequestWireState::Sent,
                ..
            })
        ) {
            match self
                .outbound
                .send_control(ControlMessage::CancelRequest { id: id.clone() })
            {
                Ok(()) | Err(Error::StreamClosed) => {}
                Err(Error::OutboundControlQueueFull) => log::debug!(
                    "Discarded {} for request {:?} because the outbound control queue is full",
                    lsp::notification::Cancel::METHOD,
                    id,
                ),
                Err(error) => log::debug!(
                    "Failed to enqueue {} for request {:?}: {error}",
                    lsp::notification::Cancel::METHOD,
                    id,
                ),
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TransportHandle {
    outbound: OutboundMailbox,
    inbound: InboundDispatcher,
    pending_requests: PendingRegistry,
    initialization: Initialization,
}

impl TransportHandle {
    pub(crate) async fn request(
        &self,
        request: jsonrpc::MethodCall,
        timeout: Duration,
    ) -> Result<Value> {
        let id = request.id.clone();
        let (response_tx, response_rx) = oneshot::channel();
        let mut lease = RequestLease::new(
            self.pending_requests.clone(),
            self.outbound.clone(),
            id.clone(),
            response_tx,
        )?;

        self.outbound
            .send_payload_async(Payload::Request { value: request })
            .await?;
        let response = tokio::time::timeout(timeout, response_rx).await;

        match response {
            Ok(Ok(response)) => {
                lease.disarm();
                response
            }
            Ok(Err(_)) => Err(Error::StreamClosed),
            Err(_) => Err(Error::Timeout(id)),
        }
    }

    pub(crate) async fn request_deferred(
        &self,
        id: jsonrpc::Id,
        method: &'static str,
        serialize: impl FnOnce() -> serde_json::Result<String> + Send + 'static,
        timeout: Duration,
    ) -> Result<Value> {
        let (response_tx, response_rx) = oneshot::channel();
        let mut lease = RequestLease::new(
            self.pending_requests.clone(),
            self.outbound.clone(),
            id.clone(),
            response_tx,
        )?;

        self.outbound
            .send_payload_async(Payload::deferred_request(id.clone(), method, serialize))
            .await?;
        let response = tokio::time::timeout(timeout, response_rx).await;

        match response {
            Ok(Ok(response)) => {
                lease.disarm();
                response
            }
            Ok(Err(_)) => Err(Error::StreamClosed),
            Err(_) => Err(Error::Timeout(id)),
        }
    }

    pub(crate) fn send(&self, payload: Payload) -> Result<()> {
        self.outbound.send_payload(payload)
    }

    pub(crate) fn reply(&self, output: jsonrpc::Output) -> Result<()> {
        self.outbound
            .send_control(ControlMessage::Response { output, sent: None })
    }

    pub(crate) async fn reply_async(&self, output: jsonrpc::Output) -> Result<()> {
        let (sent_tx, sent_rx) = oneshot::channel();
        self.outbound
            .send_control_async(ControlMessage::Response {
                output,
                sent: Some(sent_tx),
            })
            .await?;
        sent_rx.await.map_err(|_| Error::StreamClosed)
    }

    pub(crate) async fn initialized(&self) -> Result<()> {
        let notification = jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: lsp::notification::Initialized::METHOD.to_string(),
            params: jsonrpc::Params::Map(serde_json::Map::new()),
        };
        let (sent_tx, sent_rx) = oneshot::channel();
        if self
            .outbound
            .send_control_async(ControlMessage::Initialized {
                notification,
                sent: sent_tx,
            })
            .await
            .is_err()
        {
            self.close();
            return Err(Error::StreamClosed);
        }
        if sent_rx.await.is_err() {
            self.close();
            return Err(Error::StreamClosed);
        }
        Ok(())
    }

    pub(crate) fn fail_initialization(&self, error: impl Into<Arc<str>>) {
        self.initialization.fail(error);
        self.pending_requests.close();
        self.outbound.close();
        self.inbound.terminate();
    }

    pub(crate) async fn wait_for_initialization(&self) -> InitializationState {
        self.initialization.wait().await
    }

    pub(crate) fn initialization_state(&self) -> InitializationState {
        self.initialization.current()
    }

    fn close(&self) {
        self.initialization.close();
        self.pending_requests.close();
        self.outbound.close();
        self.inbound.terminate();
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.pending_requests.pending_count()
    }
}

/// A type representing all possible values sent from the server to the client.
#[derive(Debug, PartialEq, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum ServerMessage {
    /// A regular JSON-RPC request output (single response).
    Output(jsonrpc::Output),
    /// A JSON-RPC request or notification.
    Call(jsonrpc::Call),
}

const SLOW_MESSAGE_CODEC_THRESHOLD: Duration = Duration::from_millis(8);
const MAX_RAW_PAYLOAD_LOG_BYTES: usize = 16 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_STDERR_LINE_BYTES: usize = 16 * 1024;

async fn read_bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin + Send),
    buffer: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize> {
    buffer.clear();
    let mut total = 0usize;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let retained = limit.saturating_sub(buffer.len()).min(consumed);
        buffer.extend_from_slice(&available[..retained]);
        total = total.saturating_add(consumed);
        reader.consume(consumed);

        if newline.is_some() {
            return Ok(total);
        }
    }
}

fn stderr_is_failure(line: &str) -> bool {
    let line = line.trim_start();
    line.contains(" panicked at ")
        || line.starts_with("thread '") && line.contains("panicked at")
        || line.contains(" ERROR ")
        || line.starts_with("ERROR")
}

fn log_inbound_summary(language_server_name: &str, message: &ServerMessage, bytes: usize) {
    match message {
        ServerMessage::Output(jsonrpc::Output::Success(success)) => log::debug!(
            "lsp_rpc direction=in server={} kind=response id={:?} outcome=success bytes={}",
            language_server_name,
            success.id,
            bytes,
        ),
        ServerMessage::Output(jsonrpc::Output::Failure(failure)) => log::debug!(
            "lsp_rpc direction=in server={} kind=response id={:?} outcome=failure bytes={}",
            language_server_name,
            failure.id,
            bytes,
        ),
        ServerMessage::Call(jsonrpc::Call::MethodCall(call)) => log::debug!(
            "lsp_rpc direction=in server={} kind=request method={} id={:?} bytes={}",
            language_server_name,
            call.method,
            call.id,
            bytes,
        ),
        ServerMessage::Call(jsonrpc::Call::Notification(notification)) => log::debug!(
            "lsp_rpc direction=in server={} kind=notification method={} bytes={}",
            language_server_name,
            notification.method,
            bytes,
        ),
        ServerMessage::Call(jsonrpc::Call::Invalid { .. }) => log::debug!(
            "lsp_rpc direction=in server={} kind=invalid bytes={}",
            language_server_name,
            bytes,
        ),
    }
}

#[derive(Debug)]
pub struct Transport {
    name: String,
    pending_requests: PendingRegistry,
    outbound: OutboundMailbox,
    inbound: InboundDispatcher,
    initialization: Initialization,
}

impl Transport {
    pub(crate) fn start<R, W, E>(
        server_stdout: BufReader<R>,
        server_stdin: BufWriter<W>,
        server_stderr: BufReader<E>,
        id: LanguageServerId,
        name: String,
    ) -> (Receiver<(LanguageServerId, ServerEvent)>, TransportHandle)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
        E: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        Self::start_with_capacities(
            server_stdout,
            server_stdin,
            server_stderr,
            id,
            name,
            DEFAULT_TRANSPORT_CAPACITIES,
        )
    }

    #[cfg(test)]
    fn start_with_capacity<R, W, E>(
        server_stdout: BufReader<R>,
        server_stdin: BufWriter<W>,
        server_stderr: BufReader<E>,
        id: LanguageServerId,
        name: String,
        priority_capacity: usize,
    ) -> (Receiver<(LanguageServerId, ServerEvent)>, TransportHandle)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
        E: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        Self::start_with_capacities(
            server_stdout,
            server_stdin,
            server_stderr,
            id,
            name,
            TransportCapacities {
                outbound: DEFAULT_OUTBOUND_CAPACITY,
                priority: priority_capacity,
                ..DEFAULT_TRANSPORT_CAPACITIES
            },
        )
    }

    fn start_with_capacities<R, W, E>(
        server_stdout: BufReader<R>,
        server_stdin: BufWriter<W>,
        server_stderr: BufReader<E>,
        id: LanguageServerId,
        name: String,
        capacities: TransportCapacities,
    ) -> (Receiver<(LanguageServerId, ServerEvent)>, TransportHandle)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
        E: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let (client_tx, rx) = channel(capacities.client.max(1));
        let pending_requests = PendingRegistry::default();
        let initialization = Initialization::new();
        let outbound = OutboundMailbox::new(capacities.outbound, capacities.priority);
        let inbound =
            InboundDispatcher::start(id, Arc::from(name.as_str()), client_tx, capacities.inbound);

        let transport = Self {
            name,
            pending_requests: pending_requests.clone(),
            outbound: outbound.clone(),
            inbound: inbound.clone(),
            initialization: initialization.clone(),
        };

        let transport = Arc::new(transport);

        tokio::spawn(Self::recv(transport.clone(), server_stdout));
        tokio::spawn(Self::err(transport.clone(), server_stderr));
        tokio::spawn(Self::send(transport, server_stdin));

        let handle = TransportHandle {
            outbound,
            inbound,
            pending_requests,
            initialization,
        };
        (rx, handle)
    }

    fn terminate(&self) {
        self.initialization.close();
        self.pending_requests.close();
        self.outbound.close();
        self.inbound.terminate();
    }

    async fn recv_server_message(
        reader: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut Vec<u8>,
        content: &mut Vec<u8>,
        language_server_name: &str,
    ) -> Result<ServerMessage> {
        let mut content_length = None;
        loop {
            let line_bytes = read_bounded_line(reader, buffer, MAX_HEADER_BYTES).await?;
            if line_bytes == 0 {
                return Err(Error::StreamClosed);
            }
            if line_bytes > MAX_HEADER_BYTES {
                return Err(Error::HeaderTooLarge {
                    limit: MAX_HEADER_BYTES,
                });
            }

            // debug!("<- header {:?}", buffer);

            if buffer == b"\r\n" {
                // look for an empty CRLF line
                break;
            }

            let header = std::str::from_utf8(buffer)
                .context("invalid utf8 in language server header")?
                .trim();

            let parts = header.split_once(": ");

            match parts {
                Some(("Content-Length", value)) => {
                    content_length = Some(value.parse().context("invalid content length")?);
                }
                Some((_, _)) => {}
                None => {
                    // Workaround: Some non-conformant language servers will output logging and other garbage
                    // into the same stream as JSON-RPC messages. This can also happen from shell scripts that spawn
                    // the server. Skip such lines and log a warning.

                    // warn!("Failed to parse header: {:?}", header);
                }
            }
        }

        let content_length = content_length.context("missing content length")?;
        if content_length > MAX_MESSAGE_BYTES {
            return Err(Error::MessageTooLarge {
                size: content_length,
                limit: MAX_MESSAGE_BYTES,
            });
        }
        content.resize(content_length, 0);
        reader.read_exact(content).await?;
        let msg = std::str::from_utf8(content).context("invalid utf8 from server")?;

        if content_length <= MAX_RAW_PAYLOAD_LOG_BYTES {
            log::trace!("{language_server_name} <- {msg}");
        } else {
            log::trace!("{language_server_name} <- <raw payload omitted; bytes={content_length}>");
        }
        let parse_start = Instant::now();

        // NOTE: We avoid using `?` here, since it would return early on error
        // and skip clearing `content`. By returning the result directly instead,
        // we ensure `content.clear()` is always called.
        let output = sonic_rs::from_slice(content).map_err(Into::into);
        if let Ok(message) = &output {
            log_inbound_summary(language_server_name, message, content_length);
        }
        let parse_elapsed = parse_start.elapsed();
        if parse_elapsed >= SLOW_MESSAGE_CODEC_THRESHOLD {
            info!(
                "lsp_rpc phase=parse_slow server={} bytes={} elapsed_us={}",
                language_server_name,
                content_length,
                parse_elapsed.as_micros(),
            );
        }

        content.clear();

        output
    }

    async fn recv_server_error(
        err: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut Vec<u8>,
        language_server_name: &str,
    ) -> Result<()> {
        let line_bytes = read_bounded_line(err, buffer, MAX_STDERR_LINE_BYTES).await?;
        if line_bytes == 0 {
            return Err(Error::StreamClosed);
        };
        let line = String::from_utf8_lossy(buffer);
        let line = line.trim_end_matches(['\r', '\n']);
        let truncated = line_bytes > buffer.len();
        if stderr_is_failure(line) {
            log::warn!(
                "language_server_stderr server={} truncated={} message={:?}",
                language_server_name,
                truncated,
                line,
            );
        } else {
            log::debug!(
                "language_server_stderr server={} truncated={} message={:?}",
                language_server_name,
                truncated,
                line,
            );
        }

        Ok(())
    }

    async fn send_payload_to_server<W>(
        &self,
        server_stdin: &mut BufWriter<W>,
        payload: Payload,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin + Send,
    {
        //TODO: reuse string
        let json = match payload {
            Payload::Request { value } => {
                if !self.pending_requests.mark_sent(&value.id) {
                    log::trace!(
                        "Skipping language-server request canceled before it reached the writer (id={:?})",
                        value.id
                    );
                    return Ok(());
                }
                let json = serde_json::to_string(&value)?;
                log::debug!(
                    "lsp_rpc direction=out server={} kind=request method={} id={:?} bytes={}",
                    self.name,
                    value.method,
                    value.id,
                    json.len(),
                );
                json
            }
            Payload::DeferredRequest(request) => {
                if !self.pending_requests.mark_sent(&request.id) {
                    log::trace!(
                        "Skipping language-server request canceled before it reached the writer (id={:?})",
                        request.id
                    );
                    return Ok(());
                }
                let id = request.id;
                let method = request.method;
                let serialize_start = Instant::now();
                let json = tokio::task::spawn_blocking(request.serialize)
                    .await
                    .map_err(|error| Error::Other(error.into()))??;
                let serialize_elapsed = serialize_start.elapsed();
                if serialize_elapsed >= SLOW_MESSAGE_CODEC_THRESHOLD {
                    info!(
                        "lsp_rpc phase=serialize_slow server={} method={} bytes={} elapsed_us={}",
                        self.name,
                        method,
                        json.len(),
                        serialize_elapsed.as_micros(),
                    );
                }
                log::debug!(
                    "lsp_rpc direction=out server={} kind=request method={} id={:?} bytes={} deferred=true",
                    self.name,
                    method,
                    id,
                    json.len(),
                );
                json
            }
            Payload::CancelRequest { id } => {
                let notification = jsonrpc::Notification {
                    jsonrpc: Some(jsonrpc::Version::V2),
                    method: lsp::notification::Cancel::METHOD.to_string(),
                    params: jsonrpc::Params::Map(
                        serde_json::to_value(lsp::CancelParams {
                            id: jsonrpc_id_to_lsp_id(&id),
                        })?
                        .as_object()
                        .cloned()
                        .context("cancel params must serialize to an object")?,
                    ),
                };
                let json = serde_json::to_string(&notification)?;
                log::debug!(
                    "lsp_rpc direction=out server={} kind=notification method={} id={:?} bytes={}",
                    self.name,
                    lsp::notification::Cancel::METHOD,
                    id,
                    json.len(),
                );
                json
            }
            Payload::Notification(value) => {
                let json = serde_json::to_string(&value)?;
                log::debug!(
                    "lsp_rpc direction=out server={} kind=notification method={} bytes={}",
                    self.name,
                    value.method,
                    json.len(),
                );
                json
            }
            Payload::DeferredNotification(deferred) => {
                let method = deferred.method;
                let serialize_start = Instant::now();
                let json = tokio::task::spawn_blocking(deferred.serialize)
                    .await
                    .map_err(|error| Error::Other(error.into()))??;
                let serialize_elapsed = serialize_start.elapsed();
                if serialize_elapsed >= SLOW_MESSAGE_CODEC_THRESHOLD {
                    info!(
                        "lsp_rpc phase=serialize_slow server={} method={} bytes={} elapsed_us={}",
                        self.name,
                        method,
                        json.len(),
                        serialize_elapsed.as_micros(),
                    );
                }
                log::debug!(
                    "lsp_rpc direction=out server={} kind=notification method={} bytes={} deferred=true",
                    self.name,
                    method,
                    json.len(),
                );
                json
            }
            Payload::DocumentWire(wire) => {
                let method = wire.method();
                let serialize_start = Instant::now();
                let json = tokio::task::spawn_blocking(move || wire.serialize())
                    .await
                    .map_err(|error| Error::Other(error.into()))??;
                let serialize_elapsed = serialize_start.elapsed();
                if serialize_elapsed >= SLOW_MESSAGE_CODEC_THRESHOLD {
                    info!(
                        "lsp_rpc phase=serialize_slow server={} method={} bytes={} elapsed_us={}",
                        self.name,
                        method,
                        json.len(),
                        serialize_elapsed.as_micros(),
                    );
                }
                log::debug!(
                    "lsp_rpc direction=out server={} kind=notification method={} bytes={} deferred=true reconstructable=true",
                    self.name,
                    method,
                    json.len(),
                );
                json
            }
            Payload::DocumentUpdate(_) | Payload::DocumentBatch(_) => {
                unreachable!("document state must be reduced before reaching the writer")
            }
            Payload::Response(output) => {
                let json = serde_json::to_string(&output)?;
                match &output {
                    jsonrpc::Output::Success(success) => log::debug!(
                        "lsp_rpc direction=out server={} kind=response id={:?} outcome=success bytes={}",
                        self.name,
                        success.id,
                        json.len(),
                    ),
                    jsonrpc::Output::Failure(failure) => log::debug!(
                        "lsp_rpc direction=out server={} kind=response id={:?} outcome=failure bytes={}",
                        self.name,
                        failure.id,
                        json.len(),
                    ),
                }
                json
            }
        };
        self.send_string_to_server(server_stdin, json, &self.name)
            .await
    }

    async fn send_string_to_server<W>(
        &self,
        server_stdin: &mut BufWriter<W>,
        request: String,
        language_server_name: &str,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin + Send,
    {
        if request.len() <= MAX_RAW_PAYLOAD_LOG_BYTES {
            log::trace!("{language_server_name} -> {request}");
        } else {
            log::trace!(
                "{language_server_name} -> <raw payload omitted; bytes={}>",
                request.len()
            );
        }

        // send the headers
        server_stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", request.len()).as_bytes())
            .await?;

        // send the body
        server_stdin.write_all(request.as_bytes()).await?;

        server_stdin.flush().await?;

        Ok(())
    }

    async fn process_server_message(
        &self,
        msg: ServerMessage,
        language_server_name: &str,
    ) -> Result<()> {
        match msg {
            ServerMessage::Output(output) => {
                self.process_request_response(output, language_server_name)?
            }
            ServerMessage::Call(call) => self.inbound.send(call).await?,
        };
        Ok(())
    }

    fn process_request_response(
        &self,
        output: jsonrpc::Output,
        language_server_name: &str,
    ) -> Result<()> {
        let (id, result) = match output {
            jsonrpc::Output::Success(jsonrpc::Success { id, result, .. }) => (id, Ok(result)),
            jsonrpc::Output::Failure(jsonrpc::Failure { id, error, .. }) => (id, Err(error.into())),
        };

        if let Some(tx) = self.pending_requests.remove_sent(&id) {
            if let Err(error) = &result {
                if let Some(cancellation) = cancellation_name(error) {
                    log::debug!(
                        "lsp_rpc direction=in server={} kind=response id={:?} outcome=cancelled cancellation={}",
                        language_server_name,
                        id,
                        cancellation,
                    );
                } else {
                    error!("{language_server_name} <- {error}");
                }
            }
            match tx.send(result) {
                Ok(_) => (),
                Err(_) => log::debug!(
                    "Discarded response after request completion or cancellation (id={:?})",
                    id
                ),
            };
        } else {
            log::trace!(
                "Discarding Language Server response without a pending request (id={:?}) {:?}",
                id,
                result
            );
        }

        Ok(())
    }

    async fn recv<R>(transport: Arc<Self>, mut server_stdout: BufReader<R>)
    where
        R: tokio::io::AsyncRead + Unpin + Send,
    {
        let mut recv_buffer = Vec::new();
        let mut content_buffer = Vec::new();
        loop {
            match Self::recv_server_message(
                &mut server_stdout,
                &mut recv_buffer,
                &mut content_buffer,
                &transport.name,
            )
            .await
            {
                Ok(msg) => {
                    match transport.process_server_message(msg, &transport.name).await {
                        Ok(_) => {}
                        Err(err) => {
                            error!("{} err: <- {err:?}", transport.name);
                            break;
                        }
                    };
                }
                Err(err) => {
                    if !matches!(err, Error::StreamClosed) {
                        error!(
                            "Exiting {} after unexpected error: {err:?}",
                            &transport.name
                        );
                    }

                    break;
                }
            }
        }
        transport.terminate();
    }

    async fn err<E>(transport: Arc<Self>, mut server_stderr: BufReader<E>)
    where
        E: tokio::io::AsyncRead + Unpin + Send,
    {
        let mut recv_buffer = Vec::new();
        loop {
            match Self::recv_server_error(&mut server_stderr, &mut recv_buffer, &transport.name)
                .await
            {
                Ok(_) => {}
                Err(Error::StreamClosed) => break,
                Err(err) => {
                    error!("{} stderr reader failed: {err:?}", transport.name);
                    break;
                }
            }
        }
    }

    async fn send<W>(transport: Arc<Self>, mut server_stdin: BufWriter<W>)
    where
        W: AsyncWrite + Unpin + Send,
    {
        let mut priority_streak = 0;
        let mut initialize_sent = false;
        let mut fatal_error = None;

        fn is_shutdown(payload: &Payload) -> bool {
            use lsp::request::{Request, Shutdown};
            matches!(payload, Payload::Request { value: jsonrpc::MethodCall { method, .. }, .. } if method == Shutdown::METHOD)
                || matches!(payload, Payload::DeferredRequest(DeferredRequest { method, .. }) if *method == Shutdown::METHOD)
        }

        loop {
            let normal_mode = match (transport.initialization.current(), initialize_sent) {
                (InitializationState::Pending, false) => NormalReceiveMode::InitializeOnly,
                (InitializationState::Pending, true) => NormalReceiveMode::None,
                _ => NormalReceiveMode::Any,
            };
            let Some(message) = transport
                .outbound
                .recv(&mut priority_streak, normal_mode)
                .await
            else {
                break;
            };

            let result = match message {
                OutboundMessage::Payload(payload) => {
                    let initialize = payload_is_initialize(&payload);
                    let result = match transport.initialization.current() {
                        InitializationState::Pending if is_shutdown(&payload) => {
                            log::info!("Language server not initialized, shutting down");
                            break;
                        }
                        InitializationState::Pending if !payload_is_initialize(&payload) => {
                            unreachable!(
                                "mailbox released a non-initialize payload before initialization"
                            )
                        }
                        InitializationState::Pending | InitializationState::Initialized => {
                            transport
                                .send_payload_to_server(&mut server_stdin, payload)
                                .await
                        }
                        InitializationState::Failed(_) | InitializationState::Closed => break,
                    };
                    if initialize && result.is_ok() {
                        initialize_sent = true;
                    }
                    result
                }
                OutboundMessage::Control(ControlMessage::CancelRequest { id }) => {
                    transport
                        .send_payload_to_server(&mut server_stdin, Payload::CancelRequest { id })
                        .await
                }
                OutboundMessage::Control(ControlMessage::Response { output, sent }) => {
                    let result = transport
                        .send_payload_to_server(&mut server_stdin, Payload::Response(output))
                        .await;
                    if result.is_ok() {
                        if let Some(sent) = sent {
                            let _ = sent.send(());
                        }
                    }
                    result
                }
                OutboundMessage::Control(ControlMessage::Initialized { notification, sent }) => {
                    match transport.initialization.current() {
                        InitializationState::Pending => {
                            let result = transport
                                .send_payload_to_server(
                                    &mut server_stdin,
                                    Payload::Notification(notification),
                                )
                                .await;
                            if result.is_ok() {
                                if !transport.initialization.initialized() {
                                    break;
                                }
                                initialize_sent = false;
                                transport.inbound.mark_initialized();
                                let _ = sent.send(());
                            }
                            result
                        }
                        InitializationState::Initialized => {
                            let _ = sent.send(());
                            Ok(())
                        }
                        InitializationState::Failed(_) | InitializationState::Closed => break,
                    }
                }
            };

            if let Err(error) = result {
                fatal_error = Some(error);
                break;
            }
        }

        if let Some(error) = fatal_error {
            error!("{} writer failed: {error:?}", transport.name);
        }
        transport.terminate();
    }
}

fn jsonrpc_id_to_lsp_id(id: &jsonrpc::Id) -> lsp::NumberOrString {
    match id {
        jsonrpc::Id::Num(id) => i32::try_from(*id)
            .map(lsp::NumberOrString::Number)
            .unwrap_or_else(|_| lsp::NumberOrString::String(id.to_string())),
        jsonrpc::Id::Str(id) => lsp::NumberOrString::String(id.clone()),
        jsonrpc::Id::Null => lsp::NumberOrString::String("null".to_string()),
    }
}

fn cancellation_name(error: &Error) -> Option<&'static str> {
    let Error::Rpc(error) = error else {
        return None;
    };
    match error.code.code() {
        lsp::error_codes::SERVER_CANCELLED => Some("server_cancelled"),
        lsp::error_codes::REQUEST_CANCELLED => Some("request_cancelled"),
        lsp::error_codes::CONTENT_MODIFIED => Some("content_modified"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::{Output, Success, Version};
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Barrier,
        },
        thread,
    };
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

    fn method_call(id: u64, method: &str) -> jsonrpc::MethodCall {
        jsonrpc::MethodCall {
            jsonrpc: Some(Version::V2),
            id: jsonrpc::Id::Num(id),
            method: method.to_string(),
            params: jsonrpc::Params::None,
        }
    }

    fn notification(method: &str) -> Payload {
        Payload::Notification(jsonrpc::Notification {
            jsonrpc: Some(Version::V2),
            method: method.to_string(),
            params: jsonrpc::Params::None,
        })
    }

    fn diagnostics(uri: &str, version: i64) -> jsonrpc::Call {
        let Value::Object(params) = serde_json::json!({
            "uri": uri,
            "version": version,
            "diagnostics": [],
        }) else {
            unreachable!()
        };
        jsonrpc::Call::Notification(jsonrpc::Notification {
            jsonrpc: Some(Version::V2),
            method: lsp::notification::PublishDiagnostics::METHOD.to_string(),
            params: jsonrpc::Params::Map(params),
        })
    }

    fn show_message(message: &str) -> jsonrpc::Call {
        let Value::Object(params) = serde_json::json!({
            "type": lsp::MessageType::INFO,
            "message": message,
        }) else {
            unreachable!()
        };
        jsonrpc::Call::Notification(jsonrpc::Notification {
            jsonrpc: Some(Version::V2),
            method: lsp::notification::ShowMessage::METHOD.to_owned(),
            params: jsonrpc::Params::Map(params),
        })
    }

    fn diagnostics_version(event: &ServerEvent) -> Option<i32> {
        let ServerEvent::Notification(Notification::PublishDiagnostics(params)) = event else {
            return None;
        };
        params.version
    }

    fn show_message_text(event: &ServerEvent) -> Option<&str> {
        let ServerEvent::Notification(Notification::ShowMessage(params)) = event else {
            return None;
        };
        Some(&params.message)
    }

    fn document_open(uri: &str, version: i32, text: &str) -> Payload {
        Payload::DocumentUpdate(DocumentUpdate::Open {
            uri: lsp::Url::parse(uri).unwrap(),
            version,
            text: Rope::from(text),
            language_id: "rust".to_owned(),
        })
    }

    fn document_change(uri: &str, version: i32, new: &str) -> Payload {
        Payload::DocumentUpdate(DocumentUpdate::Change(DocumentChangeTarget {
            text_document: lsp::VersionedTextDocumentIdentifier {
                uri: lsp::Url::parse(uri).unwrap(),
                version,
            },
            new_text: Rope::from(new),
            sync_kind: lsp::TextDocumentSyncKind::INCREMENTAL,
            offset_encoding: crate::OffsetEncoding::Utf16,
        }))
    }

    fn document_save(uri: &str, text: &str, include_text: bool) -> Payload {
        Payload::DocumentUpdate(DocumentUpdate::Save {
            uri: lsp::Url::parse(uri).unwrap(),
            text: Rope::from(text),
            include_text,
        })
    }

    fn document_close(uri: &str) -> Payload {
        Payload::DocumentUpdate(DocumentUpdate::Close {
            uri: lsp::Url::parse(uri).unwrap(),
        })
    }

    #[test]
    fn inbound_queue_profile_classifies_typed_backlog() {
        let state = InboundState {
            queue: VecDeque::from([
                InboundQueueItem::Diagnostics("file:///workspace/src/lib.rs".to_owned()),
                InboundQueueItem::Progress(lsp::ProgressToken::String("index".to_owned())),
                InboundQueueItem::Reliable(ServerEvent::Notification(Notification::LogMessage(
                    lsp::LogMessageParams {
                        typ: lsp::MessageType::INFO,
                        message: "indexing".to_owned(),
                    },
                ))),
                InboundQueueItem::Reliable(ServerEvent::Request(crate::ServerRequest {
                    id: jsonrpc::Id::Num(1),
                    method: "workspace/workspaceFolders".to_owned(),
                    request: Ok(crate::MethodCall::WorkspaceFolders),
                })),
                InboundQueueItem::Reliable(ServerEvent::Notification(Notification::ShowMessage(
                    lsp::ShowMessageParams {
                        typ: lsp::MessageType::INFO,
                        message: "ready".to_owned(),
                    },
                ))),
                InboundQueueItem::Reliable(ServerEvent::Invalid {
                    id: jsonrpc::Id::Null,
                }),
            ]),
            ..InboundState::default()
        };

        assert_eq!(
            inbound_queue_profile(&state),
            InboundQueueProfile {
                diagnostics: 1,
                progress: 1,
                log_messages: 1,
                requests: 1,
                notifications: 1,
                invalid: 1,
            }
        );
    }

    #[test]
    fn progress_fold_preserves_lifecycle_and_latest_report() {
        let mut pending = VecDeque::new();
        fold_progress(
            &mut pending,
            lsp::WorkDoneProgress::Begin(lsp::WorkDoneProgressBegin {
                title: "Index".to_owned(),
                cancellable: None,
                message: None,
                percentage: Some(0),
            }),
        );
        for percentage in [10, 75] {
            fold_progress(
                &mut pending,
                lsp::WorkDoneProgress::Report(lsp::WorkDoneProgressReport {
                    cancellable: None,
                    message: None,
                    percentage: Some(percentage),
                }),
            );
        }
        assert_eq!(pending.len(), 2);
        assert!(matches!(
            pending.back(),
            Some(lsp::WorkDoneProgress::Report(report)) if report.percentage == Some(75)
        ));

        fold_progress(
            &mut pending,
            lsp::WorkDoneProgress::End(lsp::WorkDoneProgressEnd { message: None }),
        );
        assert_eq!(pending.len(), 2);
        assert!(matches!(
            pending.front(),
            Some(lsp::WorkDoneProgress::Begin(_))
        ));
        assert!(matches!(
            pending.back(),
            Some(lsp::WorkDoneProgress::End(_))
        ));
    }

    #[tokio::test]
    async fn outbound_mailbox_bounds_reliable_payloads_and_wakes_waiters() {
        let outbound = OutboundMailbox::new(1, 1);
        outbound.send_payload(notification("test/first")).unwrap();
        assert!(matches!(
            outbound.send_payload(notification("test/full")),
            Err(Error::OutboundQueueFull)
        ));

        let blocked_outbound = outbound.clone();
        let blocked = tokio::spawn(async move {
            blocked_outbound
                .send_payload_async(notification("test/second"))
                .await
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());

        let mut priority_streak = 0;
        assert!(matches!(
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Payload(_))
        ));
        blocked.await.unwrap().unwrap();
        assert_eq!(outbound.queue_lengths(), (1, 0));
    }

    #[tokio::test]
    async fn outbound_mailbox_folds_document_targets_without_losing_barriers() {
        let outbound = OutboundMailbox::new(1, 1);
        outbound.send_payload(notification("test/blocker")).unwrap();
        outbound
            .send_payload(document_open("file:///one.rs", 0, "a"))
            .unwrap();
        outbound
            .send_payload(document_change("file:///one.rs", 1, "ab"))
            .unwrap();
        outbound
            .send_payload(document_change("file:///one.rs", 2, "abc"))
            .unwrap();
        assert_eq!(outbound.queue_lengths(), (2, 0));
        assert!(matches!(
            outbound.send_payload(notification("test/over-capacity")),
            Err(Error::OutboundQueueFull)
        ));

        let mut priority_streak = 0;
        assert!(matches!(
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Payload(Payload::Notification(
                jsonrpc::Notification { method, .. }
            ))) if method == "test/blocker"
        ));

        let Some(OutboundMessage::Payload(Payload::DocumentWire(DocumentWire::Open {
            version,
            text,
            ..
        }))) = outbound
            .recv(&mut priority_streak, NormalReceiveMode::Any)
            .await
        else {
            panic!("expected reduced document open")
        };
        assert_eq!(version, 2);
        assert_eq!(text.to_string(), "abc");

        outbound.send_payload(notification("test/barrier")).unwrap();
        outbound
            .send_payload(document_change("file:///one.rs", 3, "abcd"))
            .unwrap();
        outbound
            .send_payload(document_change("file:///one.rs", 4, "abcde"))
            .unwrap();
        assert!(matches!(
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Payload(Payload::Notification(
                jsonrpc::Notification { method, .. }
            ))) if method == "test/barrier"
        ));

        let Some(OutboundMessage::Payload(Payload::DocumentWire(DocumentWire::Change(change)))) =
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await
        else {
            panic!("expected reduced document change")
        };
        assert_eq!(change.text_document.version, 4);
        assert_eq!(change.old_text.to_string(), "abc");
        assert_eq!(change.new_text.to_string(), "abcde");
    }

    #[tokio::test]
    async fn outbound_document_state_preserves_save_close_and_reopen_lifecycle() {
        let outbound = OutboundMailbox::new(1, 1);
        let uri = "file:///one.rs";
        let mut priority_streak = 0;

        outbound.send_payload(document_open(uri, 0, "a")).unwrap();
        assert!(matches!(
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Payload(Payload::DocumentWire(
                DocumentWire::Open { version: 0, .. }
            )))
        ));

        outbound
            .send_payload(document_save(uri, "a", true))
            .unwrap();
        outbound
            .send_payload(document_change(uri, 1, "ab"))
            .unwrap();
        outbound.send_payload(document_close(uri)).unwrap();

        assert!(matches!(
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Payload(Payload::DocumentWire(DocumentWire::Save {
                text: Some(text),
                ..
            }))) if text.to_string() == "a"
        ));
        assert!(matches!(
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Payload(Payload::DocumentWire(DocumentWire::Change(
                DocumentChange { text_document, .. }
            )))) if text_document.version == 1
        ));
        assert!(matches!(
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Payload(Payload::DocumentWire(
                DocumentWire::Close { .. }
            )))
        ));

        outbound.send_payload(document_open(uri, 0, "new")).unwrap();
        assert!(matches!(
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Payload(Payload::DocumentWire(DocumentWire::Open {
                version: 0,
                text,
                ..
            }))) if text.to_string() == "new"
        ));
    }

    fn success(id: u64) -> Output {
        Output::Success(Success {
            jsonrpc: Some(Version::V2),
            id: jsonrpc::Id::Num(id),
            result: Value::Null,
        })
    }

    fn detached_handle(priority_capacity: usize) -> TransportHandle {
        let pending_requests = PendingRegistry::default();
        let initialization = Initialization::new();
        let outbound = OutboundMailbox::new(DEFAULT_OUTBOUND_CAPACITY, priority_capacity);
        let inbound = InboundDispatcher {
            state: Arc::default(),
            ready: Arc::default(),
            space_ready: Arc::default(),
            exit_ready: Arc::default(),
            capacity: 1,
            server_name: Arc::from("test"),
        };
        TransportHandle {
            outbound,
            inbound,
            pending_requests,
            initialization,
        }
    }

    fn detached_transport(handle: &TransportHandle) -> Transport {
        Transport {
            name: "test".to_string(),
            pending_requests: handle.pending_requests.clone(),
            outbound: handle.outbound.clone(),
            inbound: handle.inbound.clone(),
            initialization: handle.initialization.clone(),
        }
    }

    fn duplex_transport(
        stdin_capacity: usize,
    ) -> (
        Receiver<(LanguageServerId, ServerEvent)>,
        TransportHandle,
        DuplexStream,
        BufReader<DuplexStream>,
    ) {
        let (server_stdout, transport_stdout) = duplex(4096);
        let (transport_stdin, server_stdin) = duplex(stdin_capacity);
        let (incoming, handle) = Transport::start_with_capacity(
            BufReader::new(transport_stdout),
            BufWriter::new(transport_stdin),
            BufReader::new(tokio::io::empty()),
            Default::default(),
            "test".to_string(),
            4,
        );
        (
            incoming,
            handle,
            server_stdout,
            BufReader::new(server_stdin),
        )
    }

    fn duplex_transport_with_capacities(
        stdin_capacity: usize,
        priority_capacity: usize,
        inbound_capacity: usize,
        client_capacity: usize,
    ) -> (
        Receiver<(LanguageServerId, ServerEvent)>,
        TransportHandle,
        DuplexStream,
        BufReader<DuplexStream>,
    ) {
        let (server_stdout, transport_stdout) = duplex(16 * 1024);
        let (transport_stdin, server_stdin) = duplex(stdin_capacity);
        let (incoming, handle) = Transport::start_with_capacities(
            BufReader::new(transport_stdout),
            BufWriter::new(transport_stdin),
            BufReader::new(tokio::io::empty()),
            Default::default(),
            "test".to_string(),
            TransportCapacities {
                outbound: DEFAULT_OUTBOUND_CAPACITY,
                priority: priority_capacity,
                inbound: inbound_capacity,
                client: client_capacity,
            },
        );
        (
            incoming,
            handle,
            server_stdout,
            BufReader::new(server_stdin),
        )
    }

    async fn read_wire_message(reader: &mut BufReader<DuplexStream>) -> ServerMessage {
        let mut header = Vec::new();
        let mut content = Vec::new();
        Transport::recv_server_message(reader, &mut header, &mut content, "test")
            .await
            .unwrap()
    }

    async fn write_server_output(writer: &mut DuplexStream, output: &Output) {
        let json = serde_json::to_string(output).unwrap();
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n{json}", json.len()).as_bytes())
            .await
            .unwrap();
    }

    async fn write_server_call(writer: &mut DuplexStream, call: jsonrpc::Call) {
        let json = serde_json::to_string(&call).unwrap();
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n{json}", json.len()).as_bytes())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn deferred_notification_serializes_only_in_writer() {
        let handle = detached_handle(1);
        let transport = detached_transport(&handle);
        let serialized = Arc::new(AtomicBool::new(false));
        let serialized_in_task = serialized.clone();
        let payload = Payload::deferred_notification("test/deferred", move || {
            serialized_in_task.store(true, Ordering::SeqCst);
            Ok(r#"{"jsonrpc":"2.0","method":"test/deferred"}"#.to_string())
        });

        assert!(!serialized.load(Ordering::SeqCst));

        let (client, mut server) = duplex(1024);
        let mut writer = BufWriter::new(client);
        transport
            .send_payload_to_server(&mut writer, payload)
            .await
            .unwrap();

        assert!(serialized.load(Ordering::SeqCst));
        let mut wire = vec![0; 256];
        let bytes = server.read(&mut wire).await.unwrap();
        let wire = std::str::from_utf8(&wire[..bytes]).unwrap();
        assert!(wire.contains("test/deferred"));
        assert_eq!(transport.pending_requests.pending_count(), 0);
    }

    #[tokio::test]
    async fn deferred_request_serializes_only_after_wire_admission() {
        let handle = detached_handle(1);
        let transport = detached_transport(&handle);
        let id = jsonrpc::Id::Num(90);
        let (response_tx, _response_rx) = oneshot::channel();
        let mut lease = RequestLease::new(
            handle.pending_requests.clone(),
            handle.outbound.clone(),
            id.clone(),
            response_tx,
        )
        .unwrap();
        let serialized = Arc::new(AtomicBool::new(false));
        let serialized_in_task = serialized.clone();
        let payload = Payload::deferred_request(id.clone(), "test/deferred", move || {
            serialized_in_task.store(true, Ordering::SeqCst);
            Ok(r#"{"jsonrpc":"2.0","id":90,"method":"test/deferred"}"#.to_string())
        });

        assert!(!serialized.load(Ordering::SeqCst));

        let (client, mut server) = duplex(1024);
        let mut writer = BufWriter::new(client);
        transport
            .send_payload_to_server(&mut writer, payload)
            .await
            .unwrap();

        assert!(serialized.load(Ordering::SeqCst));
        let mut wire = vec![0; 256];
        let bytes = server.read(&mut wire).await.unwrap();
        let wire = std::str::from_utf8(&wire[..bytes]).unwrap();
        assert!(wire.contains("test/deferred"));
        assert!(handle.pending_requests.remove(&id).is_some());
        lease.disarm();
    }

    #[tokio::test]
    async fn canceled_deferred_request_never_runs_its_codec() {
        let handle = detached_handle(1);
        let transport = detached_transport(&handle);
        let id = jsonrpc::Id::Num(91);
        let (response_tx, _response_rx) = oneshot::channel();
        let lease = RequestLease::new(
            handle.pending_requests.clone(),
            handle.outbound.clone(),
            id.clone(),
            response_tx,
        )
        .unwrap();
        let serialized = Arc::new(AtomicBool::new(false));
        let serialized_in_task = serialized.clone();
        let payload = Payload::deferred_request(id, "test/canceled", move || {
            serialized_in_task.store(true, Ordering::SeqCst);
            Ok(r#"{"jsonrpc":"2.0","id":91,"method":"test/canceled"}"#.to_string())
        });
        drop(lease);

        let (client, _server) = duplex(1024);
        transport
            .send_payload_to_server(&mut BufWriter::new(client), payload)
            .await
            .unwrap();

        assert!(!serialized.load(Ordering::SeqCst));
        assert_eq!(handle.pending_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn request_timeout_removes_pending_and_late_response_is_ignored() {
        let (_incoming, handle, mut server_stdout, mut server_stdin) = duplex_transport(4096);
        let request_handle = handle.clone();
        let request = tokio::spawn(async move {
            request_handle
                .request(method_call(1, "initialize"), Duration::from_secs(5))
                .await
        });

        assert!(matches!(
            read_wire_message(&mut server_stdin).await,
            ServerMessage::Call(jsonrpc::Call::MethodCall(jsonrpc::MethodCall {
                id: jsonrpc::Id::Num(1),
                ..
            }))
        ));
        assert_eq!(handle.pending_count(), 1);

        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(matches!(
            request.await.unwrap(),
            Err(Error::Timeout(jsonrpc::Id::Num(1)))
        ));
        assert_eq!(handle.pending_count(), 0);

        assert!(matches!(
            read_wire_message(&mut server_stdin).await,
            ServerMessage::Call(jsonrpc::Call::Notification(jsonrpc::Notification {
                method,
                ..
            })) if method == lsp::notification::Cancel::METHOD
        ));
        write_server_output(&mut server_stdout, &success(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(handle.pending_count(), 0);
        drop(server_stdout);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_request_timeout_does_not_send_cancel_before_wire() {
        let handle = detached_handle(1);
        handle.send(notification("test/blocker")).unwrap();

        let request_handle = handle.clone();
        let request = tokio::spawn(async move {
            request_handle
                .request(method_call(2, "test/full"), Duration::from_secs(10))
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(handle.pending_count(), 1);

        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(matches!(
            request.await.unwrap(),
            Err(Error::Timeout(jsonrpc::Id::Num(2)))
        ));
        assert_eq!(handle.pending_count(), 0);
        assert_eq!(handle.outbound.queue_lengths(), (2, 0));
    }

    #[test]
    fn response_cancel_race_always_leaves_registry_empty() {
        let handle = detached_handle(1);
        let transport = Arc::new(detached_transport(&handle));

        for id in 10..74 {
            let (response_tx, mut response_rx) = oneshot::channel();
            let lease = RequestLease::new(
                handle.pending_requests.clone(),
                handle.outbound.clone(),
                jsonrpc::Id::Num(id),
                response_tx,
            )
            .unwrap();
            assert!(handle.pending_requests.mark_sent(&jsonrpc::Id::Num(id)));
            let barrier = Arc::new(Barrier::new(3));

            let cancel_barrier = barrier.clone();
            let cancel = thread::spawn(move || {
                cancel_barrier.wait();
                drop(lease);
            });
            let response_barrier = barrier.clone();
            let response_transport = transport.clone();
            let response = thread::spawn(move || {
                response_barrier.wait();
                response_transport
                    .process_request_response(success(id), "test")
                    .unwrap();
            });

            barrier.wait();
            cancel.join().unwrap();
            response.join().unwrap();
            let _ = response_rx.try_recv();
            transport
                .process_request_response(success(id), "test")
                .unwrap();
            assert_eq!(handle.pending_count(), 0);
        }
    }

    #[test]
    fn response_cannot_complete_a_request_before_it_reaches_the_wire() {
        let handle = detached_handle(1);
        let transport = detached_transport(&handle);
        let id = jsonrpc::Id::Num(75);
        let (response_tx, mut response_rx) = oneshot::channel();
        let mut lease = RequestLease::new(
            handle.pending_requests.clone(),
            handle.outbound.clone(),
            id.clone(),
            response_tx,
        )
        .unwrap();

        transport
            .process_request_response(success(75), "test")
            .unwrap();
        assert_eq!(handle.pending_count(), 1);
        assert!(matches!(
            response_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        assert!(handle.pending_requests.mark_sent(&id));
        transport
            .process_request_response(success(75), "test")
            .unwrap();
        assert_eq!(response_rx.try_recv().unwrap().unwrap(), Value::Null);
        lease.disarm();
        assert_eq!(handle.pending_count(), 0);
    }

    #[tokio::test]
    async fn write_failure_drains_sent_and_queued_requests() {
        let (server_stdout, transport_stdout) = duplex(4096);
        let (transport_stdin, server_stdin) = duplex(1);
        let (_incoming, handle) = Transport::start_with_capacity(
            BufReader::new(transport_stdout),
            BufWriter::new(transport_stdin),
            BufReader::new(tokio::io::empty()),
            Default::default(),
            "test".to_string(),
            4,
        );

        let first_handle = handle.clone();
        let first = tokio::spawn(async move {
            first_handle
                .request(method_call(20, "initialize"), Duration::from_secs(60))
                .await
        });
        let second_handle = handle.clone();
        let second = tokio::spawn(async move {
            second_handle
                .request(method_call(21, "initialize"), Duration::from_secs(60))
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.pending_count() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(server_stdin);

        for request in [first, second] {
            let result = tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(result, Err(Error::StreamClosed)));
        }
        assert_eq!(handle.pending_count(), 0);
        drop(server_stdout);
    }

    #[tokio::test]
    async fn stdout_closure_drains_pending_requests() {
        let (_incoming, handle, server_stdout, _server_stdin) = duplex_transport(4096);
        let request_handle = handle.clone();
        let request = tokio::spawn(async move {
            request_handle
                .request(method_call(30, "initialize"), Duration::from_secs(60))
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.pending_count() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(server_stdout);

        let result = tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(Error::StreamClosed)));
        assert_eq!(handle.pending_count(), 0);
        assert_eq!(
            handle.wait_for_initialization().await,
            InitializationState::Closed
        );
    }

    #[tokio::test]
    async fn multiple_initialization_waiters_observe_wire_completion() {
        let (_incoming, handle, server_stdout, mut server_stdin) = duplex_transport(4096);
        let mut waiters = Vec::new();
        for _ in 0..3 {
            let waiter = handle.clone();
            waiters.push(tokio::spawn(async move {
                waiter.wait_for_initialization().await
            }));
        }
        let initialize_handle = handle.clone();
        let initialize = tokio::spawn(async move { initialize_handle.initialized().await });

        assert!(matches!(
            read_wire_message(&mut server_stdin).await,
            ServerMessage::Call(jsonrpc::Call::Notification(jsonrpc::Notification {
                method,
                ..
            })) if method == lsp::notification::Initialized::METHOD
        ));
        initialize.await.unwrap().unwrap();
        for waiter in waiters {
            assert_eq!(waiter.await.unwrap(), InitializationState::Initialized);
        }
        assert_eq!(handle.pending_count(), 0);
        drop(server_stdout);
    }

    #[tokio::test]
    async fn late_initialization_waiter_observes_terminal_states() {
        let initialized = detached_handle(1);
        assert!(initialized.initialization.initialized());
        assert_eq!(
            initialized.wait_for_initialization().await,
            InitializationState::Initialized
        );
        assert_eq!(initialized.pending_count(), 0);

        let failed = detached_handle(1);
        failed.fail_initialization("initialize failed");
        assert_eq!(
            failed.wait_for_initialization().await,
            InitializationState::Failed(Arc::from("initialize failed"))
        );
        assert_eq!(failed.pending_count(), 0);

        let closed = detached_handle(1);
        closed.close();
        assert_eq!(
            closed.wait_for_initialization().await,
            InitializationState::Closed
        );
        assert_eq!(closed.pending_count(), 0);
    }

    #[tokio::test]
    async fn priority_lane_is_bounded_and_normal_traffic_gets_a_fair_turn() {
        let outbound = OutboundMailbox::new(16, 16);
        outbound
            .send_payload(notification("test/ordered-normal"))
            .unwrap();
        for id in 0..16 {
            outbound
                .send_control(ControlMessage::CancelRequest {
                    id: jsonrpc::Id::Num(id),
                })
                .unwrap();
        }
        assert_eq!(outbound.queue_lengths(), (1, 16));

        let mut priority_streak = 0;
        for _ in 0..OUTBOUND_PRIORITY_BURST {
            assert!(matches!(
                outbound
                    .recv(&mut priority_streak, NormalReceiveMode::Any)
                    .await,
                Some(OutboundMessage::Control(_))
            ));
        }
        assert!(matches!(
            outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Payload(Payload::Notification(
                jsonrpc::Notification { method, .. }
            ))) if method == "test/ordered-normal"
        ));

        let saturated = OutboundMailbox::new(16, 2);
        let mut rejected = 0;
        for id in 0..10 {
            if matches!(
                saturated.send_control(ControlMessage::CancelRequest {
                    id: jsonrpc::Id::Num(id),
                }),
                Err(Error::OutboundControlQueueFull)
            ) {
                rejected += 1;
            }
        }
        assert_eq!(saturated.queue_lengths(), (0, 2));
        assert_eq!(rejected, 8);
        saturated
            .send_control(ControlMessage::Response {
                output: success(99),
                sent: None,
            })
            .unwrap();
        assert_eq!(saturated.queue_lengths(), (0, 2));
        let mut saw_response = false;
        let mut priority_streak = 0;
        for _ in 0..2 {
            saw_response |= matches!(
                saturated
                    .recv(&mut priority_streak, NormalReceiveMode::Any)
                    .await,
                Some(OutboundMessage::Control(ControlMessage::Response { .. }))
            );
        }
        assert!(saw_response);
    }

    #[test]
    fn cancellation_codes_have_specific_low_severity_names() {
        for (code, expected) in [
            (lsp::error_codes::SERVER_CANCELLED, "server_cancelled"),
            (lsp::error_codes::REQUEST_CANCELLED, "request_cancelled"),
            (lsp::error_codes::CONTENT_MODIFIED, "content_modified"),
        ] {
            let error = Error::Rpc(jsonrpc::Error {
                code: jsonrpc::ErrorCode::ServerError(code),
                message: "cancelled".to_string(),
                data: None,
            });
            assert_eq!(cancellation_name(&error), Some(expected));
        }
    }

    #[tokio::test]
    async fn document_notifications_stay_ordered_before_dependent_request() {
        let (_incoming, handle, mut server_stdout, mut server_stdin) = duplex_transport(4096);
        handle
            .send(document_open("file:///ordered.rs", 0, "a"))
            .unwrap();
        handle
            .send(document_change("file:///ordered.rs", 1, "ab"))
            .unwrap();

        let initialize_handle = handle.clone();
        let initialize = tokio::spawn(async move {
            initialize_handle
                .request(method_call(50, "initialize"), Duration::from_secs(10))
                .await
        });
        assert!(matches!(
            read_wire_message(&mut server_stdin).await,
            ServerMessage::Call(jsonrpc::Call::MethodCall(jsonrpc::MethodCall {
                id: jsonrpc::Id::Num(50),
                ..
            }))
        ));

        let dependent_handle = handle.clone();
        let dependent = tokio::spawn(async move {
            dependent_handle
                .request(
                    method_call(51, "textDocument/diagnostic"),
                    Duration::from_secs(10),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.pending_count() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        write_server_output(&mut server_stdout, &success(50)).await;
        initialize.await.unwrap().unwrap();
        handle.initialized().await.unwrap();

        assert!(matches!(
            read_wire_message(&mut server_stdin).await,
            ServerMessage::Call(jsonrpc::Call::Notification(jsonrpc::Notification {
                method,
                ..
            })) if method == lsp::notification::Initialized::METHOD
        ));

        let ServerMessage::Call(jsonrpc::Call::Notification(open)) =
            read_wire_message(&mut server_stdin).await
        else {
            panic!("expected reduced didOpen notification")
        };
        assert_eq!(open.method, lsp::notification::DidOpenTextDocument::METHOD);
        let jsonrpc::Params::Map(params) = open.params else {
            panic!("expected didOpen object params")
        };
        assert_eq!(params["textDocument"]["version"], 1);
        assert_eq!(params["textDocument"]["text"], "ab");

        assert!(matches!(
            read_wire_message(&mut server_stdin).await,
            ServerMessage::Call(jsonrpc::Call::MethodCall(jsonrpc::MethodCall {
                id: jsonrpc::Id::Num(51),
                ..
            }))
        ));
        write_server_output(&mut server_stdout, &success(51)).await;
        dependent.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn notification_saturation_does_not_block_response_dispatch() {
        let (_incoming, handle, mut server_stdout, mut server_stdin) =
            duplex_transport_with_capacities(4096, 4, 1, 1);
        let request_handle = handle.clone();
        let request = tokio::spawn(async move {
            request_handle
                .request(method_call(60, "initialize"), Duration::from_secs(10))
                .await
        });
        assert!(matches!(
            read_wire_message(&mut server_stdin).await,
            ServerMessage::Call(jsonrpc::Call::MethodCall(jsonrpc::MethodCall {
                id: jsonrpc::Id::Num(60),
                ..
            }))
        ));

        for index in 0..32 {
            write_server_call(
                &mut server_stdout,
                jsonrpc::Call::Notification(jsonrpc::Notification {
                    jsonrpc: Some(Version::V2),
                    method: format!("test/flood/{index}"),
                    params: jsonrpc::Params::None,
                }),
            )
            .await;
        }
        write_server_output(&mut server_stdout, &success(60)).await;

        tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .expect("response reader blocked behind notifications")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn inbound_dispatcher_bounds_reliable_work_and_coalesces_diagnostics() {
        let (client_tx, mut incoming) = channel(1);
        let dispatcher =
            InboundDispatcher::start(Default::default(), Arc::from("test"), client_tx, 2);

        dispatcher.send(show_message("blocker")).await.unwrap();
        while dispatcher.queue_len() != 0 {
            tokio::task::yield_now().await;
        }
        dispatcher.send(show_message("held")).await.unwrap();
        while dispatcher.queue_len() != 0 {
            tokio::task::yield_now().await;
        }

        dispatcher
            .send(diagnostics("file:///one.rs", 1))
            .await
            .unwrap();
        dispatcher
            .send(diagnostics("file:///one.rs", 2))
            .await
            .unwrap();
        dispatcher
            .send(diagnostics("file:///one.rs", 1))
            .await
            .unwrap();
        dispatcher
            .send(diagnostics("file:///two.rs", 4))
            .await
            .unwrap();
        assert_eq!(dispatcher.queue_len(), 2);

        let blocked_dispatcher = dispatcher.clone();
        let blocked =
            tokio::spawn(
                async move { blocked_dispatcher.send(show_message("backpressured")).await },
            );
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());

        let mut events = Vec::new();
        for _ in 0..5 {
            let (_, event) = tokio::time::timeout(Duration::from_secs(1), incoming.recv())
                .await
                .expect("inbound dispatcher stalled")
                .expect("inbound dispatcher closed");
            events.push(event);
        }
        blocked.await.unwrap().unwrap();

        assert_eq!(show_message_text(&events[0]), Some("blocker"));
        assert_eq!(show_message_text(&events[1]), Some("held"));
        assert_eq!(diagnostics_version(&events[2]), Some(2));
        assert_eq!(diagnostics_version(&events[3]), Some(4));
        assert_eq!(show_message_text(&events[4]), Some("backpressured"));
    }

    #[tokio::test]
    async fn initialization_failure_closes_transport_and_delivers_exit() {
        let (mut incoming, handle, server_stdout, _server_stdin) =
            duplex_transport_with_capacities(4096, 1, 1, 1);
        handle
            .inbound
            .send(show_message("fill-client-queue"))
            .await
            .unwrap();
        tokio::task::yield_now().await;

        handle.fail_initialization("initialize failed");
        assert_eq!(
            handle.wait_for_initialization().await,
            InitializationState::Failed(Arc::from("initialize failed"))
        );
        assert!(matches!(
            handle.send(notification("test/after-failure")),
            Err(Error::StreamClosed)
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let (_, call) = incoming.recv().await.expect("missing synthetic exit");
                if matches!(call, ServerEvent::Notification(Notification::Exit)) {
                    break;
                }
            }
        })
        .await
        .expect("synthetic exit remained blocked behind a full client queue");
        drop(server_stdout);
    }

    #[tokio::test]
    async fn async_reply_remains_responsive_during_normal_saturation() {
        let handle = detached_handle(1);
        for _ in 0..128 {
            handle.send(notification("test/blocker")).unwrap();
        }
        let reply_handle = handle.clone();
        let reply = tokio::spawn(async move { reply_handle.reply_async(success(40)).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.outbound.queue_lengths().1 != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let mut priority_streak = 0;
        match handle
            .outbound
            .recv(&mut priority_streak, NormalReceiveMode::Any)
            .await
            .unwrap()
        {
            OutboundMessage::Control(ControlMessage::Response {
                sent: Some(sent), ..
            }) => sent.send(()).unwrap(),
            message => panic!("unexpected outbound message: {message:?}"),
        }
        reply.await.unwrap().unwrap();
        assert_eq!(handle.outbound.queue_lengths(), (128, 0));
        assert_eq!(handle.pending_count(), 0);
    }

    #[tokio::test]
    async fn async_reply_uses_bounded_control_backpressure() {
        let handle = detached_handle(1);
        handle
            .outbound
            .send_control(ControlMessage::Response {
                output: success(41),
                sent: None,
            })
            .unwrap();

        let reply_handle = handle.clone();
        let reply = tokio::spawn(async move { reply_handle.reply_async(success(42)).await });
        tokio::task::yield_now().await;
        assert!(!reply.is_finished());
        assert_eq!(handle.outbound.queue_lengths(), (0, 1));

        let mut priority_streak = 0;
        assert!(matches!(
            handle
                .outbound
                .recv(&mut priority_streak, NormalReceiveMode::Any)
                .await,
            Some(OutboundMessage::Control(ControlMessage::Response {
                output: Output::Success(Success {
                    id: jsonrpc::Id::Num(41),
                    ..
                }),
                ..
            }))
        ));
        match handle
            .outbound
            .recv(&mut priority_streak, NormalReceiveMode::Any)
            .await
            .unwrap()
        {
            OutboundMessage::Control(ControlMessage::Response {
                output:
                    Output::Success(Success {
                        id: jsonrpc::Id::Num(42),
                        ..
                    }),
                sent: Some(sent),
            }) => sent.send(()).unwrap(),
            message => panic!("unexpected outbound message: {message:?}"),
        }
        reply.await.unwrap().unwrap();
        assert_eq!(handle.outbound.queue_lengths(), (0, 0));
    }
}
