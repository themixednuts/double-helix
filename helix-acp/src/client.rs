//! ACP agent client.
//!
//! Manages a connection to a single ACP agent subprocess.
//! Follows the same patterns as `helix-lsp::Client`.

use crate::{jsonrpc, methods, transport::Payload, types::*, AgentId, Error, Result};
use helix_runtime::{channel, Receiver};
use log::warn;
use serde::Serialize;
use serde_json::Value;
use slotmap::SlotMap;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    io::{BufReader, BufWriter},
    process::{Child, Command},
    sync::{mpsc, watch, Mutex, OnceCell},
    time::timeout,
};

use crate::transport::{OutboundQueue, PendingRequests, Transport};

/// Receiver for incoming agent requests/notifications from the transport.
pub type IncomingReceiver = Receiver<(AgentId, jsonrpc::Call)>;

#[derive(Clone, Debug)]
enum ProcessOutcome {
    Exited(ExitStatus),
    Failed(ProcessFailure),
}

impl ProcessOutcome {
    fn as_result(&self) -> io::Result<ExitStatus> {
        match self {
            Self::Exited(status) => Ok(*status),
            Self::Failed(error) => Err(error.to_io_error()),
        }
    }
}

#[derive(Clone, Debug)]
struct ProcessFailure {
    kind: io::ErrorKind,
    message: Arc<str>,
}

impl ProcessFailure {
    fn new(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string().into(),
        }
    }

    fn to_io_error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.to_string())
    }
}

#[derive(Clone)]
pub(crate) struct ProcessWaiter {
    outcome_rx: watch::Receiver<Option<ProcessOutcome>>,
}

impl ProcessWaiter {
    pub(crate) async fn wait(&self) -> io::Result<ExitStatus> {
        let mut outcome_rx = self.outcome_rx.clone();
        loop {
            if let Some(outcome) = outcome_rx.borrow().clone() {
                return outcome.as_result();
            }

            if outcome_rx.changed().await.is_err() {
                if let Some(outcome) = outcome_rx.borrow().clone() {
                    return outcome.as_result();
                }
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "process controller stopped without publishing an exit result",
                ));
            }
        }
    }
}

pub(crate) struct ProcessHandle {
    shutdown_tx: mpsc::Sender<()>,
    waiter: ProcessWaiter,
}

impl ProcessHandle {
    pub(crate) fn spawn(mut child: Child, description: String) -> Self {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        let (outcome_tx, outcome_rx) = watch::channel(None);

        tokio::spawn(async move {
            let result = tokio::select! {
                status = child.wait() => status,
                _ = shutdown_rx.recv() => terminate_child(&mut child).await,
            };

            let outcome = match result {
                Ok(status) => {
                    warn!(
                        "[acp_process] child exited process={} status={:?}",
                        description, status
                    );
                    ProcessOutcome::Exited(status)
                }
                Err(error) => {
                    warn!(
                        "[acp_process] child wait failed process={} err={}",
                        description, error
                    );
                    ProcessOutcome::Failed(ProcessFailure::new(error))
                }
            };
            outcome_tx.send_replace(Some(outcome));
        });

        Self {
            shutdown_tx,
            waiter: ProcessWaiter { outcome_rx },
        }
    }

    pub(crate) fn request_shutdown(&self) {
        let _ = self.shutdown_tx.try_send(());
    }

    pub(crate) async fn shutdown(&self) -> io::Result<ExitStatus> {
        self.request_shutdown();
        self.waiter.wait().await
    }

    pub(crate) fn waiter(&self) -> ProcessWaiter {
        self.waiter.clone()
    }
}

async fn terminate_child(child: &mut Child) -> io::Result<ExitStatus> {
    if let Err(kill_error) = child.start_kill() {
        return match child.try_wait() {
            Ok(Some(status)) => Ok(status),
            _ => Err(kill_error),
        };
    }

    child.wait().await
}

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
    /// MCP servers to connect to for each session.
    pub mcp_servers: Vec<McpServer>,
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
            mcp_servers: Vec::new(),
            timeout_secs: 120,
        }
    }
}

/// A connection to a single ACP agent process.
pub struct AcpAgent {
    id: AgentId,
    name: String,
    process: ProcessHandle,
    outbound: OutboundQueue,
    pending_requests: Arc<PendingRequests>,
    request_counter: AtomicU64,
    capabilities: OnceCell<AgentCapabilities>,
    agent_info: OnceCell<Implementation>,
    timeout_secs: u64,
    cwd: PathBuf,
    mcp_servers: Vec<McpServer>,
    /// The current session ID, set after `new_session` or `load_session`.
    session_id: Mutex<Option<SessionId>>,
}

impl AcpAgent {
    /// Spawn an agent process and set up the transport.
    ///
    /// Returns the agent and its incoming request/notification receiver.
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
        let process = ProcessHandle::spawn(process, format!("agent={} id={id:?}", config.command));

        let pending_requests = Arc::new(PendingRequests::default());
        let (incoming_rx, outbound) = Transport::start(
            BufReader::new(stdout),
            BufWriter::new(stdin),
            BufReader::new(stderr),
            id,
            config.command.clone(),
            pending_requests.clone(),
        );

        let agent = Arc::new(Self {
            id,
            name: config.command.clone(),
            process,
            outbound,
            pending_requests,
            request_counter: AtomicU64::new(0),
            capabilities: OnceCell::new(),
            agent_info: OnceCell::new(),
            timeout_secs: config.timeout_secs,
            cwd: config.cwd.clone(),
            mcp_servers: config.mcp_servers.clone(),
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

    /// Terminate and reap the agent process.
    ///
    /// This operation is idempotent. Calls made after natural exit or a prior
    /// shutdown observe the original exit result.
    pub async fn shutdown(&self) -> Result<()> {
        self.process.shutdown().await?;
        Ok(())
    }

    /// Wait for the agent process to exit without terminating it.
    pub async fn wait(&self) -> Result<ExitStatus> {
        self.process.waiter().wait().await.map_err(Into::into)
    }

    pub(crate) fn request_shutdown(&self) {
        self.process.request_shutdown();
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
        let outbound = self.outbound.clone();
        let pending_requests = self.pending_requests.clone();
        let id = self.next_request_id();
        let method_name = method.to_string();

        async move {
            let params = serde_json::to_value(params).map_err(|e| Error::Other(e.into()))?;
            let request = jsonrpc::MethodCall {
                jsonrpc: Some(jsonrpc::Version::V2),
                id: jsonrpc::Id::Num(id),
                method: method_name.clone(),
                params: Self::value_into_params(params),
            };
            let (tx, mut rx) = channel::<Result<Value>>(1);
            let Some(_pending_request) = pending_requests.register(jsonrpc::Id::Num(id), tx) else {
                return Err(Error::StreamClosed);
            };
            outbound
                .deliver(Payload::Request { value: request })
                .await?;
            let response = match timeout(Duration::from_secs(timeout_secs), rx.recv()).await {
                Ok(Some(response)) => response,
                Ok(None) => {
                    warn!(
                        "[acp_transport] response stream closed method={} id={}",
                        method_name, id
                    );
                    return Err(Error::StreamClosed);
                }
                Err(_) => {
                    warn!(
                        "[acp_transport] request timed out method={} id={} timeout_secs={}",
                        method_name, id, timeout_secs
                    );
                    return Err(Error::Timeout(jsonrpc::Id::Num(id)));
                }
            };
            let value = match response {
                Ok(value) => value,
                Err(err) => {
                    warn!(
                        "[acp_transport] request failed method={} id={} err={:?}",
                        method_name, id, err
                    );
                    return Err(err);
                }
            };
            serde_json::from_value(value).map_err(Into::into)
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn notify<R: Serialize>(&self, method: &str, params: R) -> Result<()> {
        let params = serde_json::to_value(params).map_err(|err| Error::Other(err.into()))?;

        let notification = jsonrpc::Notification {
            jsonrpc: Some(jsonrpc::Version::V2),
            method: method.to_string(),
            params: Self::value_into_params(params),
        };

        self.outbound
            .deliver(Payload::Notification(notification))
            .await
    }

    /// Send a JSON-RPC response to an agent request.
    pub async fn reply(&self, id: jsonrpc::Id, result: Value) -> Result<()> {
        let output = jsonrpc::Output::Success(jsonrpc::Success {
            jsonrpc: Some(jsonrpc::Version::V2),
            result,
            id,
        });

        self.outbound.deliver(Payload::Response(output)).await
    }

    /// Send a JSON-RPC error response to an agent request.
    pub async fn reply_error(&self, id: jsonrpc::Id, error: jsonrpc::Error) -> Result<()> {
        let output = jsonrpc::Output::Failure(jsonrpc::Failure {
            jsonrpc: Some(jsonrpc::Version::V2),
            error,
            id,
        });

        self.outbound.deliver(Payload::Response(output)).await
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
        let params = InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(client_capabilities)
            .client_info(client_info);

        let response: InitializeResponse = self
            .call(methods::INITIALIZE, params, self.timeout_secs)
            .await?;

        // Store capabilities
        let _ = self.capabilities.set(response.agent_capabilities.clone());
        if let Some(ref info) = response.agent_info {
            let _ = self.agent_info.set(info.clone());
        }

        Ok(response)
    }

    /// Create a new session and store the session ID.
    pub async fn new_session(&self, cwd: PathBuf) -> Result<NewSessionResponse> {
        let params = NewSessionRequest::new(cwd).mcp_servers(self.mcp_servers.clone());
        let resp: NewSessionResponse = self
            .call(methods::SESSION_NEW, params, self.timeout_secs)
            .await?;
        *self.session_id.lock().await = Some(resp.session_id.clone());
        Ok(resp)
    }

    /// Load an existing session and store the session ID.
    pub async fn load_session(&self, session_id: SessionId) -> Result<LoadSessionResponse> {
        let params = LoadSessionRequest::new(session_id.clone(), self.cwd.clone())
            .mcp_servers(self.mcp_servers.clone());
        let resp: LoadSessionResponse = self
            .call(methods::SESSION_LOAD, params, self.timeout_secs)
            .await?;
        *self.session_id.lock().await = Some(session_id);
        Ok(resp)
    }

    pub fn list_sessions(
        &self,
        cursor: Option<String>,
    ) -> impl Future<Output = Result<ListSessionsResponse>> {
        self.call(
            methods::SESSION_LIST,
            ListSessionsRequest::new().cursor(cursor),
            self.timeout_secs,
        )
    }

    pub fn resume_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
    ) -> impl Future<Output = Result<ResumeSessionResponse>> {
        self.call(
            methods::SESSION_RESUME,
            ResumeSessionRequest::new(session_id, cwd).mcp_servers(self.mcp_servers.clone()),
            self.timeout_secs,
        )
    }

    pub async fn fork_session(&self, session_id: SessionId) -> Result<ForkSessionResponse> {
        let params = ForkSessionRequest::new(session_id, self.cwd.clone())
            .mcp_servers(self.mcp_servers.clone());
        let resp: ForkSessionResponse = self
            .call(methods::SESSION_FORK, params, self.timeout_secs)
            .await?;
        *self.session_id.lock().await = Some(resp.session_id.clone());
        Ok(resp)
    }

    pub fn delete_session(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<DeleteSessionResponse>> {
        self.call(
            methods::SESSION_DELETE,
            DeleteSessionRequest::new(session_id),
            self.timeout_secs,
        )
    }

    pub fn authenticate(
        &self,
        method_id: String,
    ) -> impl Future<Output = Result<AuthenticateResponse>> {
        self.call(
            methods::AUTHENTICATE,
            AuthenticateRequest::new(method_id),
            self.timeout_secs,
        )
    }

    pub fn logout(&self) -> impl Future<Output = Result<LogoutResponse>> {
        self.call(methods::LOGOUT, LogoutRequest::new(), self.timeout_secs)
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
        let params = PromptRequest::new(session_id, prompt);
        // Prompts can take a very long time (agent is doing work)
        self.call(methods::SESSION_PROMPT, params, 600)
    }

    /// Cancel an ongoing prompt turn.
    pub async fn cancel(&self, session_id: SessionId) -> Result<()> {
        self.notify(methods::SESSION_CANCEL, CancelNotification::new(session_id))
            .await
    }

    /// Set the session mode.
    pub fn set_session_mode(
        &self,
        session_id: SessionId,
        mode_id: String,
    ) -> impl Future<Output = Result<SetSessionModeResponse>> {
        let params = SetSessionModeRequest::new(session_id, mode_id);
        self.call(methods::SESSION_SET_MODE, params, self.timeout_secs)
    }

    /// Set a session config option.
    pub fn set_session_config_option(
        &self,
        session_id: SessionId,
        config_id: String,
        value: SessionConfigOptionValue,
    ) -> impl Future<Output = Result<SetSessionConfigOptionResponse>> {
        let params = SetSessionConfigOptionRequest::new(session_id, config_id, value);
        self.call(methods::SESSION_SET_CONFIG, params, self.timeout_secs)
    }

    pub async fn complete_elicitation(&self, elicitation_id: String) -> Result<()> {
        self.notify(
            methods::ELICITATION_COMPLETE,
            CompleteElicitationNotification::new(elicitation_id),
        )
        .await
    }
}

impl Drop for AcpAgent {
    fn drop(&mut self) {
        self.process.request_shutdown();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        fs,
        future::Future,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        task::{Context, Poll},
        thread,
        time::Duration,
    };

    pub(crate) const CHILD_MODE_ENV: &str = "HELIX_ACP_TEST_CHILD_MODE";
    pub(crate) const CHILD_STARTED_ENV: &str = "HELIX_ACP_TEST_CHILD_STARTED";
    pub(crate) const CHILD_LATE_MARKER_ENV: &str = "HELIX_ACP_TEST_CHILD_LATE_MARKER";
    pub(crate) const CHILD_TEST_NAME: &str = "client::tests::lifecycle_child_process";

    static TEST_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn helper_args() -> Vec<String> {
        vec![
            "--exact".to_string(),
            CHILD_TEST_NAME.to_string(),
            "--nocapture".to_string(),
        ]
    }

    pub(crate) fn unique_test_path(label: &str) -> PathBuf {
        let id = TEST_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("helix-acp-{label}-{}-{id}", std::process::id()))
    }

    pub(crate) fn child_agent_config(
        mode: &str,
        started: &Path,
        late_marker: Option<&Path>,
    ) -> AgentConfig {
        let mut env = vec![
            (CHILD_MODE_ENV.to_string(), mode.to_string()),
            (CHILD_STARTED_ENV.to_string(), started.display().to_string()),
        ];
        if let Some(path) = late_marker {
            env.push((
                CHILD_LATE_MARKER_ENV.to_string(),
                path.display().to_string(),
            ));
        }

        AgentConfig {
            command: std::env::current_exe()
                .expect("current test executable")
                .display()
                .to_string(),
            args: helper_args(),
            env,
            cwd: std::env::current_dir().expect("current directory"),
            ..AgentConfig::default()
        }
    }

    pub(crate) async fn wait_for_file(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {}", path.display()));
    }

    #[test]
    fn lifecycle_child_process() {
        let Some(mode) = std::env::var_os(CHILD_MODE_ENV) else {
            return;
        };

        if let Some(path) = std::env::var_os(CHILD_STARTED_ENV) {
            fs::write(path, b"started").expect("write child-started marker");
        }

        if mode == "exit" {
            return;
        }

        if let Some(path) = std::env::var_os(CHILD_LATE_MARKER_ENV) {
            thread::sleep(Duration::from_millis(750));
            fs::write(path, b"still-running").expect("write late child marker");
        }

        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[tokio::test]
    async fn dropping_agent_terminates_its_process() {
        let started = unique_test_path("agent-started");
        let late_marker = unique_test_path("agent-late");
        let config = child_agent_config("hang", &started, Some(&late_marker));

        let (agent, incoming) = AcpAgent::start_standalone(&config).expect("start test agent");
        wait_for_file(&started).await;

        drop(agent);
        drop(incoming);
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert!(
            !late_marker.exists(),
            "agent process survived after its final owner was dropped"
        );
        let _ = fs::remove_file(started);
        let _ = fs::remove_file(late_marker);
    }

    #[tokio::test]
    async fn agent_shutdown_is_idempotent() {
        let started = unique_test_path("agent-shutdown-started");
        let config = child_agent_config("hang", &started, None);
        let (agent, incoming) = AcpAgent::start_standalone(&config).expect("start test agent");
        wait_for_file(&started).await;

        tokio::time::timeout(Duration::from_secs(2), agent.shutdown())
            .await
            .expect("agent shutdown timed out")
            .expect("shut down agent");
        tokio::time::timeout(Duration::from_millis(100), agent.shutdown())
            .await
            .expect("repeated shutdown did not reuse exit result")
            .expect("repeat agent shutdown");
        agent.wait().await.expect("wait for shut down agent");

        drop(incoming);
        let _ = fs::remove_file(started);
    }

    #[tokio::test]
    async fn canceling_shutdown_wait_does_not_cancel_process_termination() {
        let started = unique_test_path("agent-canceled-shutdown-started");
        let config = child_agent_config("hang", &started, None);
        let (agent, incoming) = AcpAgent::start_standalone(&config).expect("start test agent");
        wait_for_file(&started).await;

        let mut shutdown = Box::pin(agent.shutdown());
        let mut context = Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(matches!(
            shutdown.as_mut().poll(&mut context),
            Poll::Pending
        ));
        drop(shutdown);

        tokio::time::timeout(Duration::from_secs(2), agent.wait())
            .await
            .expect("canceled shutdown wait canceled process termination")
            .expect("wait for terminated agent");

        drop(incoming);
        let _ = fs::remove_file(started);
    }
}
