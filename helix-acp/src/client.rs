//! ACP agent client.
//!
//! Manages a connection to a single ACP agent subprocess.
//! Follows the same patterns as `helix-lsp::Client`.

use crate::{
    jsonrpc, methods, transport::Payload, types::*, AgentId, Error, Result, PROTOCOL_VERSION,
};
use serde::Serialize;
use serde_json::Value;
use slotmap::SlotMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    io::{BufReader, BufWriter},
    process::{Child, Command},
    sync::{mpsc::UnboundedSender, Mutex, Notify, OnceCell},
    time::timeout,
};

use crate::transport::Transport;

/// Receiver for incoming agent requests/notifications from the transport.
pub type IncomingReceiver = tokio::sync::mpsc::UnboundedReceiver<(AgentId, jsonrpc::Call)>;

/// Configuration for launching an ACP agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// The command to run (e.g., "claude", "codex").
    pub command: String,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Environment variables to set.
    pub env: Vec<(String, String)>,
    /// Working directory for the agent process.
    pub cwd: PathBuf,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: PathBuf::from("."),
            timeout_secs: 120,
        }
    }
}

/// A connection to a single ACP agent process.
pub struct AcpAgent {
    id: AgentId,
    name: String,
    _process: Child,
    server_tx: UnboundedSender<Payload>,
    request_counter: AtomicU64,
    capabilities: OnceCell<AgentCapabilities>,
    agent_info: OnceCell<Implementation>,
    initialize_notify: Arc<Notify>,
    timeout_secs: u64,
    /// The current session ID, set after `new_session` or `load_session`.
    session_id: Mutex<Option<SessionId>>,
}

impl AcpAgent {
    /// Spawn an agent process and set up the transport.
    ///
    /// Returns `(agent, incoming_receiver, initialize_notify)`.
    /// The caller should poll `incoming_receiver` for agent requests/notifications.
    pub fn start(id: AgentId, config: &AgentConfig) -> Result<(Arc<Self>, IncomingReceiver)> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        if !config.cwd.as_os_str().is_empty() {
            cmd.current_dir(&config.cwd);
        }

        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let mut process = cmd.spawn().map_err(|e| {
            Error::Other(anyhow::anyhow!(
                "Failed to spawn agent '{}': {}",
                config.command,
                e
            ))
        })?;

        let stdin = process.stdin.take().expect("stdin was piped");
        let stdout = process.stdout.take().expect("stdout was piped");
        let stderr = process.stderr.take().expect("stderr was piped");

        let (incoming_rx, server_tx, initialize_notify) = Transport::start(
            BufReader::new(stdout),
            BufWriter::new(stdin),
            BufReader::new(stderr),
            id,
            config.command.clone(),
        );

        let agent = Arc::new(Self {
            id,
            name: config.command.clone(),
            _process: process,
            server_tx,
            request_counter: AtomicU64::new(0),
            capabilities: OnceCell::new(),
            agent_info: OnceCell::new(),
            initialize_notify,
            timeout_secs: config.timeout_secs,
            session_id: Mutex::new(None),
        });

        Ok((agent, incoming_rx))
    }

    /// Like `start` but allocates a new agent id internally. Use for one-off clients (e.g. examples).
    pub fn start_standalone(config: &AgentConfig) -> Result<(Arc<Self>, IncomingReceiver)> {
        let mut map = SlotMap::<AgentId, ()>::default();
        let id = map.insert(());
        Self::start(id, config)
    }

    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capabilities(&self) -> Option<&AgentCapabilities> {
        self.capabilities.get()
    }

    pub fn agent_info(&self) -> Option<&Implementation> {
        self.agent_info.get()
    }

    /// Get the current session ID, if any.
    pub async fn session_id(&self) -> Option<SessionId> {
        self.session_id.lock().await.clone()
    }

    fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::Relaxed)
    }

    fn value_into_params(value: Value) -> jsonrpc::Params {
        match value {
            Value::Null => jsonrpc::Params::None,
            Value::Array(vec) => jsonrpc::Params::Array(vec),
            Value::Object(map) => jsonrpc::Params::Map(map),
            _ => jsonrpc::Params::Array(vec![value]),
        }
    }

    /// Send a JSON-RPC request and wait for the response.
    fn call<R: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: R,
        timeout_secs: u64,
    ) -> impl Future<Output = Result<T>> {
        let server_tx = self.server_tx.clone();
        let id = self.next_request_id();

        let rx = serde_json::to_value(params).and_then(|params| {
            let request = jsonrpc::MethodCall {
                jsonrpc: Some(jsonrpc::Version::V2),
                id: jsonrpc::Id::Num(id),
                method: method.to_string(),
                params: Self::value_into_params(params),
            };
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Value>>(1);
            server_tx
                .send(Payload::Request {
                    chan: tx,
                    value: request,
                })
                .map_err(|e| {
                    serde_json::Error::io(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        e.to_string(),
                    ))
                })?;
            Ok(rx)
        });

        async move {
            let mut rx = rx.map_err(|e| Error::Other(e.into()))?;
            let response = timeout(Duration::from_secs(timeout_secs), rx.recv())
                .await
                .map_err(|_| Error::Timeout(jsonrpc::Id::Num(id)))?
                .ok_or(Error::StreamClosed)?;
            let value = response?;
            serde_json::from_value(value).map_err(Into::into)
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    fn notify<R: Serialize>(&self, method: &str, params: R) {
        let params = match serde_json::to_value(params) {
            Ok(params) => params,
            Err(err) => {
                log::error!("Failed to serialize notification params: {err}");
                return;
            }
        };

        let notification = jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: method.to_string(),
            params: Self::value_into_params(params),
        };

        if let Err(err) = self.server_tx.send(Payload::Notification(notification)) {
            log::error!("Failed to send notification: {err}");
        }
    }

    /// Send a JSON-RPC response to an agent request.
    pub fn reply(&self, id: jsonrpc::Id, result: Value) {
        let output = jsonrpc::Output::Success(jsonrpc::Success {
            jsonrpc: Some(jsonrpc::Version::V2),
            result,
            id,
        });

        if let Err(err) = self.server_tx.send(Payload::Response(output)) {
            log::error!("Failed to send response: {err}");
        }
    }

    /// Send a JSON-RPC error response to an agent request.
    pub fn reply_error(&self, id: jsonrpc::Id, error: jsonrpc::Error) {
        let output = jsonrpc::Output::Failure(jsonrpc::Failure {
            jsonrpc: Some(jsonrpc::Version::V2),
            error,
            id,
        });

        if let Err(err) = self.server_tx.send(Payload::Response(output)) {
            log::error!("Failed to send error response: {err}");
        }
    }

    // -----------------------------------------------------------------------
    // ACP Client -> Agent methods
    // -----------------------------------------------------------------------

    /// Initialize the connection with the agent.
    ///
    /// This must be called before any other method. It negotiates the protocol
    /// version and exchanges capabilities.
    pub async fn initialize(
        self: &Arc<Self>,
        client_info: Implementation,
        client_capabilities: ClientCapabilities,
    ) -> Result<InitializeResponse> {
        let params = InitializeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities,
            client_info: Some(client_info),
        };

        let response: InitializeResponse = self
            .call(methods::INITIALIZE, params, self.timeout_secs)
            .await?;

        // Store capabilities
        let _ = self.capabilities.set(response.agent_capabilities.clone());
        if let Some(ref info) = response.agent_info {
            let _ = self.agent_info.set(info.clone());
        }

        // Signal that initialization is complete — buffered messages will be sent
        self.initialize_notify.notify_one();

        Ok(response)
    }

    /// Create a new session and store the session ID.
    pub async fn new_session(&self, cwd: PathBuf) -> Result<NewSessionResponse> {
        let params = NewSessionRequest {
            mcp_servers: Vec::new(),
            cwd,
        };
        let resp: NewSessionResponse = self
            .call(methods::SESSION_NEW, params, self.timeout_secs)
            .await?;
        *self.session_id.lock().await = Some(resp.session_id.clone());
        Ok(resp)
    }

    /// Load an existing session and store the session ID.
    pub async fn load_session(&self, session_id: SessionId) -> Result<LoadSessionResponse> {
        let params = LoadSessionRequest {
            session_id: session_id.clone(),
        };
        let resp: LoadSessionResponse = self
            .call(methods::SESSION_LOAD, params, self.timeout_secs)
            .await?;
        *self.session_id.lock().await = Some(resp.session_id.clone());
        Ok(resp)
    }

    /// Send a prompt to the agent within a session.
    ///
    /// This is a long-running request. The agent will send `session/update`
    /// notifications during processing. The response arrives when the agent
    /// finishes the turn.
    pub fn prompt(
        &self,
        session_id: SessionId,
        prompt: Vec<ContentBlock>,
    ) -> impl Future<Output = Result<PromptResponse>> {
        let params = PromptRequest { session_id, prompt };
        // Prompts can take a very long time (agent is doing work)
        self.call(methods::SESSION_PROMPT, params, 600)
    }

    /// Cancel an ongoing prompt turn.
    pub fn cancel(&self, session_id: SessionId) {
        self.notify(methods::SESSION_CANCEL, CancelNotification { session_id });
    }

    /// Set the session mode.
    pub fn set_session_mode(
        &self,
        session_id: SessionId,
        mode_id: String,
    ) -> impl Future<Output = Result<SetSessionModeResponse>> {
        let params = SetSessionModeRequest {
            session_id,
            mode_id,
        };
        self.call(methods::SESSION_SET_MODE, params, self.timeout_secs)
    }

    /// Set a session config option.
    pub fn set_session_config_option(
        &self,
        session_id: SessionId,
        config_id: String,
        value_id: String,
    ) -> impl Future<Output = Result<SetSessionConfigOptionResponse>> {
        let params = SetSessionConfigOptionRequest {
            session_id,
            config_id,
            value_id,
        };
        self.call(methods::SESSION_SET_CONFIG, params, self.timeout_secs)
    }
}
