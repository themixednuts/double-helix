//! Newline-delimited JSON-RPC transport for ACP.
//!
//! Unlike LSP which uses Content-Length headers, ACP messages are delimited
//! by newlines (`\n`). Each message is a single JSON-RPC 2.0 object on one line.
//! Transport logs expose bounded message metadata at info level; raw wire text
//! is emitted only at trace level and is capped.

use crate::{jsonrpc, AgentId, Error, Result};
use anyhow::Context;
use helix_runtime::{channel, Receiver, Sender};
use log::{error, info, trace, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fmt, io,
    sync::{Arc, Mutex as StdMutex, MutexGuard},
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::oneshot,
};

const OUTBOUND_QUEUE_CAPACITY: usize = 256;
const MAX_ACP_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_STDERR_LINE_BYTES: usize = 64 * 1024;
const MAX_LOG_METADATA_BYTES: usize = 128;
const MAX_TRACE_PAYLOAD_BYTES: usize = 4 * 1024;
const LOG_TRUNCATION_MARKER: &str = "...<truncated>";

#[derive(Debug)]
pub enum Payload {
    Request { value: jsonrpc::MethodCall },
    Notification(jsonrpc::Notification),
    Response(jsonrpc::Output),
}

type DeliveryAcknowledgment = std::result::Result<(), String>;

#[derive(Debug)]
struct OutboundMessage {
    payload: Payload,
    delivered_tx: oneshot::Sender<DeliveryAcknowledgment>,
}

#[derive(Clone, Debug)]
pub(crate) struct OutboundQueue {
    tx: Sender<OutboundMessage>,
    pending_requests: Arc<PendingRequests>,
}

impl OutboundQueue {
    fn bounded(
        pending_requests: Arc<PendingRequests>,
        capacity: usize,
    ) -> (Self, Receiver<OutboundMessage>) {
        let (tx, rx) = channel(capacity);
        (
            Self {
                tx,
                pending_requests,
            },
            rx,
        )
    }

    pub(crate) async fn deliver(&self, payload: Payload) -> Result<()> {
        if self.pending_requests.is_closed() {
            return Err(Error::StreamClosed);
        }

        let (delivered_tx, delivered_rx) = oneshot::channel();
        self.tx
            .send(OutboundMessage {
                payload,
                delivered_tx,
            })
            .await
            .map_err(|_| Error::StreamClosed)?;

        match delivered_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(Error::Other(anyhow::anyhow!(message))),
            Err(_) => Err(Error::StreamClosed),
        }
    }
}

type ResponseSender = Sender<Result<Value>>;

#[derive(Debug, Default)]
pub(crate) struct PendingRequests {
    inner: StdMutex<PendingRequestState>,
}

#[derive(Debug, Default)]
struct PendingRequestState {
    requests: HashMap<jsonrpc::Id, ResponseSender>,
    closed: bool,
}

impl PendingRequests {
    fn lock(&self) -> MutexGuard<'_, PendingRequestState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        id: jsonrpc::Id,
        response_tx: ResponseSender,
    ) -> Option<PendingRequestGuard> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        let replaced = state.requests.insert(id.clone(), response_tx);
        assert!(replaced.is_none(), "duplicate pending ACP request id: {id}");
        Some(PendingRequestGuard {
            id,
            pending: self.clone(),
        })
    }

    fn contains(&self, id: &jsonrpc::Id) -> bool {
        self.lock().requests.contains_key(id)
    }

    fn remove(&self, id: &jsonrpc::Id) -> Option<ResponseSender> {
        self.lock().requests.remove(id)
    }

    fn len(&self) -> usize {
        self.lock().requests.len()
    }

    fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn close(&self) -> Vec<(jsonrpc::Id, ResponseSender)> {
        let mut state = self.lock();
        state.closed = true;
        std::mem::take(&mut state.requests).into_iter().collect()
    }
}

pub(crate) struct PendingRequestGuard {
    id: jsonrpc::Id,
    pending: Arc<PendingRequests>,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        self.pending.remove(&self.id);
    }
}

/// A message received from the agent's stdout.
#[derive(Debug, PartialEq, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum AgentMessage {
    /// A response to one of our requests.
    Output(jsonrpc::Output),
    /// A request or notification from the agent.
    Call(jsonrpc::Call),
}

#[derive(Debug, PartialEq, Eq)]
struct MessageLogMetadata {
    kind: &'static str,
    method: String,
    id: String,
}

impl MessageLogMetadata {
    fn from_agent_message(message: &AgentMessage) -> Self {
        match message {
            AgentMessage::Output(output) => Self::new("response", None, Some(output_id(output))),
            AgentMessage::Call(jsonrpc::Call::MethodCall(call)) => {
                Self::new("request", Some(&call.method), Some(&call.id))
            }
            AgentMessage::Call(jsonrpc::Call::Notification(notification)) => {
                Self::new("notification", Some(&notification.method), None)
            }
            AgentMessage::Call(jsonrpc::Call::Invalid { id }) => {
                Self::new("invalid", None, Some(id))
            }
        }
    }

    fn from_payload(payload: &Payload) -> Self {
        match payload {
            Payload::Request { value } => {
                Self::new("request", Some(&value.method), Some(&value.id))
            }
            Payload::Notification(notification) => {
                Self::new("notification", Some(&notification.method), None)
            }
            Payload::Response(output) => Self::new("response", None, Some(output_id(output))),
        }
    }

    fn stderr() -> Self {
        Self::new("stderr", None, None)
    }

    fn new(kind: &'static str, method: Option<&str>, id: Option<&jsonrpc::Id>) -> Self {
        Self {
            kind,
            method: method.map_or_else(|| "-".to_owned(), bounded_metadata),
            id: id.map_or_else(|| "-".to_owned(), bounded_id),
        }
    }
}

fn output_id(output: &jsonrpc::Output) -> &jsonrpc::Id {
    match output {
        jsonrpc::Output::Success(success) => &success.id,
        jsonrpc::Output::Failure(failure) => &failure.id,
    }
}

fn bounded_id(id: &jsonrpc::Id) -> String {
    match id {
        jsonrpc::Id::Null => "null".to_owned(),
        jsonrpc::Id::Num(value) => value.to_string(),
        jsonrpc::Id::Str(value) => bounded_metadata(value),
    }
}

fn bounded_metadata(value: &str) -> String {
    let content_limit = MAX_LOG_METADATA_BYTES.saturating_sub(LOG_TRUNCATION_MARKER.len());
    let mut output = String::with_capacity(MAX_LOG_METADATA_BYTES.min(value.len()));
    let mut truncated = false;

    for character in value.chars() {
        let escaped_len = character
            .escape_default()
            .map(char::len_utf8)
            .sum::<usize>();
        if output.len().saturating_add(escaped_len) > content_limit {
            truncated = true;
            break;
        }
        output.extend(character.escape_default());
    }

    if truncated {
        output.push_str(LOG_TRUNCATION_MARKER);
    }
    output
}

fn transport_metadata_line(
    agent_name: &str,
    direction: &str,
    metadata: &MessageLogMetadata,
    bytes: usize,
) -> String {
    format!(
        "{} {} kind={} method={} id={} bytes={bytes}",
        bounded_metadata(agent_name),
        direction,
        metadata.kind,
        metadata.method,
        metadata.id
    )
}

struct TracePayload<'a>(&'a str);

impl fmt::Display for TracePayload<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.len() <= MAX_TRACE_PAYLOAD_BYTES {
            return formatter.write_str(self.0);
        }

        let mut boundary = MAX_TRACE_PAYLOAD_BYTES.saturating_sub(LOG_TRUNCATION_MARKER.len());
        while !self.0.is_char_boundary(boundary) {
            boundary -= 1;
        }
        formatter.write_str(&self.0[..boundary])?;
        formatter.write_str(LOG_TRUNCATION_MARKER)
    }
}

async fn read_bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin + Send),
    buffer: &mut Vec<u8>,
    limit: usize,
) -> io::Result<usize> {
    buffer.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buffer.len());
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if consumed > limit.saturating_sub(buffer.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line exceeds {limit} byte limit"),
            ));
        }
        let complete = available[..consumed].ends_with(b"\n");
        buffer.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if complete {
            return Ok(buffer.len());
        }
    }
}

#[derive(Debug)]
pub struct Transport {
    id: AgentId,
    name: String,
    pending_requests: Arc<PendingRequests>,
}

impl Transport {
    pub(crate) fn start(
        agent_stdout: BufReader<ChildStdout>,
        agent_stdin: BufWriter<ChildStdin>,
        agent_stderr: BufReader<ChildStderr>,
        id: AgentId,
        name: String,
        pending_requests: Arc<PendingRequests>,
    ) -> (Receiver<(AgentId, jsonrpc::Call)>, OutboundQueue) {
        let (client_tx, rx) = channel(256);
        let (outbound, client_rx) =
            OutboundQueue::bounded(pending_requests.clone(), OUTBOUND_QUEUE_CAPACITY);

        let transport = Arc::new(Self {
            id,
            name,
            pending_requests,
        });

        tokio::spawn(Self::recv(transport.clone(), agent_stdout, client_tx));
        tokio::spawn(Self::err(transport.clone(), agent_stderr));
        tokio::spawn(Self::send(transport, agent_stdin, client_rx));

        (rx, outbound)
    }

    /// Read a single newline-delimited JSON-RPC message from the agent.
    async fn recv_agent_message(
        reader: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut Vec<u8>,
        agent_name: &str,
    ) -> Result<AgentMessage> {
        loop {
            if read_bounded_line(reader, buffer, MAX_ACP_MESSAGE_BYTES).await? == 0 {
                return Err(Error::StreamClosed);
            }

            let line = std::str::from_utf8(buffer)
                .context("agent message is not valid UTF-8")?
                .trim_end_matches(['\r', '\n']);
            if line.trim().is_empty() {
                continue; // skip blank lines
            }

            trace!(
                "{} <- payload={}",
                bounded_metadata(agent_name),
                TracePayload(line)
            );
            let line_len = line.len();
            let bytes = std::mem::take(buffer);
            let (mut bytes, message) = tokio::task::spawn_blocking(move || {
                let end = bytes
                    .iter()
                    .rposition(|byte| !matches!(byte, b'\r' | b'\n'))
                    .map_or(0, |index| index + 1);
                let message = serde_json::from_slice(&bytes[..end]);
                (bytes, message)
            })
            .await
            .map_err(|error| Error::Other(error.into()))?;
            bytes.clear();
            *buffer = bytes;
            let message = message?;
            let metadata = MessageLogMetadata::from_agent_message(&message);
            info!(
                "{}",
                transport_metadata_line(agent_name, "<-", &metadata, line_len)
            );

            return Ok(message);
        }
    }

    /// Read stderr lines from the agent (for logging).
    async fn recv_agent_error(
        err: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut Vec<u8>,
        agent_name: &str,
    ) -> Result<()> {
        if read_bounded_line(err, buffer, MAX_STDERR_LINE_BYTES).await? == 0 {
            return Err(Error::StreamClosed);
        }
        let line = String::from_utf8_lossy(buffer);
        let line = line.trim_end_matches(['\r', '\n']);
        error!(
            "{}",
            transport_metadata_line(agent_name, "<-", &MessageLogMetadata::stderr(), line.len())
        );
        trace!(
            "{} err <- payload={}",
            bounded_metadata(agent_name),
            TracePayload(line)
        );
        Ok(())
    }

    /// Serialize and send a payload to the agent via stdin.
    async fn send_payload_to_agent(
        &self,
        agent_stdin: &mut (impl AsyncWrite + Unpin),
        payload: Payload,
    ) -> Result<()> {
        if self.pending_requests.is_closed() {
            return Err(Error::StreamClosed);
        }

        let metadata = MessageLogMetadata::from_payload(&payload);
        if let Payload::Request { value } = &payload {
            if !self.pending_requests.contains(&value.id) {
                info!(
                    "{} status=canceled",
                    transport_metadata_line(&self.name, "->", &metadata, 0)
                );
                return Ok(());
            }
        }
        let json = tokio::task::spawn_blocking(move || match payload {
            Payload::Request { value } => serde_json::to_string(&value),
            Payload::Notification(value) => serde_json::to_string(&value),
            Payload::Response(output) => serde_json::to_string(&output),
        })
        .await
        .map_err(|error| Error::Other(error.into()))??;

        info!(
            "{}",
            transport_metadata_line(&self.name, "->", &metadata, json.len())
        );
        trace!(
            "{} -> payload={}",
            bounded_metadata(&self.name),
            TracePayload(&json)
        );

        // ACP uses newline-delimited JSON (no Content-Length headers)
        agent_stdin.write_all(json.as_bytes()).await?;
        agent_stdin.write_all(b"\n").await?;
        agent_stdin.flush().await?;

        Ok(())
    }

    async fn deliver_outbound(
        &self,
        agent_stdin: &mut (impl AsyncWrite + Unpin),
        message: OutboundMessage,
    ) -> bool {
        let result = match self
            .send_payload_to_agent(agent_stdin, message.payload)
            .await
        {
            Ok(()) => Ok(()),
            Err(err) => {
                error!("{} send error: {err:?}", self.name);
                self.close_pending_requests().await;
                Err(err.to_string())
            }
        };
        let delivered = result.is_ok();
        let _ = message.delivered_tx.send(result);
        delivered
    }

    async fn close_pending_requests(&self) {
        for (id, tx) in self.pending_requests.close() {
            if tx.send(Err(Error::StreamClosed)).await.is_err() {
                error!("Could not close pending request (id={id})");
            }
        }
    }

    /// Route an incoming message from the agent.
    async fn process_agent_message(
        &self,
        client_tx: &Sender<(AgentId, jsonrpc::Call)>,
        msg: AgentMessage,
    ) -> Result<()> {
        match msg {
            AgentMessage::Output(output) => self.process_request_response(output).await?,
            AgentMessage::Call(call) => {
                client_tx
                    .send((self.id, call))
                    .await
                    .context("failed to forward agent message")?;
            }
        }
        Ok(())
    }

    /// Match a response to a pending request.
    async fn process_request_response(&self, output: jsonrpc::Output) -> Result<()> {
        let (id, result) = match output {
            jsonrpc::Output::Success(jsonrpc::Success { id, result, .. }) => (id, Ok(result)),
            jsonrpc::Output::Failure(jsonrpc::Failure { id, error, .. }) => {
                error!(
                    "{} <- kind=response-error method=- id={} code={} message={}",
                    bounded_metadata(&self.name),
                    bounded_id(&id),
                    error.code.code(),
                    bounded_metadata(&error.message)
                );
                (id, Err(Error::AgentError(error)))
            }
        };

        if let Some(tx) = self.pending_requests.remove(&id) {
            if tx.send(result).await.is_err() {
                error!("Response channel closed (id={id}), original request likely timed out");
            }
        } else {
            error!("Received response without matching request (id={id})");
        }

        Ok(())
    }

    /// Receive loop: reads messages from agent stdout.
    async fn recv<R>(
        transport: Arc<Self>,
        mut agent_stdout: R,
        client_tx: Sender<(AgentId, jsonrpc::Call)>,
    ) where
        R: AsyncBufRead + Unpin + Send,
    {
        let mut buffer = Vec::new();
        loop {
            match Self::recv_agent_message(&mut agent_stdout, &mut buffer, &transport.name).await {
                Ok(msg) => {
                    if let Err(err) = transport.process_agent_message(&client_tx, msg).await {
                        error!("{} recv error: {err:?}", transport.name);
                        break;
                    }
                }
                Err(Error::StreamClosed) => {
                    info!("{} agent process closed stdout", transport.name);
                    break;
                }
                Err(err) => {
                    error!("{} unexpected error: {err:?}", transport.name);
                    break;
                }
            }
        }

        let pending = transport.pending_requests.len();
        warn!(
            "[acp_transport] stdout receive stopped agent={} pending_requests={}",
            transport.name, pending
        );
        transport.close_pending_requests().await;

        let exit = jsonrpc::Call::Notification(jsonrpc::Notification {
            jsonrpc: None,
            method: "exit".to_string(),
            params: jsonrpc::Params::None,
        });
        warn!(
            "[acp_transport] injecting synthetic exit notification agent={}",
            transport.name
        );
        let _ = client_tx.send((transport.id, exit)).await;
    }

    /// Stderr loop: logs agent stderr output.
    async fn err(transport: Arc<Self>, mut agent_stderr: BufReader<ChildStderr>) {
        let mut buffer = Vec::new();
        while Self::recv_agent_error(&mut agent_stderr, &mut buffer, &transport.name)
            .await
            .is_ok()
        {}
    }

    /// Send loop: writes the single bounded outbound queue to agent stdin in FIFO order.
    async fn send<W>(
        transport: Arc<Self>,
        mut agent_stdin: W,
        mut client_rx: Receiver<OutboundMessage>,
    ) where
        W: AsyncWrite + Unpin,
    {
        while let Some(message) = client_rx.recv().await {
            if !transport.deliver_outbound(&mut agent_stdin, message).await {
                break;
            }
        }
        warn!(
            "[acp_transport] client send channel closed agent={}",
            transport.name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::SlotMap;
    use std::{
        future::Future,
        io,
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    fn test_transport() -> Arc<Transport> {
        let mut ids = SlotMap::<AgentId, ()>::default();
        let id = ids.insert(());
        Arc::new(Transport {
            id,
            name: "test-agent".to_string(),
            pending_requests: Arc::new(PendingRequests::default()),
        })
    }

    fn request(id: jsonrpc::Id) -> Payload {
        Payload::Request {
            value: jsonrpc::MethodCall {
                jsonrpc: Some(jsonrpc::Version::V2),
                id,
                method: "test/request".to_string(),
                params: jsonrpc::Params::None,
            },
        }
    }

    fn response(id: u64) -> Payload {
        Payload::Response(jsonrpc::Output::Success(jsonrpc::Success {
            jsonrpc: Some(jsonrpc::Version::V2),
            id: jsonrpc::Id::Num(id),
            result: serde_json::Value::Null,
        }))
    }

    fn notification(method: &str) -> Payload {
        Payload::Notification(jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: method.to_string(),
            params: jsonrpc::Params::None,
        })
    }

    #[test]
    fn incoming_metadata_redacts_json_payload_fields() {
        let secret = "incoming-super-secret";
        let raw = format!(
            r#"{{"jsonrpc":"2.0","id":"request-7","method":"session/new","params":{{"token":"{secret}"}}}}"#
        );
        let message: AgentMessage = serde_json::from_str(&raw).unwrap();
        let metadata = MessageLogMetadata::from_agent_message(&message);
        let line = transport_metadata_line("test-agent", "<-", &metadata, raw.len());

        assert!(line.contains("kind=request"));
        assert!(line.contains("method=session/new"));
        assert!(line.contains("id=request-7"));
        assert!(line.contains(&format!("bytes={}", raw.len())));
        assert!(!line.contains(secret));
        assert!(!line.contains("params"));
        assert!(!line.contains(&raw));
    }

    #[test]
    fn outbound_metadata_redacts_json_payload_fields() {
        let secret = "outgoing-super-secret";
        let mut params = serde_json::Map::new();
        params.insert("token".to_owned(), Value::String(secret.to_owned()));
        let payload = Payload::Notification(jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: "session/update".to_owned(),
            params: jsonrpc::Params::Map(params),
        });
        let json = match &payload {
            Payload::Notification(notification) => serde_json::to_string(notification).unwrap(),
            Payload::Request { .. } | Payload::Response(_) => unreachable!(),
        };
        let metadata = MessageLogMetadata::from_payload(&payload);
        let line = transport_metadata_line("test-agent", "->", &metadata, json.len());

        assert!(line.contains("kind=notification"));
        assert!(line.contains("method=session/update"));
        assert!(line.contains("id=-"));
        assert!(line.contains(&format!("bytes={}", json.len())));
        assert!(!line.contains(secret));
        assert!(!line.contains("params"));
        assert!(!line.contains(&json));
    }

    #[test]
    fn metadata_fields_are_bounded_and_escape_controls() {
        let method = format!("session/\n{}", "x".repeat(MAX_LOG_METADATA_BYTES * 2));
        let payload = Payload::Notification(jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method,
            params: jsonrpc::Params::None,
        });
        let metadata = MessageLogMetadata::from_payload(&payload);
        let agent_name = format!("agent\n{}", "y".repeat(MAX_LOG_METADATA_BYTES * 2));
        let line = transport_metadata_line(&agent_name, "->", &metadata, 12);

        assert!(metadata.method.len() <= MAX_LOG_METADATA_BYTES);
        assert!(metadata.method.ends_with(LOG_TRUNCATION_MARKER));
        assert!(!metadata.method.contains('\n'));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn trace_payload_is_capped_on_a_utf8_boundary() {
        let payload = format!("{}ésecret-tail", "x".repeat(MAX_TRACE_PAYLOAD_BYTES));
        let rendered = TracePayload(&payload).to_string();

        assert!(rendered.len() <= MAX_TRACE_PAYLOAD_BYTES);
        assert!(rendered.ends_with(LOG_TRUNCATION_MARKER));
        assert!(!rendered.contains("secret-tail"));
        assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn bounded_line_reader_rejects_before_buffer_growth() {
        let mut reader = BufReader::new(&b"123456789\n"[..]);
        let mut buffer = Vec::new();

        let error = read_bounded_line(&mut reader, &mut buffer, 8)
            .await
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(buffer.len() <= 8);
    }

    #[test]
    fn dropping_pending_request_guard_removes_registration() {
        let pending = Arc::new(PendingRequests::default());
        let (response_tx, _response_rx) = channel(1);
        let guard = pending
            .register(jsonrpc::Id::Num(1), response_tx)
            .expect("open pending registry");
        assert_eq!(pending.len(), 1);

        drop(guard);

        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn canceled_queued_request_is_not_written() {
        let transport = test_transport();
        let id = jsonrpc::Id::Num(1);
        let (response_tx, _response_rx) = channel(1);
        let guard = transport
            .pending_requests
            .register(id.clone(), response_tx)
            .expect("open pending registry");
        let payload = request(id);
        drop(guard);

        let (writer, mut reader) = tokio::io::duplex(128);
        let mut writer = BufWriter::new(writer);
        transport
            .send_payload_to_agent(&mut writer, payload)
            .await
            .expect("skip canceled request");

        let mut byte = [0; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(25), reader.read(&mut byte))
                .await
                .is_err(),
            "canceled request was written after its waiter was dropped"
        );
    }

    #[tokio::test]
    async fn malformed_stdout_closes_pending_requests_and_reports_exit() {
        let transport = test_transport();
        let (response_tx, mut response_rx) = channel(1);
        let _guard = transport
            .pending_requests
            .register(jsonrpc::Id::Num(1), response_tx)
            .expect("open pending registry");
        let (client_tx, mut client_rx) = channel(1);

        Transport::recv(
            transport.clone(),
            BufReader::new(&b"not-json\n"[..]),
            client_tx,
        )
        .await;

        assert!(matches!(
            response_rx.recv().await,
            Some(Err(Error::StreamClosed))
        ));
        assert_eq!(transport.pending_requests.len(), 0);
        let (late_tx, _late_rx) = channel(1);
        assert!(transport
            .pending_requests
            .register(jsonrpc::Id::Num(2), late_tx)
            .is_none());
        let (_, call) = client_rx.recv().await.expect("synthetic exit notification");
        assert!(matches!(
            call,
            jsonrpc::Call::Notification(jsonrpc::Notification { method, .. }) if method == "exit"
        ));
    }

    #[tokio::test]
    async fn bounded_outbound_queue_waits_without_dropping_and_preserves_order() {
        let pending = Arc::new(PendingRequests::default());
        let (outbound, mut outbound_rx) = OutboundQueue::bounded(pending, 1);
        let mut first = Box::pin(outbound.deliver(response(1)));
        let mut second = Box::pin(outbound.deliver(notification("test/second")));
        let mut context = Context::from_waker(futures_util::task::noop_waker_ref());

        assert!(matches!(first.as_mut().poll(&mut context), Poll::Pending));
        assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));

        let OutboundMessage {
            payload,
            delivered_tx,
        } = outbound_rx.recv().await.expect("first queued message");
        assert!(matches!(
            payload,
            Payload::Response(jsonrpc::Output::Success(jsonrpc::Success {
                id: jsonrpc::Id::Num(1),
                ..
            }))
        ));
        delivered_tx.send(Ok(())).expect("acknowledge first write");
        first.await.expect("first delivery");

        assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));
        let OutboundMessage {
            payload,
            delivered_tx,
        } = outbound_rx.recv().await.expect("second queued message");
        assert!(matches!(
            payload,
            Payload::Notification(jsonrpc::Notification { method, .. })
                if method == "test/second"
        ));
        delivered_tx.send(Ok(())).expect("acknowledge second write");
        second.await.expect("second delivery");
    }

    #[tokio::test]
    async fn single_writer_writes_responses_and_notifications_in_queue_order() {
        let transport = test_transport();
        let (outbound, outbound_rx) = OutboundQueue::bounded(transport.pending_requests.clone(), 2);
        let (writer, reader) = tokio::io::duplex(1024);
        let writer_task = tokio::spawn(Transport::send(
            transport,
            BufWriter::new(writer),
            outbound_rx,
        ));

        outbound.deliver(response(7)).await.expect("write response");
        outbound
            .deliver(notification("test/after-response"))
            .await
            .expect("write notification");
        drop(outbound);
        writer_task.await.expect("writer task");

        let mut reader = BufReader::new(reader);
        let mut first = String::new();
        let mut second = String::new();
        reader.read_line(&mut first).await.expect("read response");
        reader
            .read_line(&mut second)
            .await
            .expect("read notification");
        let first: serde_json::Value = serde_json::from_str(first.trim()).expect("response JSON");
        let second: serde_json::Value =
            serde_json::from_str(second.trim()).expect("notification JSON");

        assert_eq!(first["id"], 7);
        assert_eq!(second["method"], "test/after-response");
    }

    struct FailingWriter;

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test writer failed",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn request_write_failure_is_acknowledged_and_closes_pending_requests() {
        let transport = test_transport();
        let id = jsonrpc::Id::Num(1);
        let (response_tx, mut response_rx) = channel(1);
        let _guard = transport
            .pending_requests
            .register(id.clone(), response_tx)
            .expect("open pending registry");
        let (outbound, outbound_rx) = OutboundQueue::bounded(transport.pending_requests.clone(), 1);
        let writer_task = tokio::spawn(Transport::send(
            transport.clone(),
            FailingWriter,
            outbound_rx,
        ));

        assert!(matches!(
            outbound.deliver(request(id)).await,
            Err(Error::Other(_))
        ));

        assert!(matches!(
            response_rx.recv().await,
            Some(Err(Error::StreamClosed))
        ));
        assert_eq!(transport.pending_requests.len(), 0);
        assert!(transport.pending_requests.is_closed());
        drop(outbound);
        writer_task.await.expect("writer task");
    }
}
