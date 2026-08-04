use crate::{
    protocol::{
        ErrorCode, ProcessExit, ProcessId, ProcessKind, ProcessOutput, ProcessSpec, ProcessStream,
        RemoteError, ServerEvent, ServerFrame, MAX_ACTIVE_PROCESSES, MAX_PROCESS_ARGUMENTS,
        MAX_PROCESS_ENVIRONMENT, MAX_PROCESS_INPUT_BYTES, MAX_PROCESS_SPEC_BYTES,
    },
    workspace::Workspace,
};
use serde_bytes::ByteBuf;
use std::{collections::HashMap, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::ChildStdin,
    sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;

const PROCESS_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
const PROCESS_INPUT_CAPACITY: usize = 64;

pub(crate) struct ProcessTable {
    handles: Mutex<HashMap<ProcessId, ProcessHandle>>,
    slots: Arc<Semaphore>,
}

struct ProcessHandle {
    input: Option<mpsc::Sender<ByteBuf>>,
    kill: CancellationToken,
    _slot: OwnedSemaphorePermit,
}

impl ProcessTable {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            handles: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_ACTIVE_PROCESSES)),
        })
    }

    pub(crate) async fn start(
        self: &Arc<Self>,
        spec: ProcessSpec,
        workspace: Arc<Workspace>,
        outbound: mpsc::Sender<ServerFrame>,
    ) -> Result<(), RemoteError> {
        if !matches!(spec.kind, ProcessKind::Pipes) {
            return Err(RemoteError::new(
                ErrorCode::CapabilityUnavailable,
                "remote pseudoterminals are unavailable in this server build",
            ));
        }
        validate_process_spec(&spec)?;
        let cwd = workspace.resolve_existing(&spec.cwd).await?;
        let slot = self.slots.clone().try_acquire_owned().map_err(|_| {
            RemoteError::new(ErrorCode::ResourceExhausted, "remote process limit reached")
                .retryable()
        })?;
        let id = spec.process;
        let kill = CancellationToken::new();
        let (input, input_rx) = mpsc::channel(PROCESS_INPUT_CAPACITY);
        {
            let mut handles = self.handles.lock().await;
            if handles.contains_key(&id) {
                return Err(RemoteError::new(
                    ErrorCode::Conflict,
                    "remote process ID is already active",
                ));
            }
            handles.insert(
                id,
                ProcessHandle {
                    input: Some(input),
                    kill: kill.clone(),
                    _slot: slot,
                },
            );
        }
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(cwd)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.handles.lock().await.remove(&id);
                return Err(RemoteError::new(
                    ErrorCode::ProcessUnavailable,
                    format!("failed to start remote process '{}': {error}", spec.program),
                ));
            }
        };
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.start_kill();
            self.handles.lock().await.remove(&id);
            return Err(RemoteError::new(
                ErrorCode::Internal,
                "remote process pipes could not be created",
            ));
        };
        if !self.handles.lock().await.contains_key(&id) {
            let _ = child.start_kill();
            return Err(RemoteError::new(
                ErrorCode::Canceled,
                "remote process start was canceled",
            ));
        }

        let table = self.clone();
        tokio::spawn(async move {
            let input_task = tokio::spawn(forward_input(stdin, input_rx, kill.clone()));
            let stdout_task = tokio::spawn(forward_output(
                id,
                ProcessStream::Stdout,
                stdout,
                outbound.clone(),
            ));
            let stderr_task = tokio::spawn(forward_output(
                id,
                ProcessStream::Stderr,
                stderr,
                outbound.clone(),
            ));
            let status = tokio::select! {
                status = child.wait() => status,
                _ = kill.cancelled() => {
                    let _ = child.kill().await;
                    child.wait().await
                }
            };
            kill.cancel();
            let _ = input_task.await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            table.handles.lock().await.remove(&id);
            let exit = match status {
                Ok(status) => ProcessExit {
                    process: id,
                    code: status.code(),
                    signal: exit_signal(&status),
                },
                Err(error) => {
                    let _ = outbound
                        .send(ServerFrame::Event(ServerEvent::Log(crate::RemoteLog {
                            level: crate::RemoteLogLevel::Error,
                            target: "remote_process".to_owned(),
                            message: format!("failed to wait for remote process {id:?}: {error}"),
                        })))
                        .await;
                    ProcessExit {
                        process: id,
                        code: None,
                        signal: None,
                    }
                }
            };
            let _ = outbound
                .send(ServerFrame::Event(ServerEvent::ProcessExited(exit)))
                .await;
        });
        Ok(())
    }

    pub(crate) async fn enqueue_input(
        &self,
        id: ProcessId,
        bytes: ByteBuf,
    ) -> Result<(), RemoteError> {
        if bytes.len() > MAX_PROCESS_INPUT_BYTES {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                format!("remote process input exceeds {MAX_PROCESS_INPUT_BYTES} bytes"),
            ));
        }
        let (input, kill) = self
            .handles
            .lock()
            .await
            .get(&id)
            .and_then(|handle| {
                handle
                    .input
                    .clone()
                    .map(|input| (input, handle.kill.clone()))
            })
            .ok_or_else(|| {
                RemoteError::new(
                    ErrorCode::ProcessUnavailable,
                    "remote process is not running",
                )
            })?;
        match input.try_send(bytes) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                kill.cancel();
                Err(RemoteError::new(
                    ErrorCode::ResourceExhausted,
                    "remote process input queue is full",
                ))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(RemoteError::new(
                ErrorCode::ProcessUnavailable,
                "remote process input is closed",
            )),
        }
    }

    pub(crate) async fn close_input(&self, id: ProcessId) {
        if let Some(handle) = self.handles.lock().await.get_mut(&id) {
            handle.input.take();
        }
    }

    pub(crate) async fn kill(&self, id: ProcessId) -> Result<(), RemoteError> {
        let kill = self
            .handles
            .lock()
            .await
            .get(&id)
            .map(|handle| handle.kill.clone())
            .ok_or_else(|| {
                RemoteError::new(
                    ErrorCode::ProcessUnavailable,
                    "remote process is not running",
                )
            })?;
        kill.cancel();
        Ok(())
    }

    pub(crate) async fn kill_all(&self) {
        let handles = self
            .handles
            .lock()
            .await
            .values()
            .map(|handle| handle.kill.clone())
            .collect::<Vec<_>>();
        for kill in handles {
            kill.cancel();
        }
    }
}

async fn forward_input(
    mut stdin: ChildStdin,
    mut input: mpsc::Receiver<ByteBuf>,
    canceled: CancellationToken,
) {
    loop {
        let bytes = tokio::select! {
            _ = canceled.cancelled() => break,
            bytes = input.recv() => bytes,
        };
        let Some(bytes) = bytes else {
            break;
        };
        if stdin.write_all(&bytes).await.is_err() {
            canceled.cancel();
            break;
        }
    }
}

fn validate_process_spec(spec: &ProcessSpec) -> Result<(), RemoteError> {
    if spec.program.is_empty() || spec.program.contains('\0') {
        return Err(RemoteError::new(
            ErrorCode::InvalidRequest,
            "remote process program is invalid",
        ));
    }
    if spec.args.len() > MAX_PROCESS_ARGUMENTS {
        return Err(RemoteError::new(
            ErrorCode::InvalidRequest,
            "remote process has too many arguments",
        ));
    }
    if spec.env.len() > MAX_PROCESS_ENVIRONMENT {
        return Err(RemoteError::new(
            ErrorCode::InvalidRequest,
            "remote process has too many environment variables",
        ));
    }

    let mut bytes = spec.program.len();
    for arg in &spec.args {
        if arg.contains('\0') {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "remote process argument contains a NUL byte",
            ));
        }
        bytes = bytes
            .checked_add(arg.len())
            .ok_or_else(process_spec_too_large)?;
    }
    for (key, value) in &spec.env {
        if key.is_empty() || key.contains(['\0', '=']) || value.contains('\0') {
            return Err(RemoteError::new(
                ErrorCode::InvalidRequest,
                "remote process environment is invalid",
            ));
        }
        bytes = bytes
            .checked_add(key.len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .ok_or_else(process_spec_too_large)?;
    }
    if bytes > MAX_PROCESS_SPEC_BYTES {
        return Err(process_spec_too_large());
    }
    Ok(())
}

fn process_spec_too_large() -> RemoteError {
    RemoteError::new(
        ErrorCode::InvalidRequest,
        format!("remote process specification exceeds {MAX_PROCESS_SPEC_BYTES} bytes"),
    )
}

async fn forward_output(
    process: ProcessId,
    stream: ProcessStream,
    mut reader: impl AsyncRead + Unpin,
    outbound: mpsc::Sender<ServerFrame>,
) {
    let mut bytes = vec![0; PROCESS_OUTPUT_CHUNK_BYTES];
    loop {
        match reader.read(&mut bytes).await {
            Ok(0) => break,
            Ok(read) => {
                let event = ServerFrame::Event(ServerEvent::ProcessOutput(ProcessOutput {
                    process,
                    stream,
                    bytes: serde_bytes::ByteBuf::from(bytes[..read].to_vec()),
                }));
                if outbound.send(event).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| signal.to_string())
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}
