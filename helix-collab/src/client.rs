use crate::{
    ClientFrame, ConnectCode, Connected, Event, HostFrame, ParticipantInfo, ProtocolError, Request,
    Response, SyncTransferId, TransportError, MAX_BUFFER_SNAPSHOT_BYTES,
    MAX_BUFFER_SNAPSHOT_CHUNK_BYTES, MAX_SYNC_MESSAGE_BYTES, MAX_SYNC_MESSAGE_CHUNK_BYTES,
};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const COMMAND_QUEUE_CAPACITY: usize = 256;
const EVENT_QUEUE_CAPACITY: usize = 1024;
const MAX_IN_FLIGHT_REQUESTS: usize = 128;
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(500);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

type RequestResult = Result<Response, ClientError>;

struct Command {
    request: Request,
    response: oneshot::Sender<RequestResult>,
    canceled: Option<CancellationToken>,
}

#[derive(Clone)]
pub(crate) struct ClientRequestHandle {
    commands: mpsc::Sender<Command>,
}

impl ClientRequestHandle {
    pub(crate) async fn request(&self, request: Request) -> RequestResult {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(Command {
                request,
                response,
                canceled: None,
            })
            .await
            .map_err(|_| ClientError::Closed)?;
        receiver.await.map_err(|_| ClientError::Closed)?
    }

    pub(crate) async fn request_cancellable(
        &self,
        request: Request,
        canceled: CancellationToken,
    ) -> RequestResult {
        let (response, receiver) = oneshot::channel();
        tokio::select! {
            _ = canceled.cancelled() => return Err(ClientError::Canceled),
            result = self.commands.send(Command {
                request,
                response,
                canceled: Some(canceled.clone()),
            }) => {
                result.map_err(|_| ClientError::Closed)?;
            }
        }
        tokio::select! {
            _ = canceled.cancelled() => Err(ClientError::Canceled),
            result = receiver => result.map_err(|_| ClientError::Closed)?,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connected(ParticipantInfo),
    Reconnecting { attempt: u32 },
    Failed(String),
    Closed,
}

/// Bounded collaboration client with an owned connection task.
pub struct Client {
    participant: watch::Receiver<ParticipantInfo>,
    resume: watch::Receiver<ConnectCode>,
    state: watch::Receiver<ConnectionState>,
    commands: mpsc::Sender<Command>,
    snapshot_requests: Mutex<()>,
    sync_requests: Mutex<()>,
    next_sync_transfer: AtomicU64,
    events: mpsc::Receiver<Event>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Client {
    pub async fn connect(code: ConnectCode, name: impl Into<String>) -> Result<Self, ClientError> {
        let name = name.into();
        let connection = Connected::connect(&code, name.clone()).await?;
        let current_participant = connection.participant.clone();
        let current_resume = connection.resume.clone();
        let (participant_tx, participant_rx) = watch::channel(current_participant.clone());
        let (resume_tx, resume_rx) = watch::channel(current_resume.clone());
        let (state_tx, state_rx) = watch::channel(ConnectionState::Connected(current_participant));
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run(
            connection,
            name,
            current_resume,
            commands_rx,
            events_tx,
            participant_tx,
            resume_tx,
            state_tx,
            shutdown_rx,
        ));
        Ok(Self {
            participant: participant_rx,
            resume: resume_rx,
            state: state_rx,
            commands: commands_tx,
            snapshot_requests: Mutex::new(()),
            sync_requests: Mutex::new(()),
            next_sync_transfer: AtomicU64::new(1),
            events: events_rx,
            shutdown: shutdown_tx,
            task: Some(task),
        })
    }

    pub fn participant(&self) -> ParticipantInfo {
        self.participant.borrow().clone()
    }

    pub fn resume_code(&self) -> ConnectCode {
        self.resume.borrow().clone()
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.state.borrow().clone()
    }

    pub fn subscribe_connection_state(&self) -> watch::Receiver<ConnectionState> {
        self.state.clone()
    }

    pub async fn request(&self, request: Request) -> RequestResult {
        if matches!(
            request,
            Request::ContinueBufferSnapshot { .. }
                | Request::StartBufferSync { .. }
                | Request::ContinueBufferSync { .. }
        ) {
            return Err(ClientError::InvalidSnapshot(
                "payload transfers are managed by the collaboration client",
            ));
        }
        if let Request::SyncBuffer {
            buffer,
            epoch,
            message,
        } = request
        {
            if message.len() <= MAX_SYNC_MESSAGE_CHUNK_BYTES {
                return self
                    .request_handle()
                    .request(Request::SyncBuffer {
                        buffer,
                        epoch,
                        message,
                    })
                    .await;
            }
            let _guard = self.sync_requests.lock().await;
            let transfer = self.next_sync_transfer.fetch_add(1, Ordering::Relaxed);
            if transfer == 0 || transfer == u64::MAX {
                return Err(ClientError::ResourceExhausted);
            }
            return upload_sync(
                &self.request_handle(),
                SyncTransferId(transfer),
                buffer,
                epoch,
                message.into_vec(),
            )
            .await;
        }
        if matches!(
            request,
            Request::OpenBuffer { .. } | Request::ReadBuffer { .. }
        ) {
            let _guard = self.snapshot_requests.lock().await;
            let requests = self.request_handle();
            let response = requests.request(request).await?;
            return assemble_snapshot(&requests, response).await;
        }
        self.request_handle().request(request).await
    }

    pub(crate) fn request_handle(&self) -> ClientRequestHandle {
        ClientRequestHandle {
            commands: self.commands.clone(),
        }
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    pub fn try_next_event(&mut self) -> Result<Event, mpsc::error::TryRecvError> {
        self.events.try_recv()
    }

    pub async fn shutdown(mut self) -> Result<(), ClientError> {
        let _ = self.shutdown.send(true);
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await.map_err(ClientError::Task)?;
        Ok(())
    }
}

async fn upload_sync(
    requests: &ClientRequestHandle,
    transfer: SyncTransferId,
    buffer: crate::BufferId,
    epoch: u64,
    message: Vec<u8>,
) -> RequestResult {
    if message.len() > MAX_SYNC_MESSAGE_BYTES {
        return Err(ClientError::InvalidSync(
            "sync message exceeds the protocol limit",
        ));
    }
    let total_bytes = message.len() as u64;
    let mut offset = MAX_SYNC_MESSAGE_CHUNK_BYTES;
    expect_unit(
        requests
            .request(Request::StartBufferSync {
                transfer,
                buffer,
                epoch,
                total_bytes,
                message: message[..offset].to_vec().into(),
            })
            .await?,
    )?;
    while offset < message.len() {
        let end = offset
            .saturating_add(MAX_SYNC_MESSAGE_CHUNK_BYTES)
            .min(message.len());
        expect_unit(
            requests
                .request(Request::ContinueBufferSync {
                    transfer,
                    offset: offset as u64,
                    message: message[offset..end].to_vec().into(),
                })
                .await?,
        )?;
        offset = end;
    }
    Ok(Response::Unit)
}

fn expect_unit(response: Response) -> Result<(), ClientError> {
    if matches!(response, Response::Unit) {
        Ok(())
    } else {
        Err(ClientError::InvalidSync(
            "sync transfer returned another response kind",
        ))
    }
}

async fn assemble_snapshot(requests: &ClientRequestHandle, response: Response) -> RequestResult {
    let Response::Buffer {
        buffer,
        epoch,
        total_bytes,
        snapshot,
        mut continuation,
    } = response
    else {
        return Err(ClientError::InvalidSnapshot(
            "buffer request returned another response kind",
        ));
    };
    let total = usize::try_from(total_bytes)
        .ok()
        .filter(|total| *total <= MAX_BUFFER_SNAPSHOT_BYTES)
        .ok_or(ClientError::InvalidSnapshot(
            "declared snapshot size exceeds the protocol limit",
        ))?;
    let mut snapshot = snapshot.into_vec();
    if snapshot.len() > MAX_BUFFER_SNAPSHOT_CHUNK_BYTES || snapshot.len() > total {
        return Err(ClientError::InvalidSnapshot(
            "initial snapshot chunk has an invalid size",
        ));
    }
    snapshot.reserve_exact(total - snapshot.len());

    while snapshot.len() < total {
        let next = continuation.ok_or(ClientError::InvalidSnapshot(
            "snapshot transfer ended before the declared size",
        ))?;
        if next.offset != snapshot.len() as u64 {
            return Err(ClientError::InvalidSnapshot(
                "snapshot continuation offset is not contiguous",
            ));
        }
        let response = requests
            .request(Request::ContinueBufferSnapshot { continuation: next })
            .await?;
        let Response::BufferSnapshotChunk {
            transfer,
            offset,
            snapshot: chunk,
            continuation: next_continuation,
        } = response
        else {
            return Err(ClientError::InvalidSnapshot(
                "snapshot continuation returned another response kind",
            ));
        };
        if transfer != next.transfer || offset != next.offset {
            return Err(ClientError::InvalidSnapshot(
                "snapshot continuation identity or offset changed",
            ));
        }
        let chunk = chunk.into_vec();
        if chunk.is_empty()
            || chunk.len() > MAX_BUFFER_SNAPSHOT_CHUNK_BYTES
            || chunk.len() > total - snapshot.len()
        {
            return Err(ClientError::InvalidSnapshot(
                "snapshot continuation chunk has an invalid size",
            ));
        }
        snapshot.extend_from_slice(&chunk);
        if let Some(next) = next_continuation {
            if next.transfer != transfer || next.offset != snapshot.len() as u64 {
                return Err(ClientError::InvalidSnapshot(
                    "snapshot continuation is not contiguous",
                ));
            }
        }
        continuation = next_continuation;
    }
    if continuation.is_some() {
        return Err(ClientError::InvalidSnapshot(
            "snapshot transfer continued beyond the declared size",
        ));
    }
    Ok(Response::Buffer {
        buffer,
        epoch,
        total_bytes,
        snapshot: snapshot.into(),
        continuation: None,
    })
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    mut connection: Connected,
    name: String,
    mut resume: ConnectCode,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    participant: watch::Sender<ParticipantInfo>,
    resume_tx: watch::Sender<ConnectCode>,
    state: watch::Sender<ConnectionState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut attempt = 0;
    loop {
        match run_connection(&mut connection, &mut commands, &events, &mut shutdown).await {
            SessionEnd::Shutdown => break,
            SessionEnd::Disconnected(message) => {
                fail_queued(&mut commands, &message);
                attempt += 1;
                let _ = state.send(ConnectionState::Reconnecting { attempt });
            }
        }

        let delay = reconnect_delay(attempt);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
        }
        if *shutdown.borrow() {
            break;
        }

        match Connected::connect(&resume, name.clone()).await {
            Ok(next) => {
                attempt = 0;
                resume = next.resume.clone();
                let _ = resume_tx.send(resume.clone());
                let _ = participant.send(next.participant.clone());
                let _ = state.send(ConnectionState::Connected(next.participant.clone()));
                connection = next;
            }
            Err(error) if reconnectable(&error) => {
                attempt = attempt.saturating_add(1);
                let _ = state.send(ConnectionState::Reconnecting { attempt });
            }
            Err(error) => {
                let message = error.to_string();
                let _ = state.send(ConnectionState::Failed(message.clone()));
                fail_all(&mut commands, &message);
                return;
            }
        }
    }
    fail_all(&mut commands, "collaboration client is closed");
    let _ = state.send(ConnectionState::Closed);
}

enum SessionEnd {
    Shutdown,
    Disconnected(String),
}

struct PendingRequest {
    response: oneshot::Sender<RequestResult>,
    cancel_watch: Option<JoinHandle<()>>,
}

async fn run_connection(
    connection: &mut Connected,
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    shutdown: &mut watch::Receiver<bool>,
) -> SessionEnd {
    let mut next_id = 1_u64;
    let mut pending = HashMap::<u64, PendingRequest>::new();
    let (canceled_tx, mut canceled_rx) = mpsc::channel(MAX_IN_FLIGHT_REQUESTS);
    let result = loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break SessionEnd::Shutdown;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    break SessionEnd::Shutdown;
                };
                if pending.len() >= MAX_IN_FLIGHT_REQUESTS {
                    let _ = command.response.send(Err(ClientError::ResourceExhausted));
                    continue;
                }
                let id = next_id;
                let Some(next) = next_id.checked_add(1) else {
                    let _ = command.response.send(Err(ClientError::ResourceExhausted));
                    continue;
                };
                next_id = next;
                if let Err(error) = connection.sender.send(&ClientFrame::Request {
                    id,
                    request: command.request,
                }).await {
                    let message = error.to_string();
                    let _ = command.response.send(Err(ClientError::Disconnected(message.clone())));
                    break SessionEnd::Disconnected(message);
                }
                let cancel_watch = command.canceled.map(|canceled| {
                    let canceled_tx = canceled_tx.clone();
                    tokio::spawn(async move {
                        canceled.cancelled().await;
                        let _ = canceled_tx.send(id).await;
                    })
                });
                pending.insert(id, PendingRequest {
                    response: command.response,
                    cancel_watch,
                });
            }
            canceled = canceled_rx.recv() => {
                let Some(id) = canceled else {
                    continue;
                };
                if pending.contains_key(&id) {
                    if let Err(error) = connection.sender.send(&ClientFrame::Cancel { id }).await {
                        break SessionEnd::Disconnected(error.to_string());
                    }
                }
            }
            frame = connection.receiver.receive::<HostFrame>() => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => break SessionEnd::Disconnected(error.to_string()),
                };
                match frame {
                    HostFrame::Response { id, result } => {
                        let Some(request) = pending.remove(&id) else {
                            break SessionEnd::Disconnected("host returned an unknown request ID".to_owned());
                        };
                        if let Some(watch) = request.cancel_watch {
                            watch.abort();
                        }
                        let _ = request.response.send(result.map_err(ClientError::Protocol));
                    }
                    HostFrame::Event(event) => {
                        let delivered = if matches!(event, Event::Presence(_)) {
                            events.try_send(event).is_ok()
                        } else {
                            events.send(event).await.is_ok()
                        };
                        if !delivered && events.is_closed() {
                            break SessionEnd::Shutdown;
                        }
                    }
                    HostFrame::Authenticated { .. } | HostFrame::Rejected(_) => {
                        break SessionEnd::Disconnected(
                            "host sent an authentication frame after authentication".to_owned(),
                        );
                    }
                }
            }
        }
    };
    let message = match &result {
        SessionEnd::Shutdown => "collaboration client is closed".to_owned(),
        SessionEnd::Disconnected(message) => message.clone(),
    };
    for (_, request) in pending {
        if let Some(watch) = request.cancel_watch {
            watch.abort();
        }
        let _ = request
            .response
            .send(Err(ClientError::Disconnected(message.clone())));
    }
    connection.sender.close();
    result
}

fn reconnect_delay(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(6);
    let base = INITIAL_RECONNECT_DELAY
        .saturating_mul(1_u32 << exponent)
        .min(MAX_RECONNECT_DELAY);
    let mut entropy = [0_u8; 2];
    let _ = getrandom::fill(&mut entropy);
    let jitter = 800 + u16::from_le_bytes(entropy) as u32 % 401;
    base.saturating_mul(jitter) / 1000
}

fn reconnectable(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Connection(_)
            | TransportError::Connect(_)
            | TransportError::Io(_)
            | TransportError::Write(_)
            | TransportError::EndpointClosed
            | TransportError::ConnectTimeout
            | TransportError::AuthenticationTimeout
    )
}

fn fail_queued(commands: &mut mpsc::Receiver<Command>, message: &str) {
    while let Ok(command) = commands.try_recv() {
        let _ = command
            .response
            .send(Err(ClientError::Disconnected(message.to_owned())));
    }
}

fn fail_all(commands: &mut mpsc::Receiver<Command>, message: &str) {
    while let Ok(command) = commands.try_recv() {
        let _ = command
            .response
            .send(Err(ClientError::Disconnected(message.to_owned())));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Protocol(ProtocolError),
    #[error("collaboration connection was lost: {0}")]
    Disconnected(String),
    #[error("collaboration request limit reached")]
    ResourceExhausted,
    #[error("host returned an invalid collaboration snapshot: {0}")]
    InvalidSnapshot(&'static str),
    #[error("collaboration sync transfer is invalid: {0}")]
    InvalidSync(&'static str),
    #[error("collaboration client is closed")]
    Closed,
    #[error("collaboration request was canceled")]
    Canceled,
    #[error("collaboration client task failed: {0}")]
    Task(#[source] tokio::task::JoinError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BufferId, SnapshotContinuation, SnapshotTransferId};
    use std::sync::Arc;

    #[tokio::test]
    async fn assembles_bounded_snapshot_chunks_contiguously() {
        let total = MAX_BUFFER_SNAPSHOT_CHUNK_BYTES * 2 + 17;
        let bytes = Arc::<[u8]>::from(
            (0..total)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let transfer = SnapshotTransferId(7);
        let (commands, mut command_rx) = mpsc::channel(4);
        let requests = ClientRequestHandle { commands };
        let response_bytes = bytes.clone();
        let responder = tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                let Request::ContinueBufferSnapshot { continuation } = command.request else {
                    panic!("unexpected snapshot request")
                };
                assert_eq!(continuation.transfer, transfer);
                let offset = continuation.offset as usize;
                let end = offset
                    .saturating_add(MAX_BUFFER_SNAPSHOT_CHUNK_BYTES)
                    .min(response_bytes.len());
                let next = (end < response_bytes.len()).then_some(SnapshotContinuation {
                    transfer,
                    offset: end as u64,
                });
                command
                    .response
                    .send(Ok(Response::BufferSnapshotChunk {
                        transfer,
                        offset: offset as u64,
                        snapshot: response_bytes[offset..end].to_vec().into(),
                        continuation: next,
                    }))
                    .unwrap();
                if next.is_none() {
                    break;
                }
            }
        });

        let response = assemble_snapshot(
            &requests,
            Response::Buffer {
                buffer: BufferId(3),
                epoch: 11,
                total_bytes: total as u64,
                snapshot: bytes[..MAX_BUFFER_SNAPSHOT_CHUNK_BYTES].to_vec().into(),
                continuation: Some(SnapshotContinuation {
                    transfer,
                    offset: MAX_BUFFER_SNAPSHOT_CHUNK_BYTES as u64,
                }),
            },
        )
        .await
        .unwrap();
        let Response::Buffer {
            buffer,
            epoch,
            total_bytes,
            snapshot,
            continuation,
        } = response
        else {
            panic!("expected assembled buffer")
        };
        assert_eq!(buffer, BufferId(3));
        assert_eq!(epoch, 11);
        assert_eq!(total_bytes, total as u64);
        assert_eq!(snapshot.as_ref(), bytes.as_ref());
        assert_eq!(continuation, None);
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn uploads_large_sync_messages_in_bounded_chunks() {
        let total = MAX_SYNC_MESSAGE_CHUNK_BYTES * 2 + 17;
        let message = (0..total)
            .map(|index| (index % 239) as u8)
            .collect::<Vec<_>>();
        let expected = message.clone();
        let transfer = SyncTransferId(12);
        let (commands, mut command_rx) = mpsc::channel(4);
        let requests = ClientRequestHandle { commands };
        let responder = tokio::spawn(async move {
            let mut assembled = Vec::with_capacity(total);
            while let Some(command) = command_rx.recv().await {
                match command.request {
                    Request::StartBufferSync {
                        transfer: incoming,
                        buffer,
                        epoch,
                        total_bytes,
                        message,
                    } => {
                        assert_eq!(incoming, transfer);
                        assert_eq!(buffer, BufferId(8));
                        assert_eq!(epoch, 3);
                        assert_eq!(total_bytes, total as u64);
                        assert!(assembled.is_empty());
                        assembled.extend_from_slice(&message);
                    }
                    Request::ContinueBufferSync {
                        transfer: incoming,
                        offset,
                        message,
                    } => {
                        assert_eq!(incoming, transfer);
                        assert_eq!(offset, assembled.len() as u64);
                        assembled.extend_from_slice(&message);
                    }
                    _ => panic!("unexpected sync upload request"),
                }
                command.response.send(Ok(Response::Unit)).unwrap();
                if assembled.len() == total {
                    return assembled;
                }
            }
            assembled
        });

        assert!(matches!(
            upload_sync(&requests, transfer, BufferId(8), 3, message)
                .await
                .unwrap(),
            Response::Unit
        ));
        assert_eq!(responder.await.unwrap(), expected);
    }
}
