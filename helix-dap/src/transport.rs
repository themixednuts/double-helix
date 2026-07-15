use crate::{registry::DebugAdapterId, Error, Result};
use anyhow::Context;
use helix_runtime::{channel, Receiver, Sender};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::{collections::HashMap, fmt::Debug};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
};

const MAX_DAP_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    #[serde(skip)]
    pub back_ch: Option<Sender<Result<Response>>>,
    pub seq: u64,
    pub command: String,
    pub arguments: Option<Value>,
}

#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Serialize)]
pub struct Response {
    // seq is omitted as unused and is not sent by some implementations
    pub request_seq: u64,
    pub success: bool,
    pub command: String,
    pub message: Option<String>,
    pub body: Option<Value>,
}

#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Serialize)]
pub struct Event {
    pub event: String,
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum Payload {
    // type = "event"
    Event(Event),
    // type = "response"
    Response(Response),
    // type = "request"
    Request(Request),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessageError {
    Unhandled,
    Invalid(String),
}

#[derive(Debug)]
pub struct ServerAdapterRequest {
    pub sequence: u64,
    pub command: String,
    pub request: core::result::Result<crate::Request, ServerMessageError>,
}

#[derive(Debug)]
pub struct ServerAdapterEvent {
    pub name: String,
    pub event: core::result::Result<crate::Event, ServerMessageError>,
}

#[derive(Debug)]
pub enum ServerEvent {
    Event(ServerAdapterEvent),
    Request(ServerAdapterRequest),
    UnexpectedResponse(Response),
}

fn server_message_result<T>(result: Result<T>) -> core::result::Result<T, ServerMessageError> {
    result.map_err(|error| match error {
        Error::Unhandled => ServerMessageError::Unhandled,
        error => ServerMessageError::Invalid(error.to_string()),
    })
}

#[derive(Debug)]
pub(crate) struct Transport {
    #[allow(unused)]
    id: DebugAdapterId,
    pending_requests: Mutex<HashMap<u64, Sender<Result<Response>>>>,
}

impl Transport {
    pub(crate) fn start(
        server_stdout: Box<dyn AsyncBufRead + Unpin + Send>,
        server_stdin: Box<dyn AsyncWrite + Unpin + Send>,
        server_stderr: Option<Box<dyn AsyncBufRead + Unpin + Send>>,
        id: DebugAdapterId,
    ) -> (Receiver<ServerEvent>, Sender<Payload>) {
        let (client_tx, rx) = channel(256);
        let (tx, client_rx) = channel(256);

        let transport = Self {
            id,
            pending_requests: Mutex::new(HashMap::default()),
        };

        let transport = Arc::new(transport);

        tokio::spawn(Self::recv(id, transport.clone(), server_stdout, client_tx));
        tokio::spawn(Self::send(transport, server_stdin, client_rx));
        if let Some(stderr) = server_stderr {
            tokio::spawn(Self::err(stderr));
        }

        (rx, tx)
    }

    async fn recv_server_message(
        id: DebugAdapterId,
        reader: &mut Box<dyn AsyncBufRead + Unpin + Send>,
        buffer: &mut String,
        content: &mut Vec<u8>,
    ) -> Result<Payload> {
        let mut content_length = None;
        loop {
            buffer.clear();
            if reader.read_line(buffer).await? == 0 {
                return Err(Error::StreamClosed);
            };

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
        if content_length > MAX_DAP_MESSAGE_BYTES {
            return Err(Error::Other(anyhow::anyhow!(
                "DAP message exceeds {MAX_DAP_MESSAGE_BYTES} byte limit: {content_length}"
            )));
        }
        content.resize(content_length, 0);
        reader.read_exact(content).await?;
        log::debug!("[{id}] <- DAP bytes={content_length}");

        let bytes = std::mem::take(content);
        let (mut bytes, output) = tokio::task::spawn_blocking(move || {
            let output = sonic_rs::from_slice(&bytes).map_err(Into::into);
            (bytes, output)
        })
        .await
        .map_err(|error| Error::Other(error.into()))?;
        bytes.clear();
        *content = bytes;

        output
    }

    async fn recv_server_error(
        err: &mut (impl AsyncBufRead + Unpin + Send),
        buffer: &mut String,
    ) -> Result<()> {
        buffer.truncate(0);
        if err.read_line(buffer).await? == 0 {
            return Err(Error::StreamClosed);
        };
        error!("err <- {}", buffer);

        Ok(())
    }

    async fn send_payload_to_server(
        &self,
        server_stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
        mut payload: Payload,
    ) -> Result<()> {
        if let Payload::Request(request) = &mut payload {
            if let Some(back) = request.back_ch.take() {
                self.pending_requests.lock().await.insert(request.seq, back);
            }
        }
        let json = tokio::task::spawn_blocking(move || serde_json::to_string(&payload))
            .await
            .map_err(|error| Error::Other(error.into()))??;
        self.send_string_to_server(server_stdin, json).await
    }

    async fn send_string_to_server(
        &self,
        server_stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
        request: String,
    ) -> Result<()> {
        log::debug!("[{}] -> DAP bytes={}", self.id, request.len());

        // send the headers
        server_stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", request.len()).as_bytes())
            .await?;

        // send the body
        server_stdin.write_all(request.as_bytes()).await?;

        server_stdin.flush().await?;

        Ok(())
    }

    fn process_response(&self, res: Response) -> Result<Response> {
        if res.success {
            info!(
                "[{}] <- DAP success in response to {}",
                self.id, res.request_seq
            );

            Ok(res)
        } else {
            error!(
                "[{}] <- DAP error {:?} for command #{} {}",
                self.id, res.message, res.request_seq, res.command
            );

            Err(Error::Other(anyhow::format_err!(
                "debug adapter request failed: {}",
                res.message.as_deref().unwrap_or("unknown error")
            )))
        }
    }

    async fn close_pending_requests(&self) {
        let pending = {
            let mut pending = self.pending_requests.lock().await;
            pending.drain().collect::<Vec<_>>()
        };
        for (id, tx) in pending {
            if tx.send(Err(Error::StreamClosed)).await.is_err() {
                log::debug!("debug request receiver closed before transport shutdown id={id}");
            }
        }
    }

    async fn process_server_message(
        &self,
        client_tx: &Sender<ServerEvent>,
        msg: Payload,
    ) -> Result<()> {
        match msg {
            Payload::Response(res) => {
                let request_seq = res.request_seq;
                let tx = self.pending_requests.lock().await.remove(&request_seq);

                match tx {
                    Some(tx) => match tx.send(self.process_response(res)).await {
                        Ok(_) => (),
                        Err(_) => error!(
                            "Tried sending response into a closed channel (id={:?}), original request likely timed out",
                            request_seq
                        ),
                    }
                    None => {
                        warn!("Response to nonexistent request #{}", res.request_seq);
                        let _ = client_tx.send(ServerEvent::UnexpectedResponse(res)).await;
                    }
                }

                Ok(())
            }
            Payload::Request(Request {
                seq,
                command,
                arguments,
                ..
            }) => {
                info!("[{}] <- DAP request {} #{}", self.id, command, seq);
                let request = server_message_result(crate::Request::parse(&command, arguments));
                let _ = client_tx
                    .send(ServerEvent::Request(ServerAdapterRequest {
                        sequence: seq,
                        command,
                        request,
                    }))
                    .await;
                Ok(())
            }
            Payload::Event(event) => {
                log::debug!("[{}] <- DAP event {}", self.id, event.event);
                let parsed = server_message_result(crate::Event::parse(&event.event, event.body));
                let _ = client_tx
                    .send(ServerEvent::Event(ServerAdapterEvent {
                        name: event.event,
                        event: parsed,
                    }))
                    .await;
                Ok(())
            }
        }
    }

    async fn recv(
        id: DebugAdapterId,
        transport: Arc<Self>,
        mut server_stdout: Box<dyn AsyncBufRead + Unpin + Send>,
        client_tx: Sender<ServerEvent>,
    ) {
        let mut recv_buffer = String::new();
        let mut content_buffer = Vec::new();
        loop {
            match Self::recv_server_message(
                id,
                &mut server_stdout,
                &mut recv_buffer,
                &mut content_buffer,
            )
            .await
            {
                Ok(msg) => match transport.process_server_message(&client_tx, msg).await {
                    Ok(_) => (),
                    Err(err) => {
                        error!(" [{id}] err: <- {err:?}");
                        break;
                    }
                },
                Err(err) => {
                    if !matches!(err, Error::StreamClosed) {
                        error!("Exiting after unexpected error: {err:?}");
                    }

                    transport.close_pending_requests().await;
                    break;
                }
            }
        }
    }

    async fn send_inner(
        transport: Arc<Self>,
        mut server_stdin: Box<dyn AsyncWrite + Unpin + Send>,
        mut client_rx: Receiver<Payload>,
    ) -> Result<()> {
        while let Some(payload) = client_rx.recv().await {
            transport
                .send_payload_to_server(&mut server_stdin, payload)
                .await?;
        }
        Ok(())
    }

    async fn send(
        transport: Arc<Self>,
        server_stdin: Box<dyn AsyncWrite + Unpin + Send>,
        client_rx: Receiver<Payload>,
    ) {
        if let Err(err) = Self::send_inner(transport.clone(), server_stdin, client_rx).await {
            error!("err: <- {:?}", err);
            transport.close_pending_requests().await;
        }
    }

    async fn err(mut server_stderr: Box<dyn AsyncBufRead + Unpin + Send>) {
        let mut recv_buffer = String::new();
        loop {
            match Self::recv_server_error(&mut server_stderr, &mut recv_buffer).await {
                Ok(_) => {}
                Err(err) => {
                    error!("err: <- {:?}", err);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncWriteExt, BufReader};

    fn test_transport() -> Transport {
        Transport {
            id: DebugAdapterId::default(),
            pending_requests: Mutex::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn transport_decodes_adapter_events_before_client_delivery() {
        let transport = test_transport();
        let (tx, mut rx) = channel(1);

        transport
            .process_server_message(
                &tx,
                Payload::Event(Event {
                    event: "output".to_owned(),
                    body: Some(serde_json::json!({
                        "category": "stdout",
                        "output": "ready\n"
                    })),
                }),
            )
            .await
            .unwrap();

        let ServerEvent::Event(event) = rx.recv().await.unwrap() else {
            panic!("expected typed adapter event");
        };
        assert_eq!(event.name, "output");
        assert!(matches!(event.event, Ok(crate::Event::Output(_))));
    }

    #[tokio::test]
    async fn transport_classifies_unknown_adapter_events() {
        let transport = test_transport();
        let (tx, mut rx) = channel(1);

        transport
            .process_server_message(
                &tx,
                Payload::Event(Event {
                    event: "vendor/custom".to_owned(),
                    body: None,
                }),
            )
            .await
            .unwrap();

        let ServerEvent::Event(event) = rx.recv().await.unwrap() else {
            panic!("expected typed adapter event");
        };
        assert!(matches!(event.event, Err(ServerMessageError::Unhandled)));
    }

    #[tokio::test]
    async fn closed_adapter_stdout_terminates_reader_without_spinning() {
        let (server_stdout, transport_stdout) = duplex(64);
        drop(server_stdout);
        let (client_tx, _client_rx) = channel(1);

        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            Transport::recv(
                DebugAdapterId::default(),
                Arc::new(test_transport()),
                Box::new(BufReader::new(transport_stdout)),
                client_tx,
            ),
        )
        .await
        .expect("closed adapter reader must terminate");
    }

    #[tokio::test]
    async fn oversized_adapter_message_is_rejected_before_allocation() {
        let (mut server_stdout, transport_stdout) = duplex(128);
        server_stdout
            .write_all(format!("Content-Length: {}\r\n\r\n", MAX_DAP_MESSAGE_BYTES + 1).as_bytes())
            .await
            .unwrap();
        let mut reader: Box<dyn AsyncBufRead + Unpin + Send> =
            Box::new(BufReader::new(transport_stdout));
        let mut header = String::new();
        let mut content = Vec::new();

        let error = Transport::recv_server_message(
            DebugAdapterId::default(),
            &mut reader,
            &mut header,
            &mut content,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("exceeds"));
        assert!(content.is_empty());
    }
}
