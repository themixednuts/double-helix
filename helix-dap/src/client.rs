use crate::{
    registry::DebugAdapterId,
    requests::{DisconnectArguments, TerminateArguments},
    transport::{Payload, Request, Response, Transport},
    Error, Result,
};
use helix_core::syntax::config::{DebugAdapterConfig, DebuggerQuirks};
use helix_dap_types::*;
use helix_runtime::{channel, Receiver, Sender};

use serde_json::Value;

use anyhow::anyhow;
use std::{
    collections::HashMap,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncBufRead, AsyncWrite, BufReader, BufWriter},
    net::TcpStream,
    process::{Child, Command},
    time,
};

#[derive(Debug)]
pub struct Client {
    id: DebugAdapterId,
    _process: Option<Child>,
    server_tx: Sender<Payload>,
    request_counter: Arc<AtomicU64>,
    connection_type: Option<ConnectionType>,
    starting_request_args: Option<Value>,
    /// The socket address of the debugger, if using TCP transport.
    pub socket: Option<SocketAddr>,
    pub caps: Option<DebuggerCapabilities>,
    // thread_id -> frames
    pub stack_frames: HashMap<ThreadId, Vec<StackFrame>>,
    frame_variables: HashMap<usize, FrameVariables>,
    pub thread_states: ThreadStates,
    pub thread_id: Option<ThreadId>,
    /// Currently active frame for the current thread.
    pub active_frame: Option<usize>,
    pub quirks: DebuggerQuirks,
    /// The config which was used to start this debugger.
    pub config: Option<DebugAdapterConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameVariables {
    pub scopes: Vec<Scope>,
    pub variables: Vec<Variable>,
}

impl FrameVariables {
    pub fn variable_value(&self, name: &str, case_sensitive: bool) -> Option<&str> {
        self.variables
            .iter()
            .find(|variable| {
                if case_sensitive {
                    variable.name == name
                } else {
                    variable.name.eq_ignore_ascii_case(name)
                }
            })
            .map(|variable| variable.value.as_str())
    }
}

pub fn should_evaluate_inline_value(in_flight: usize, cap: usize) -> bool {
    in_flight < cap
}

#[derive(Debug, Clone)]
pub struct RequestHandle {
    server_tx: Sender<Payload>,
    request_counter: Arc<AtomicU64>,
}

impl RequestHandle {
    fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn call<R: helix_dap_types::Request>(
        &self,
        arguments: R::Arguments,
    ) -> impl Future<Output = Result<Value>>
    where
        R::Arguments: serde::Serialize,
    {
        let server_tx = self.server_tx.clone();
        let id = self.next_request_id();

        async move {
            use std::time::Duration;
            use tokio::time::timeout;

            let arguments = Some(serde_json::to_value(arguments)?);

            let (callback_tx, mut callback_rx) = channel(1);

            let req = Request {
                back_ch: Some(callback_tx),
                seq: id,
                command: R::COMMAND.to_string(),
                arguments,
            };

            server_tx
                .send(Payload::Request(req))
                .await
                .map_err(|e| Error::Other(e.into()))?;

            timeout(Duration::from_secs(20), callback_rx.recv())
                .await
                .map_err(|_| Error::Timeout(id))?
                .ok_or(Error::StreamClosed)?
                .map(|response| response.body.unwrap_or_default())
        }
    }

    pub async fn request<R: helix_dap_types::Request>(
        &self,
        params: R::Arguments,
    ) -> Result<R::Result>
    where
        R::Arguments: serde::Serialize,
        R::Result: core::fmt::Debug,
    {
        let json = self.call::<R>(params).await?;
        let response = serde_json::from_value(json)?;
        Ok(response)
    }

    pub async fn scopes(&self, frame_id: usize) -> Result<Vec<Scope>> {
        let response = self
            .request::<requests::Scopes>(requests::ScopesArguments { frame_id })
            .await?;
        Ok(response.scopes)
    }

    pub async fn variables(&self, variables_reference: usize) -> Result<Vec<Variable>> {
        let response = self
            .request::<requests::Variables>(requests::VariablesArguments {
                variables_reference,
                filter: None,
                start: None,
                count: None,
                format: None,
            })
            .await?;
        Ok(response.variables)
    }

    pub async fn evaluate(
        &self,
        expression: String,
        frame_id: Option<usize>,
        context: Option<String>,
    ) -> Result<requests::EvaluateResponse> {
        self.request::<requests::Evaluate>(requests::EvaluateArguments {
            expression,
            frame_id,
            context,
            format: None,
        })
        .await
    }
}

impl Client {
    // Spawn a process and communicate with it by either TCP or stdio
    // The returned stream includes the Client ID so consumers can differentiate between multiple clients
    pub async fn process(
        transport: &str,
        command: &str,
        args: Vec<&str>,
        port_arg: Option<&str>,
        id: DebugAdapterId,
    ) -> Result<(Self, Receiver<(DebugAdapterId, Payload)>)> {
        if command.is_empty() {
            return Result::Err(Error::Other(anyhow!("Command not provided")));
        }
        match (transport, port_arg) {
            ("tcp", Some(port_arg)) => Self::tcp_process(command, args, port_arg, id).await,
            ("stdio", _) => Self::stdio(command, args, id),
            _ => Result::Err(Error::Other(anyhow!("Incorrect transport {}", transport))),
        }
    }

    pub fn streams(
        rx: Box<dyn AsyncBufRead + Unpin + Send>,
        tx: Box<dyn AsyncWrite + Unpin + Send>,
        err: Option<Box<dyn AsyncBufRead + Unpin + Send>>,
        id: DebugAdapterId,
        process: Option<Child>,
    ) -> Result<(Self, Receiver<(DebugAdapterId, Payload)>)> {
        let (server_rx, server_tx) = Transport::start(rx, tx, err, id);
        let (client_tx, client_rx) = channel(256);

        let client = Self {
            id,
            _process: process,
            server_tx,
            request_counter: Arc::new(AtomicU64::new(0)),
            caps: None,
            connection_type: None,
            starting_request_args: None,
            socket: None,
            stack_frames: HashMap::new(),
            frame_variables: HashMap::new(),
            thread_states: HashMap::new(),
            thread_id: None,
            active_frame: None,
            quirks: DebuggerQuirks::default(),
            config: None,
        };

        tokio::spawn(Self::recv(id, server_rx, client_tx));

        Ok((client, client_rx))
    }

    pub async fn tcp(
        addr: std::net::SocketAddr,
        id: DebugAdapterId,
    ) -> Result<(Self, Receiver<(DebugAdapterId, Payload)>)> {
        let stream = TcpStream::connect(addr).await?;
        let (rx, tx) = stream.into_split();
        Self::streams(Box::new(BufReader::new(rx)), Box::new(tx), None, id, None)
    }

    pub fn stdio(
        cmd: &str,
        args: Vec<&str>,
        id: DebugAdapterId,
    ) -> Result<(Self, Receiver<(DebugAdapterId, Payload)>)> {
        let cmd = resolve_adapter_command(cmd)?;

        let process = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // make sure the process is reaped on drop
            .kill_on_drop(true)
            .spawn();

        let mut process = process?;

        // TODO: do we need bufreader/writer here? or do we use async wrappers on unblock?
        let writer = BufWriter::new(process.stdin.take().expect("Failed to open stdin"));
        let reader = BufReader::new(process.stdout.take().expect("Failed to open stdout"));
        let stderr = BufReader::new(process.stderr.take().expect("Failed to open stderr"));

        Self::streams(
            Box::new(reader),
            Box::new(writer),
            Some(Box::new(stderr)),
            id,
            Some(process),
        )
    }

    async fn get_port() -> Option<u16> {
        Some(
            tokio::net::TcpListener::bind(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                0,
            ))
            .await
            .ok()?
            .local_addr()
            .ok()?
            .port(),
        )
    }

    pub fn starting_request_args(&self) -> Option<&Value> {
        self.starting_request_args.as_ref()
    }

    pub async fn tcp_process(
        cmd: &str,
        args: Vec<&str>,
        port_format: &str,
        id: DebugAdapterId,
    ) -> Result<(Self, Receiver<(DebugAdapterId, Payload)>)> {
        let port = Self::get_port().await.unwrap();
        let cmd = resolve_adapter_command(cmd)?;

        let process = Command::new(cmd)
            .args(args)
            .args(port_format.replace("{}", &port.to_string()).split(' '))
            // silence messages
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Do not kill debug adapter when leaving, it should exit automatically
            .spawn()?;

        // Wait for adapter to become ready for connection
        time::sleep(time::Duration::from_millis(500)).await;
        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
        let stream = TcpStream::connect(socket).await?;

        let (rx, tx) = stream.into_split();
        let mut result = Self::streams(
            Box::new(BufReader::new(rx)),
            Box::new(tx),
            None,
            id,
            Some(process),
        );

        // Set the socket address for the client
        if let Ok((client, _)) = &mut result {
            client.socket = Some(socket);
        }

        result
    }

    async fn recv(
        id: DebugAdapterId,
        mut server_rx: Receiver<Payload>,
        client_tx: Sender<(DebugAdapterId, Payload)>,
    ) {
        while let Some(msg) = server_rx.recv().await {
            match msg {
                Payload::Event(ev) => {
                    let _ = client_tx.send((id, Payload::Event(ev))).await;
                }
                Payload::Response(_) => unreachable!(),
                Payload::Request(req) => {
                    let _ = client_tx.send((id, Payload::Request(req))).await;
                }
            }
        }
    }

    pub fn id(&self) -> DebugAdapterId {
        self.id
    }

    pub fn connection_type(&self) -> Option<ConnectionType> {
        self.connection_type
    }

    fn next_request_id(&self) -> u64 {
        // > The `seq` for the first message sent by a client or debug adapter
        // > is 1, and for each subsequent message is 1 greater than the
        // > previous message sent by that actor
        // <https://microsoft.github.io/debug-adapter-protocol/specification#Base_Protocol_ProtocolMessage>
        self.request_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn request_handle(&self) -> RequestHandle {
        RequestHandle {
            server_tx: self.server_tx.clone(),
            request_counter: self.request_counter.clone(),
        }
    }

    // Internal, called by specific DAP commands when resuming
    pub fn resume_application(&mut self) {
        if let Some(thread_id) = self.thread_id {
            self.thread_states.insert(thread_id, "running".to_string());
            self.stack_frames.remove(&thread_id);
        }
        self.frame_variables.clear();
        self.active_frame = None;
        self.thread_id = None;
    }

    /// Execute a RPC request on the debugger.
    pub fn call<R: helix_dap_types::Request>(
        &self,
        arguments: R::Arguments,
    ) -> impl Future<Output = Result<Value>>
    where
        R::Arguments: serde::Serialize,
    {
        let server_tx = self.server_tx.clone();
        let id = self.next_request_id();

        async move {
            use std::time::Duration;
            use tokio::time::timeout;

            let arguments = Some(serde_json::to_value(arguments)?);

            let (callback_tx, mut callback_rx) = channel(1);

            let req = Request {
                back_ch: Some(callback_tx),
                seq: id,
                command: R::COMMAND.to_string(),
                arguments,
            };

            server_tx
                .send(Payload::Request(req))
                .await
                .map_err(|e| Error::Other(e.into()))?;

            // TODO: specifiable timeout, delay other calls until initialize success
            timeout(Duration::from_secs(20), callback_rx.recv())
                .await
                .map_err(|_| Error::Timeout(id))? // return Timeout
                .ok_or(Error::StreamClosed)?
                .map(|response| response.body.unwrap_or_default())
            // TODO: check response.success
        }
    }

    pub async fn request<R: helix_dap_types::Request>(
        &self,
        params: R::Arguments,
    ) -> Result<R::Result>
    where
        R::Arguments: serde::Serialize,
        R::Result: core::fmt::Debug, // TODO: temporary
    {
        // a future that resolves into the response
        let json = self.call::<R>(params).await?;
        let response = serde_json::from_value(json)?;
        Ok(response)
    }

    pub fn reply(
        &self,
        request_seq: u64,
        command: &str,
        result: core::result::Result<Value, Error>,
    ) -> impl Future<Output = Result<()>> {
        let server_tx = self.server_tx.clone();
        let command = command.to_string();

        async move {
            let response = match result {
                Ok(result) => Response {
                    request_seq,
                    command,
                    success: true,
                    message: None,
                    body: Some(result),
                },
                Err(error) => Response {
                    request_seq,
                    command,
                    success: false,
                    message: Some(error.to_string()),
                    body: None,
                },
            };

            server_tx
                .send(Payload::Response(response))
                .await
                .map_err(|e| Error::Other(e.into()))?;

            Ok(())
        }
    }

    pub fn capabilities(&self) -> &DebuggerCapabilities {
        self.caps.as_ref().expect("debugger not yet initialized!")
    }

    pub async fn initialize(&mut self, adapter_id: String) -> Result<()> {
        let args = requests::InitializeArguments {
            client_id: Some("hx".to_owned()),
            client_name: Some("helix".to_owned()),
            adapter_id,
            locale: Some("en-us".to_owned()),
            lines_start_at_one: Some(true),
            columns_start_at_one: Some(true),
            path_format: Some("path".to_owned()),
            supports_variable_type: Some(true),
            supports_variable_paging: Some(false),
            supports_run_in_terminal_request: Some(true),
            supports_memory_references: Some(false),
            supports_progress_reporting: Some(false),
            supports_invalidated_event: Some(false),
        };

        let response = self.request::<requests::Initialize>(args).await?;
        self.caps = Some(response);

        Ok(())
    }

    pub fn disconnect(
        &mut self,
        args: Option<DisconnectArguments>,
    ) -> impl Future<Output = Result<Value>> {
        self.connection_type = None;
        self.call::<requests::Disconnect>(args)
    }

    pub fn terminate(
        &mut self,
        args: Option<TerminateArguments>,
    ) -> impl Future<Output = Result<Value>> {
        self.connection_type = None;
        self.call::<requests::Terminate>(args)
    }

    pub fn launch(&mut self, args: serde_json::Value) -> impl Future<Output = Result<Value>> {
        self.connection_type = Some(ConnectionType::Launch);
        self.starting_request_args = Some(args.clone());
        self.call::<requests::Launch>(args)
    }

    pub fn attach(&mut self, args: serde_json::Value) -> impl Future<Output = Result<Value>> {
        self.connection_type = Some(ConnectionType::Attach);
        self.starting_request_args = Some(args.clone());
        self.call::<requests::Attach>(args)
    }

    pub fn restart(&self) -> impl Future<Output = Result<Value>> {
        let args = if let Some(args) = &self.starting_request_args {
            args.clone()
        } else {
            Value::Null
        };
        self.call::<requests::Restart>(args)
    }

    pub async fn set_breakpoints(
        &self,
        file: PathBuf,
        breakpoints: Vec<SourceBreakpoint>,
    ) -> Result<Option<Vec<Breakpoint>>> {
        let args = requests::SetBreakpointsArguments {
            source: Source {
                path: Some(file),
                name: None,
                source_reference: None,
                presentation_hint: None,
                origin: None,
                sources: None,
                adapter_data: None,
                checksums: None,
            },
            breakpoints: Some(breakpoints),
            source_modified: Some(false),
        };

        let response = self.request::<requests::SetBreakpoints>(args).await?;

        Ok(response.breakpoints)
    }

    pub async fn configuration_done(&self) -> Result<()> {
        self.request::<requests::ConfigurationDone>(Some(requests::ConfigurationDoneArguments {}))
            .await
    }

    pub fn continue_thread(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::ContinueArguments { thread_id };

        self.call::<requests::Continue>(args)
    }

    pub async fn stack_trace(
        &self,
        thread_id: ThreadId,
    ) -> Result<(Vec<StackFrame>, Option<usize>)> {
        let args = requests::StackTraceArguments {
            thread_id,
            start_frame: None,
            levels: None,
            format: None,
        };

        let response = self.request::<requests::StackTrace>(args).await?;
        Ok((response.stack_frames, response.total_frames))
    }

    pub fn stack_trace_request(
        &self,
        thread_id: ThreadId,
    ) -> impl Future<Output = Result<(Vec<StackFrame>, Option<usize>)>> {
        let args = requests::StackTraceArguments {
            thread_id,
            start_frame: None,
            levels: None,
            format: None,
        };

        let future = self.call::<requests::StackTrace>(args);
        async move {
            let json = future.await?;
            let response: requests::StackTraceResponse = serde_json::from_value(json)?;
            Ok((response.stack_frames, response.total_frames))
        }
    }

    pub fn threads(&self) -> impl Future<Output = Result<Value>> {
        self.call::<requests::Threads>(Some(requests::ThreadsArguments {}))
    }

    pub async fn scopes(&self, frame_id: usize) -> Result<Vec<Scope>> {
        let args = requests::ScopesArguments { frame_id };

        let response = self.request::<requests::Scopes>(args).await?;
        Ok(response.scopes)
    }

    pub async fn variables(&self, variables_reference: usize) -> Result<Vec<Variable>> {
        let args = requests::VariablesArguments {
            variables_reference,
            filter: None,
            start: None,
            count: None,
            format: None,
        };

        let response = self.request::<requests::Variables>(args).await?;
        Ok(response.variables)
    }

    pub fn step_in(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::StepInArguments {
            thread_id,
            target_id: None,
            granularity: None,
        };

        self.call::<requests::StepIn>(args)
    }

    pub fn step_out(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::StepOutArguments {
            thread_id,
            granularity: None,
        };

        self.call::<requests::StepOut>(args)
    }

    pub fn next(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::NextArguments {
            thread_id,
            granularity: None,
        };

        self.call::<requests::Next>(args)
    }

    pub fn pause(&self, thread_id: ThreadId) -> impl Future<Output = Result<Value>> {
        let args = requests::PauseArguments { thread_id };

        self.call::<requests::Pause>(args)
    }

    pub async fn eval(
        &self,
        expression: String,
        frame_id: Option<usize>,
    ) -> Result<requests::EvaluateResponse> {
        let args = requests::EvaluateArguments {
            expression,
            frame_id,
            context: None,
            format: None,
        };

        self.request::<requests::Evaluate>(args).await
    }

    pub async fn eval_with_context(
        &self,
        expression: String,
        frame_id: Option<usize>,
        context: Option<String>,
    ) -> Result<requests::EvaluateResponse> {
        let args = requests::EvaluateArguments {
            expression,
            frame_id,
            context,
            format: None,
        };

        self.request::<requests::Evaluate>(args).await
    }

    pub fn cache_frame_variables(
        &mut self,
        frame_id: usize,
        scopes: Vec<Scope>,
        variables: Vec<Variable>,
    ) {
        self.frame_variables
            .insert(frame_id, FrameVariables { scopes, variables });
    }

    pub fn frame_variables(&self, frame_id: usize) -> Option<&FrameVariables> {
        self.frame_variables.get(&frame_id)
    }

    pub fn clear_frame_variables(&mut self) {
        self.frame_variables.clear();
    }

    pub fn set_exception_breakpoints(
        &self,
        filters: Vec<String>,
    ) -> impl Future<Output = Result<Value>> {
        let args = requests::SetExceptionBreakpointsArguments { filters };

        self.call::<requests::SetExceptionBreakpoints>(args)
    }

    pub fn current_stack_frame(&self) -> Option<&StackFrame> {
        self.stack_frames
            .get(&self.thread_id?)?
            .get(self.active_frame?)
    }
}

fn resolve_adapter_command(cmd: &str) -> Result<std::path::PathBuf> {
    match helix_pkg::resolve::command(&helix_pkg::Store::open_default(), cmd) {
        Some(resolved) => Ok(resolved.path),
        None => Ok(helix_stdx::env::which(cmd)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variable(name: &str, value: &str) -> Variable {
        Variable {
            name: name.to_string(),
            value: value.to_string(),
            ty: None,
            presentation_hint: None,
            evaluate_name: None,
            variables_reference: 0,
            named_variables: None,
            indexed_variables: None,
            memory_reference: None,
        }
    }

    #[test]
    fn inline_variable_lookup_resolves_from_frame_snapshot() {
        let snapshot = FrameVariables {
            scopes: Vec::new(),
            variables: vec![variable("count", "42")],
        };

        assert_eq!(snapshot.variable_value("count", true), Some("42"));
        assert_eq!(snapshot.variable_value("COUNT", true), None);
        assert_eq!(snapshot.variable_value("COUNT", false), Some("42"));
    }

    #[test]
    fn inline_evaluate_cap_skips_excess_requests() {
        assert!(should_evaluate_inline_value(3, 4));
        assert!(!should_evaluate_inline_value(4, 4));
    }
}
