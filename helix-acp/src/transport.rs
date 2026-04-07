//! Newline-delimited JSON-RPC transport for ACP.
//!
//! Unlike LSP which uses Content-Length headers, ACP messages are delimited
//! by newlines (`\n`). Each message is a single JSON-RPC 2.0 object on one line.

use crate::{jsonrpc, AgentId, Error, Result};
use anyhow::Context;
use helix_runtime::{channel, Receiver, Sender};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::{Mutex, Notify},
};

#[derive(Debug)]
pub enum Payload {
    Request {
        chan: Sender<Result<Value>>,
        value: jsonrpc::MethodCall,
    },
    Notification(jsonrpc::Notification),
    Response(jsonrpc::Output),
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

#[derive(Debug)]
pub struct Transport {
    id: AgentId,
    name: String,
    pending_requests: Mutex<HashMap<jsonrpc::Id, Sender<Result<Value>>>>,
}

impl Transport {
    fn payload_label(payload: &Payload) -> String {
        match payload {
            Payload::Request { value, .. } => format!("request:{}:{}", value.method, value.id),
            Payload::Notification(value) => format!("notification:{}", value.method),
            Payload::Response(output) => match output {
                jsonrpc::Output::Success(success) => format!("response:{}", success.id),
                jsonrpc::Output::Failure(failure) => format!("response:{}", failure.id),
            },
        }
    }

    pub fn start(
        agent_stdout: BufReader<ChildStdout>,
        agent_stdin: BufWriter<ChildStdin>,
        agent_stderr: BufReader<ChildStderr>,
        id: AgentId,
        name: String,
    ) -> (Receiver<(AgentId, jsonrpc::Call)>, Sender<Payload>, Arc<Notify>) {
        let (client_tx, rx) = channel(256);
        let (tx, client_rx) = channel(256);
        let notify = Arc::new(Notify::new());

        let transport = Arc::new(Self {
            id,
            name,
            pending_requests: Mutex::new(HashMap::default()),
        });

        tokio::spawn(Self::recv(
            transport.clone(),
            agent_stdout,
            client_tx.clone(),
        ));
        tokio::spawn(Self::err(transport.clone(), agent_stderr));
        tokio::spawn(Self::send(
            transport,
            agent_stdin,
            client_tx,
            client_rx,
            notify.clone(),
        ));

        (rx, tx, notify)
    }

    /// Read a single newline-delimited JSON-RPC message from the agent.
    async fn recv_agent_message(
        reader: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut String,
        agent_name: &str,
    ) -> Result<AgentMessage> {
        loop {
            buffer.clear();
            if reader.read_line(buffer).await? == 0 {
                return Err(Error::StreamClosed);
            }

            let trimmed = buffer.trim();
            if trimmed.is_empty() {
                continue; // skip blank lines
            }

            info!("{agent_name} <- {trimmed}");

            return serde_json::from_str(trimmed).map_err(Into::into);
        }
    }

    /// Read stderr lines from the agent (for logging).
    async fn recv_agent_error(
        err: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut String,
        agent_name: &str,
    ) -> Result<()> {
        buffer.clear();
        if err.read_line(buffer).await? == 0 {
            return Err(Error::StreamClosed);
        }
        error!("{agent_name} err <- {buffer:?}");
        Ok(())
    }

    /// Serialize and send a payload to the agent via stdin.
    async fn send_payload_to_agent(
        &self,
        agent_stdin: &mut BufWriter<ChildStdin>,
        payload: Payload,
    ) -> Result<()> {
        let json = match payload {
            Payload::Request { chan, value } => {
                self.pending_requests
                    .lock()
                    .await
                    .insert(value.id.clone(), chan);
                serde_json::to_string(&value)?
            }
            Payload::Notification(value) => serde_json::to_string(&value)?,
            Payload::Response(output) => serde_json::to_string(&output)?,
        };

        info!("{} -> {json}", self.name);

        // ACP uses newline-delimited JSON (no Content-Length headers)
        agent_stdin.write_all(json.as_bytes()).await?;
        agent_stdin.write_all(b"\n").await?;
        agent_stdin.flush().await?;

        Ok(())
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
                error!("{} <- error: {error}", self.name);
                (id, Err(Error::AgentError(error)))
            }
        };

        if let Some(tx) = self.pending_requests.lock().await.remove(&id) {
            if tx.send(result).await.is_err() {
                error!("Response channel closed (id={id}), original request likely timed out");
            }
        } else {
            error!("Received response without matching request (id={id})");
        }

        Ok(())
    }

    /// Receive loop: reads messages from agent stdout.
    async fn recv(
        transport: Arc<Self>,
        mut agent_stdout: BufReader<ChildStdout>,
        client_tx: Sender<(AgentId, jsonrpc::Call)>,
    ) {
        let mut buffer = String::new();
        loop {
            match Self::recv_agent_message(&mut agent_stdout, &mut buffer, &transport.name).await {
                Ok(msg) => {
                    if let Err(err) = transport.process_agent_message(&client_tx, msg).await {
                        error!("{} recv error: {err:?}", transport.name);
                        break;
                    }
                }
                Err(Error::StreamClosed) => {
                    let pending = transport.pending_requests.lock().await.len();
                    info!("{} agent process closed stdout", transport.name);
                    warn!(
                        "[acp_transport] stdout closed agent={} pending_requests={}",
                        transport.name, pending
                    );

                    // Close all pending requests
                    for (id, tx) in transport.pending_requests.lock().await.drain() {
                        if tx.send(Err(Error::StreamClosed)).await.is_err() {
                            error!("Could not close pending request (id={id})");
                        }
                    }

                    // Inject a synthetic exit notification
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
                    break;
                }
                Err(err) => {
                    error!("{} unexpected error: {err:?}", transport.name);
                    break;
                }
            }
        }
    }

    /// Stderr loop: logs agent stderr output.
    async fn err(transport: Arc<Self>, mut agent_stderr: BufReader<ChildStderr>) {
        let mut buffer = String::new();
        while Self::recv_agent_error(&mut agent_stderr, &mut buffer, &transport.name)
            .await
            .is_ok()
        {}
    }

    /// Send loop: writes queued payloads to agent stdin.
    ///
    /// Messages are buffered until initialization completes (signaled by `initialize_notify`).
    /// Only `initialize` and `cancel` are allowed through before initialization.
    async fn send(
        transport: Arc<Self>,
        mut agent_stdin: BufWriter<ChildStdin>,
        _client_tx: Sender<(AgentId, jsonrpc::Call)>,
        mut client_rx: Receiver<Payload>,
        initialize_notify: Arc<Notify>,
    ) {
        let mut pending_messages: Vec<Payload> = Vec::new();
        let mut is_pending = true;

        fn is_initialize(payload: &Payload) -> bool {
            matches!(
                payload,
                Payload::Request {
                    value: jsonrpc::MethodCall { method, .. },
                    ..
                } if method == crate::methods::INITIALIZE
            )
        }

        loop {
            tokio::select! {
                biased;
                _ = initialize_notify.notified() => {
                    is_pending = false;
                    warn!(
                        "[acp_transport] initialize complete agent={} draining_pending={}",
                        transport.name,
                        pending_messages.len()
                    );

                    // Drain buffered messages
                    for msg in pending_messages.drain(..) {
                        if let Err(err) = transport.send_payload_to_agent(&mut agent_stdin, msg).await {
                            error!("{} send error: {err:?}", transport.name);
                        }
                    }
                }
                msg = client_rx.recv() => {
                    if let Some(msg) = msg {
                        if is_pending && !is_initialize(&msg) {
                            // Buffer non-initialize messages until ready
                            if let Payload::Notification(_) = msg {
                                warn!(
                                    "[acp_transport] dropping pre-init notification agent={}",
                                    transport.name
                                );
                                continue; // drop notifications before init
                            }
                            warn!(
                                "[acp_transport] buffering pre-init payload agent={} payload={}",
                                transport.name,
                                Self::payload_label(&msg)
                            );
                            pending_messages.push(msg);
                        } else if let Err(err) =
                            transport.send_payload_to_agent(&mut agent_stdin, msg).await
                        {
                            error!("{} send error: {err:?}", transport.name);
                        }
                    } else {
                        // Channel closed
                        warn!("[acp_transport] client send channel closed agent={}", transport.name);
                        break;
                    }
                }
            }
        }
    }
}
