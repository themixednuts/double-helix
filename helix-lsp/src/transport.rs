use crate::{
    jsonrpc,
    lsp::{self, notification::Notification as _},
    Error, LanguageServerId, Result,
};
use anyhow::Context;
use helix_runtime::{channel, Receiver, Sender};
use log::{error, info};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
        BufWriter,
    },
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::{Mutex, Notify},
};

#[derive(Debug)]
pub enum Payload {
    Request {
        chan: Sender<Result<Value>>,
        value: jsonrpc::MethodCall,
    },
    CancelRequest {
        id: jsonrpc::Id,
    },
    Notification(jsonrpc::Notification),
    Response(jsonrpc::Output),
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

#[derive(Debug)]
pub struct Transport {
    id: LanguageServerId,
    name: String,
    pending_requests: Mutex<HashMap<jsonrpc::Id, Sender<Result<Value>>>>,
}

impl Transport {
    pub fn start(
        server_stdout: BufReader<ChildStdout>,
        server_stdin: BufWriter<ChildStdin>,
        server_stderr: BufReader<ChildStderr>,
        id: LanguageServerId,
        name: String,
    ) -> (
        Receiver<(LanguageServerId, jsonrpc::Call)>,
        Sender<Payload>,
        Arc<Notify>,
    ) {
        let (client_tx, rx) = channel(256);
        let (tx, client_rx) = channel(256);
        let notify = Arc::new(Notify::new());

        let transport = Self {
            id,
            name,
            pending_requests: Mutex::new(HashMap::default()),
        };

        let transport = Arc::new(transport);

        tokio::spawn(Self::recv(
            transport.clone(),
            server_stdout,
            client_tx.clone(),
        ));
        tokio::spawn(Self::err(transport.clone(), server_stderr));
        tokio::spawn(Self::send(
            transport,
            server_stdin,
            client_tx,
            client_rx,
            notify.clone(),
        ));

        (rx, tx, notify)
    }

    async fn recv_server_message(
        reader: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut String,
        content: &mut Vec<u8>,
        language_server_name: &str,
    ) -> Result<ServerMessage> {
        let mut content_length = None;
        loop {
            buffer.clear();
            if reader.read_line(buffer).await? == 0 {
                return Err(Error::StreamClosed);
            }

            // debug!("<- header {:?}", buffer);

            if buffer == "\r\n" {
                // look for an empty CRLF line
                break;
            }

            let header = buffer.trim();

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
        content.resize(content_length, 0);
        reader.read_exact(content).await?;
        let msg = std::str::from_utf8(content).context("invalid utf8 from server")?;

        info!("{language_server_name} <- {msg}");

        // NOTE: We avoid using `?` here, since it would return early on error
        // and skip clearing `content`. By returning the result directly instead,
        // we ensure `content.clear()` is always called.
        let output = sonic_rs::from_slice(content).map_err(Into::into);

        content.clear();

        output
    }

    async fn recv_server_error(
        err: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut String,
        language_server_name: &str,
    ) -> Result<()> {
        buffer.truncate(0);
        if err.read_line(buffer).await? == 0 {
            return Err(Error::StreamClosed);
        };
        error!("{language_server_name} err <- {buffer:?}");

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
            Payload::Request { chan, value } => {
                self.pending_requests
                    .lock()
                    .await
                    .insert(value.id.clone(), chan);
                serde_json::to_string(&value)?
            }
            Payload::CancelRequest { id } => {
                if self.pending_requests.lock().await.remove(&id).is_none() {
                    log::trace!(
                        "Skipping cancel for language server request that is no longer pending (id={:?})",
                        id
                    );
                    return Ok(());
                }
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
                serde_json::to_string(&notification)?
            }
            Payload::Notification(value) => serde_json::to_string(&value)?,
            Payload::Response(error) => serde_json::to_string(&error)?,
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
        info!("{language_server_name} -> {request}");

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
        client_tx: &Sender<(LanguageServerId, jsonrpc::Call)>,
        msg: ServerMessage,
        language_server_name: &str,
    ) -> Result<()> {
        match msg {
            ServerMessage::Output(output) => {
                self.process_request_response(output, language_server_name)
                    .await?
            }
            ServerMessage::Call(call) => {
                client_tx
                    .send((self.id, call))
                    .await
                    .context("failed to send a message to server")?;
                // let notification = Notification::parse(&method, params);
            }
        };
        Ok(())
    }

    async fn process_request_response(
        &self,
        output: jsonrpc::Output,
        language_server_name: &str,
    ) -> Result<()> {
        let (id, result) = match output {
            jsonrpc::Output::Success(jsonrpc::Success { id, result, .. }) => (id, Ok(result)),
            jsonrpc::Output::Failure(jsonrpc::Failure { id, error, .. }) => (id, Err(error.into())),
        };

        if let Some(tx) = self.pending_requests.lock().await.remove(&id) {
            if let Err(error) = &result {
                error!("{language_server_name} <- {error}");
            }
            match tx.send(result).await {
                Ok(_) => (),
                Err(_) => error!(
                    "Tried sending response into a closed channel (id={:?}), original request likely timed out",
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

    async fn recv(
        transport: Arc<Self>,
        mut server_stdout: BufReader<ChildStdout>,
        client_tx: Sender<(LanguageServerId, jsonrpc::Call)>,
    ) {
        let mut recv_buffer = String::new();
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
                    match transport
                        .process_server_message(&client_tx, msg, &transport.name)
                        .await
                    {
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

                    // Close any outstanding requests.
                    for (id, tx) in transport.pending_requests.lock().await.drain() {
                        match tx.send(Err(Error::StreamClosed)).await {
                            Ok(_) => (),
                            Err(_) => {
                                error!("Could not close request on a closed channel (id={:?})", id)
                            }
                        }
                    }

                    // Hack: inject a terminated notification so we trigger code that needs to happen after exit
                    let notification =
                        ServerMessage::Call(jsonrpc::Call::Notification(jsonrpc::Notification {
                            jsonrpc: None,
                            method: lsp::notification::Exit::METHOD.to_string(),
                            params: jsonrpc::Params::None,
                        }));
                    match transport
                        .process_server_message(&client_tx, notification, &transport.name)
                        .await
                    {
                        Ok(_) => {}
                        Err(err) => {
                            error!("err: <- {:?}", err);
                        }
                    }
                    break;
                }
            }
        }
    }

    async fn err(transport: Arc<Self>, mut server_stderr: BufReader<ChildStderr>) {
        let mut recv_buffer = String::new();
        loop {
            match Self::recv_server_error(&mut server_stderr, &mut recv_buffer, &transport.name)
                .await
            {
                Ok(_) => {}
                Err(err) => {
                    error!("{} err: <- {err:?}", transport.name);
                    break;
                }
            }
        }
    }

    async fn send(
        transport: Arc<Self>,
        mut server_stdin: BufWriter<ChildStdin>,
        client_tx: Sender<(LanguageServerId, jsonrpc::Call)>,
        mut client_rx: Receiver<Payload>,
        initialize_notify: Arc<Notify>,
    ) {
        let mut pending_messages: Vec<Payload> = Vec::new();
        let mut is_pending = true;

        // Determine if a message is allowed to be sent early
        fn is_initialize(payload: &Payload) -> bool {
            use lsp::{
                notification::Initialized,
                request::{Initialize, Request},
            };
            match payload {
                Payload::Request {
                    value: jsonrpc::MethodCall { method, .. },
                    ..
                } if method == Initialize::METHOD => true,
                Payload::Notification(jsonrpc::Notification { method, .. })
                    if method == Initialized::METHOD =>
                {
                    true
                }
                _ => false,
            }
        }

        fn is_shutdown(payload: &Payload) -> bool {
            use lsp::request::{Request, Shutdown};
            matches!(payload, Payload::Request { value: jsonrpc::MethodCall { method, .. }, .. } if method == Shutdown::METHOD)
        }

        // TODO: events that use capabilities need to do the right thing

        loop {
            tokio::select! {
                biased;
                _ = initialize_notify.notified() => { // TODO: notified is technically not cancellation safe
                    // server successfully initialized
                    is_pending = false;

                    // Hack: inject an initialized notification so we trigger code that needs to happen after init
                    let notification = ServerMessage::Call(jsonrpc::Call::Notification(jsonrpc::Notification {
                        jsonrpc: None,

                        method: lsp::notification::Initialized::METHOD.to_string(),
                        params: jsonrpc::Params::None,
                    }));
                    let language_server_name = &transport.name;
                    match transport.process_server_message(&client_tx, notification, language_server_name).await {
                        Ok(_) => {}
                        Err(err) => {
                            error!("{language_server_name} err: <- {err:?}");
                        }
                    }

                    // drain the pending queue and send payloads to server
                    for msg in pending_messages.drain(..) {
                        log::info!("Draining pending message {:?}", msg);
                        match transport.send_payload_to_server(&mut server_stdin, msg).await {
                            Ok(_) => {}
                            Err(err) => {
                                error!("{language_server_name} err: <- {err:?}");
                            }
                        }
                    }
                }
                msg = client_rx.recv() => {
                    if let Some(msg) = msg {
                        if let Payload::CancelRequest { id } = &msg {
                            pending_messages.retain(|payload| {
                                !matches!(
                                    payload,
                                    Payload::Request {
                                        value: jsonrpc::MethodCall { id: pending_id, .. },
                                        ..
                                    } if pending_id == id
                                )
                            });
                            match transport.send_payload_to_server(&mut server_stdin, msg).await {
                                Ok(_) => {}
                                Err(err) => {
                                    error!("{} err: <- {err:?}", transport.name);
                                }
                            }
                            continue;
                        }

                        if is_pending && is_shutdown(&msg) {
                            log::info!("Language server not initialized, shutting down");
                            break;
                        } else if is_pending && !is_initialize(&msg) {
                            // ignore notifications
                            if let Payload::Notification(_) = msg {
                                continue;
                            }

                            log::info!("Language server not initialized, delaying request");
                            pending_messages.push(msg);
                        } else {
                            match transport.send_payload_to_server(&mut server_stdin, msg).await {
                                Ok(_) => {}
                                Err(err) => {
                                    error!("{} err: <- {err:?}", transport.name);
                                }
                            }
                        }
                    } else {
                        // channel closed
                        break;
                    }
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::{Output, Success, Version};
    use tokio::io::{duplex, AsyncReadExt};

    #[tokio::test]
    async fn cancel_request_removes_pending_and_sends_cancel_once() {
        let transport = Transport {
            id: Default::default(),
            name: "test".to_string(),
            pending_requests: Mutex::new(HashMap::default()),
        };
        let id = jsonrpc::Id::Num(7);
        let (tx, _rx) = channel(1);
        transport
            .pending_requests
            .lock()
            .await
            .insert(id.clone(), tx);

        let (client, mut server) = duplex(1024);
        let mut writer = BufWriter::new(client);
        transport
            .send_payload_to_server(&mut writer, Payload::CancelRequest { id: id.clone() })
            .await
            .unwrap();
        transport
            .send_payload_to_server(&mut writer, Payload::CancelRequest { id })
            .await
            .unwrap();
        drop(writer);

        let mut out = String::new();
        server.read_to_string(&mut out).await.unwrap();
        assert_eq!(out.matches("\"method\":\"$/cancelRequest\"").count(), 1);
        assert!(transport.pending_requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn late_response_after_cancel_is_ignored() {
        let transport = Transport {
            id: Default::default(),
            name: "test".to_string(),
            pending_requests: Mutex::new(HashMap::default()),
        };
        let output = Output::Success(Success {
            jsonrpc: Some(Version::V2),
            id: jsonrpc::Id::Num(7),
            result: Value::Null,
        });

        transport
            .process_request_response(output, "test")
            .await
            .unwrap();
    }
}
