//! Terminal manager for ACP agent terminal requests.
//!
//! Spawns and tracks child processes on behalf of ACP agents.

use crate::{
    client::{ProcessHandle, ProcessWaiter},
    types::*,
};
use std::collections::HashMap;
use std::io;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;

const DEFAULT_OUTPUT_BYTE_LIMIT: u64 = 1_000_000;
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

struct ManagedTerminal {
    session_id: SessionId,
    output: Arc<Mutex<OutputBuffer>>,
    process: ProcessHandle,
    outcome_rx: watch::Receiver<Option<TerminalOutcome>>,
}

#[derive(Clone, Debug)]
struct TerminalExitResult {
    exit_code: Option<u32>,
    signal: Option<String>,
}

#[derive(Clone, Debug)]
enum TerminalOutcome {
    Exited(TerminalExitResult),
    Failed(Arc<str>),
}

impl TerminalOutcome {
    fn as_result(&self) -> anyhow::Result<TerminalExitResult> {
        match self {
            Self::Exited(result) => Ok(result.clone()),
            Self::Failed(message) => Err(anyhow::anyhow!(message.to_string())),
        }
    }
}

struct OutputBuffer {
    output: String,
    byte_limit: usize,
    truncated: bool,
}

impl OutputBuffer {
    fn new(byte_limit: u64) -> Self {
        Self {
            output: String::new(),
            byte_limit: byte_limit.min(usize::MAX as u64) as usize,
            truncated: false,
        }
    }

    fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        self.output.push_str(text);
        if self.output.len() <= self.byte_limit {
            return;
        }

        self.truncated = true;
        let mut keep_from = self.output.len() - self.byte_limit;
        while keep_from < self.output.len() && !self.output.is_char_boundary(keep_from) {
            keep_from += 1;
        }
        self.output.drain(..keep_from);
    }

    fn snapshot(&self) -> (String, bool) {
        (self.output.clone(), self.truncated)
    }
}

struct TerminalManagerState {
    terminals: HashMap<String, Arc<ManagedTerminal>>,
    shut_down: bool,
}

/// Manages terminal processes spawned by ACP agents.
pub struct TerminalManager {
    state: Mutex<TerminalManagerState>,
    counter: AtomicU64,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TerminalManagerState {
                terminals: HashMap::new(),
                shut_down: false,
            }),
            counter: AtomicU64::new(0),
        }
    }

    /// Spawn a new terminal process and return its ID.
    pub async fn create(
        &self,
        req: &CreateTerminalRequest,
    ) -> anyhow::Result<CreateTerminalResponse> {
        if self.state.lock().await.shut_down {
            anyhow::bail!("Terminal manager is shut down");
        }
        if req.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
            anyhow::bail!("Terminal working directory must be absolute");
        }

        let mut cmd = Command::new(&req.command);

        cmd.args(&req.args);

        for var in &req.env {
            cmd.env(&var.name, &var.value);
        }

        if let Some(ref cwd) = req.cwd {
            cmd.current_dir(cwd);
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn terminal '{}': {}", req.command, e))?;

        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        let terminal_id = format!("term-{id}");
        let byte_limit = req.output_byte_limit.unwrap_or(DEFAULT_OUTPUT_BYTE_LIMIT);
        let output = Arc::new(Mutex::new(OutputBuffer::new(byte_limit)));
        let stdout = child.stdout.take().expect("terminal stdout was piped");
        let stderr = child.stderr.take().expect("terminal stderr was piped");
        let stdout_task = tokio::spawn(collect_output(stdout, output.clone()));
        let stderr_task = tokio::spawn(collect_output(stderr, output.clone()));
        let process = ProcessHandle::spawn(
            child,
            format!("terminal={terminal_id} command={}", req.command),
        );
        let (outcome_tx, outcome_rx) = watch::channel(None);
        tokio::spawn(monitor_terminal(
            terminal_id.clone(),
            process.waiter(),
            stdout_task,
            stderr_task,
            outcome_tx,
        ));

        let managed = Arc::new(ManagedTerminal {
            session_id: req.session_id.clone(),
            output,
            process,
            outcome_rx,
        });

        let mut state = self.state.lock().await;
        if state.shut_down {
            drop(state);
            let _ = managed.shutdown().await;
            anyhow::bail!("Terminal manager is shut down");
        }
        state.terminals.insert(terminal_id.clone(), managed);

        Ok(CreateTerminalResponse::new(terminal_id))
    }

    /// Get the current output of a terminal.
    pub async fn output(
        &self,
        req: &TerminalOutputRequest,
    ) -> anyhow::Result<TerminalOutputResponse> {
        let term = self.terminal(&req.session_id, &req.terminal_id).await?;
        let exit_status = term
            .current_outcome()?
            .map(|result| terminal_exit_status(&result));
        // The monitor publishes exit only after both output collectors finish.
        // Observe it first so an exited snapshot cannot miss drained bytes.
        let (output, truncated) = term.output.lock().await.snapshot();

        Ok(TerminalOutputResponse::new(output, truncated).exit_status(exit_status))
    }

    /// Wait for a terminal to exit.
    pub async fn wait_for_exit(
        &self,
        req: &WaitForTerminalExitRequest,
    ) -> anyhow::Result<WaitForTerminalExitResponse> {
        let term = self.terminal(&req.session_id, &req.terminal_id).await?;
        let result = term.wait_for_exit().await?;

        Ok(WaitForTerminalExitResponse::new(terminal_exit_status(
            &result,
        )))
    }

    /// Kill a terminal process.
    pub async fn kill(&self, req: &KillTerminalRequest) -> anyhow::Result<KillTerminalResponse> {
        let term = self.terminal(&req.session_id, &req.terminal_id).await?;
        term.shutdown().await?;
        Ok(KillTerminalResponse::new())
    }

    /// Release a terminal, killing it first if it is still running.
    pub async fn release(
        &self,
        req: &ReleaseTerminalRequest,
    ) -> anyhow::Result<ReleaseTerminalResponse> {
        let terminal_id = req.terminal_id.to_string();
        let term = {
            let mut state = self.state.lock().await;
            let term = state
                .terminals
                .get(&terminal_id)
                .ok_or_else(|| anyhow::anyhow!("Unknown terminal: {}", req.terminal_id))?;
            if term.session_id != req.session_id {
                anyhow::bail!("Unknown terminal: {}", req.terminal_id);
            }
            state
                .terminals
                .remove(&terminal_id)
                .expect("terminal existed while manager lock was held")
        };

        term.shutdown().await?;
        Ok(ReleaseTerminalResponse::new())
    }

    /// Kill, reap, and forget every terminal managed by this instance.
    ///
    /// The manager is closed before work begins, so concurrent creates cannot
    /// escape the shutdown. All terminals are attempted and repeated calls are
    /// no-ops.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let terminals = {
            let mut state = self.state.lock().await;
            state.shut_down = true;
            state.terminals.drain().collect::<Vec<_>>()
        };
        let mut first_error = None;

        for (_, terminal) in &terminals {
            terminal.process.request_shutdown();
        }

        for (terminal_id, terminal) in terminals {
            if let Err(error) = terminal.shutdown().await {
                log::warn!(
                    "[acp_terminal] failed to shut down terminal={} err={}",
                    terminal_id,
                    error
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn terminal(
        &self,
        session_id: &SessionId,
        terminal_id: &TerminalId,
    ) -> anyhow::Result<Arc<ManagedTerminal>> {
        let state = self.state.lock().await;
        let terminal = state
            .terminals
            .get(&terminal_id.to_string())
            .ok_or_else(|| anyhow::anyhow!("Unknown terminal: {terminal_id}"))?;
        if &terminal.session_id != session_id {
            anyhow::bail!("Unknown terminal: {terminal_id}");
        }
        Ok(terminal.clone())
    }
}

impl ManagedTerminal {
    fn current_outcome(&self) -> anyhow::Result<Option<TerminalExitResult>> {
        self.outcome_rx
            .borrow()
            .clone()
            .map(|outcome| outcome.as_result())
            .transpose()
    }

    async fn wait_for_exit(&self) -> anyhow::Result<TerminalExitResult> {
        let mut outcome_rx = self.outcome_rx.clone();
        loop {
            if let Some(outcome) = outcome_rx.borrow().clone() {
                return outcome.as_result();
            }

            if outcome_rx.changed().await.is_err() {
                if let Some(outcome) = outcome_rx.borrow().clone() {
                    return outcome.as_result();
                }
                anyhow::bail!("Terminal process monitor stopped without an exit result");
            }
        }
    }

    async fn shutdown(&self) -> anyhow::Result<TerminalExitResult> {
        self.process.request_shutdown();
        self.wait_for_exit().await
    }
}

async fn monitor_terminal(
    terminal_id: String,
    waiter: ProcessWaiter,
    stdout_task: JoinHandle<io::Result<()>>,
    stderr_task: JoinHandle<io::Result<()>>,
    outcome_tx: watch::Sender<Option<TerminalOutcome>>,
) {
    let result = waiter.wait().await;
    finish_output_collectors(&terminal_id, stdout_task, stderr_task).await;

    let outcome = match result {
        Ok(status) => TerminalOutcome::Exited(terminal_exit_result(status)),
        Err(error) => TerminalOutcome::Failed(error.to_string().into()),
    };
    outcome_tx.send_replace(Some(outcome));
}

async fn finish_output_collectors(
    terminal_id: &str,
    mut stdout_task: JoinHandle<io::Result<()>>,
    mut stderr_task: JoinHandle<io::Result<()>>,
) {
    let deadline = tokio::time::sleep(OUTPUT_DRAIN_TIMEOUT);
    tokio::pin!(deadline);
    let mut stdout_result = None;
    let mut stderr_result = None;

    while stdout_result.is_none() || stderr_result.is_none() {
        tokio::select! {
            result = &mut stdout_task, if stdout_result.is_none() => {
                stdout_result = Some(result);
            }
            result = &mut stderr_task, if stderr_result.is_none() => {
                stderr_result = Some(result);
            }
            _ = &mut deadline => break,
        }
    }

    finish_or_abort_output_collector(terminal_id, "stdout", stdout_task, stdout_result).await;
    finish_or_abort_output_collector(terminal_id, "stderr", stderr_task, stderr_result).await;
}

async fn finish_or_abort_output_collector(
    terminal_id: &str,
    stream: &str,
    mut task: JoinHandle<io::Result<()>>,
    result: Option<Result<io::Result<()>, tokio::task::JoinError>>,
) {
    let result = match result {
        Some(result) => result,
        None if task.is_finished() => (&mut task).await,
        None => {
            log::warn!(
                "[acp_terminal] output drain timed out terminal={} stream={}",
                terminal_id,
                stream
            );
            task.abort();
            let _ = task.await;
            return;
        }
    };
    log_output_collector_result(terminal_id, stream, result);
}

fn log_output_collector_result(
    terminal_id: &str,
    stream: &str,
    result: Result<io::Result<()>, tokio::task::JoinError>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::warn!(
            "[acp_terminal] output read failed terminal={} stream={} err={}",
            terminal_id,
            stream,
            error
        ),
        Err(error) => {
            log::warn!(
                "[acp_terminal] output task failed terminal={} stream={} err={}",
                terminal_id,
                stream,
                error
            );
        }
    }
}

async fn collect_output(
    mut reader: impl AsyncRead + Unpin,
    output: Arc<Mutex<OutputBuffer>>,
) -> io::Result<()> {
    let mut read_buffer = [0u8; 4096];
    let mut undecoded = Vec::new();

    loop {
        match reader.read(&mut read_buffer).await {
            Ok(0) => {
                append_decoded(&output, decode_available(&mut undecoded, true)).await;
                return Ok(());
            }
            Ok(read) => {
                undecoded.extend_from_slice(&read_buffer[..read]);
                append_decoded(&output, decode_available(&mut undecoded, false)).await;
            }
            Err(error) => {
                append_decoded(&output, decode_available(&mut undecoded, true)).await;
                return Err(error);
            }
        }
    }
}

async fn append_decoded(output: &Mutex<OutputBuffer>, decoded: String) {
    if !decoded.is_empty() {
        output.lock().await.append(&decoded);
    }
}

fn decode_available(bytes: &mut Vec<u8>, end_of_stream: bool) -> String {
    let mut decoded = String::new();
    let mut consumed = 0;

    while consumed < bytes.len() {
        match std::str::from_utf8(&bytes[consumed..]) {
            Ok(valid) => {
                decoded.push_str(valid);
                consumed = bytes.len();
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                if valid_len > 0 {
                    decoded.push_str(
                        std::str::from_utf8(&bytes[consumed..consumed + valid_len])
                            .expect("UTF-8 validator identified a valid prefix"),
                    );
                    consumed += valid_len;
                }

                match error.error_len() {
                    Some(invalid_len) => {
                        decoded.push(char::REPLACEMENT_CHARACTER);
                        consumed += invalid_len;
                    }
                    None if end_of_stream => {
                        decoded.push(char::REPLACEMENT_CHARACTER);
                        consumed = bytes.len();
                    }
                    None => break,
                }
            }
        }
    }

    if consumed > 0 {
        bytes.drain(..consumed);
    }
    decoded
}

fn terminal_exit_result(status: ExitStatus) -> TerminalExitResult {
    TerminalExitResult {
        exit_code: status.code().map(|code| code as u32),
        signal: exit_signal(&status),
    }
}

#[cfg(unix)]
fn exit_signal(status: &ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;

    status.signal().map(|signal| match signal {
        1 => "SIGHUP".to_string(),
        2 => "SIGINT".to_string(),
        3 => "SIGQUIT".to_string(),
        6 => "SIGABRT".to_string(),
        9 => "SIGKILL".to_string(),
        13 => "SIGPIPE".to_string(),
        14 => "SIGALRM".to_string(),
        15 => "SIGTERM".to_string(),
        _ => signal.to_string(),
    })
}

#[cfg(not(unix))]
fn exit_signal(_status: &ExitStatus) -> Option<String> {
    None
}

fn terminal_exit_status(result: &TerminalExitResult) -> TerminalExitStatus {
    TerminalExitStatus::new()
        .exit_code(result.exit_code)
        .signal(result.signal.clone())
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        state.shut_down = true;
        for terminal in state.terminals.values() {
            terminal.process.request_shutdown();
        }
        state.terminals.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tests::{
        helper_args, unique_test_path, wait_for_file, CHILD_LATE_MARKER_ENV, CHILD_MODE_ENV,
        CHILD_STARTED_ENV,
    };
    use std::{
        fs,
        future::Future,
        task::{Context, Poll},
        time::Duration,
    };

    fn child_request(
        session_id: &str,
        started: &std::path::Path,
        late_marker: Option<&std::path::Path>,
    ) -> CreateTerminalRequest {
        child_request_with_mode(session_id, "hang", started, late_marker)
    }

    fn child_request_with_mode(
        session_id: &str,
        mode: &str,
        started: &std::path::Path,
        late_marker: Option<&std::path::Path>,
    ) -> CreateTerminalRequest {
        let mut env = vec![
            EnvVariable::new(CHILD_MODE_ENV, mode),
            EnvVariable::new(CHILD_STARTED_ENV, started.display().to_string()),
        ];
        if let Some(path) = late_marker {
            env.push(EnvVariable::new(
                CHILD_LATE_MARKER_ENV,
                path.display().to_string(),
            ));
        }

        CreateTerminalRequest::new(
            session_id.to_string(),
            std::env::current_exe()
                .expect("current test executable")
                .display()
                .to_string(),
        )
        .args(helper_args())
        .env(env)
    }

    #[tokio::test]
    async fn kill_terminates_process_and_unblocks_wait() {
        let manager = TerminalManager::new();
        let started = unique_test_path("terminal-kill-started");
        let created = manager
            .create(&child_request("session-1", &started, None))
            .await
            .expect("create terminal");
        wait_for_file(&started).await;

        manager
            .kill(&KillTerminalRequest::new(
                "session-1",
                created.terminal_id.clone(),
            ))
            .await
            .expect("kill terminal");

        let waited = tokio::time::timeout(
            Duration::from_secs(2),
            manager.wait_for_exit(&WaitForTerminalExitRequest::new(
                "session-1",
                created.terminal_id.clone(),
            )),
        )
        .await
        .expect("kill did not unblock terminal wait")
        .expect("wait for killed terminal");
        assert!(
            waited.exit_status.exit_code.is_some() || waited.exit_status.signal.is_some(),
            "killed terminal did not report how it exited"
        );

        let output = manager
            .output(&TerminalOutputRequest::new(
                "session-1",
                created.terminal_id.clone(),
            ))
            .await
            .expect("killed terminal remains valid until release");
        assert!(output.exit_status.is_some());
        manager
            .release(&ReleaseTerminalRequest::new(
                "session-1",
                created.terminal_id,
            ))
            .await
            .expect("release killed terminal");

        let _ = fs::remove_file(started);
    }

    #[tokio::test]
    async fn release_kills_running_process_before_returning() {
        let manager = TerminalManager::new();
        let started = unique_test_path("terminal-release-started");
        let late_marker = unique_test_path("terminal-release-late");
        let created = manager
            .create(&child_request("session-1", &started, Some(&late_marker)))
            .await
            .expect("create terminal");
        let terminal_id = created.terminal_id;
        wait_for_file(&started).await;

        manager
            .release(&ReleaseTerminalRequest::new(
                "session-1",
                terminal_id.clone(),
            ))
            .await
            .expect("release terminal");
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert!(
            !late_marker.exists(),
            "released terminal process continued running"
        );
        assert!(
            manager
                .output(&TerminalOutputRequest::new("session-1", terminal_id))
                .await
                .is_err(),
            "released terminal ID remained valid"
        );
        let _ = fs::remove_file(started);
        let _ = fs::remove_file(late_marker);
    }

    #[tokio::test]
    async fn concurrent_and_late_waiters_observe_the_same_exit() {
        let manager = Arc::new(TerminalManager::new());
        let started = unique_test_path("terminal-waiters-started");
        let terminal_id = manager
            .create(&child_request("session-1", &started, None))
            .await
            .expect("create terminal")
            .terminal_id;
        wait_for_file(&started).await;

        let mut waiters = Vec::new();
        for _ in 0..16 {
            let manager = manager.clone();
            let terminal_id = terminal_id.clone();
            waiters.push(tokio::spawn(async move {
                manager
                    .wait_for_exit(&WaitForTerminalExitRequest::new("session-1", terminal_id))
                    .await
            }));
        }
        tokio::task::yield_now().await;

        manager
            .kill(&KillTerminalRequest::new("session-1", terminal_id.clone()))
            .await
            .expect("kill terminal");
        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(2), waiter)
                .await
                .expect("terminal waiter lost its exit wakeup")
                .expect("terminal waiter task")
                .expect("wait for terminal exit");
        }

        tokio::time::timeout(
            Duration::from_millis(100),
            manager.wait_for_exit(&WaitForTerminalExitRequest::new(
                "session-1",
                terminal_id.clone(),
            )),
        )
        .await
        .expect("late waiter did not observe retained exit state")
        .expect("late terminal wait");
        manager
            .release(&ReleaseTerminalRequest::new("session-1", terminal_id))
            .await
            .expect("release terminal");

        let _ = fs::remove_file(started);
    }

    #[tokio::test]
    async fn natural_exit_is_retained_for_repeated_waits() {
        let manager = TerminalManager::new();
        let started = unique_test_path("terminal-natural-exit-started");
        let terminal_id = manager
            .create(&child_request_with_mode(
                "session-1",
                "exit",
                &started,
                None,
            ))
            .await
            .expect("create terminal")
            .terminal_id;
        wait_for_file(&started).await;

        for _ in 0..2 {
            let response = tokio::time::timeout(
                Duration::from_secs(2),
                manager.wait_for_exit(&WaitForTerminalExitRequest::new(
                    "session-1",
                    terminal_id.clone(),
                )),
            )
            .await
            .expect("terminal did not report natural exit")
            .expect("wait for terminal");
            assert_eq!(response.exit_status.exit_code, Some(0));
            assert_eq!(response.exit_status.signal, None);
        }
        manager
            .release(&ReleaseTerminalRequest::new("session-1", terminal_id))
            .await
            .expect("release terminal");

        let _ = fs::remove_file(started);
    }

    #[tokio::test]
    async fn terminal_ids_are_scoped_to_their_session() {
        let manager = TerminalManager::new();
        let started = unique_test_path("terminal-session-started");
        let terminal_id = manager
            .create(&child_request("session-1", &started, None))
            .await
            .expect("create terminal")
            .terminal_id;
        wait_for_file(&started).await;

        assert!(manager
            .output(&TerminalOutputRequest::new(
                "session-2",
                terminal_id.clone(),
            ))
            .await
            .is_err());
        assert!(manager
            .kill(&KillTerminalRequest::new("session-2", terminal_id.clone(),))
            .await
            .is_err());
        manager
            .release(&ReleaseTerminalRequest::new("session-1", terminal_id))
            .await
            .expect("release terminal from owning session");

        let _ = fs::remove_file(started);
    }

    #[tokio::test]
    async fn manager_shutdown_is_idempotent_and_rejects_new_terminals() {
        let manager = TerminalManager::new();
        let started = unique_test_path("terminal-shutdown-started");
        let late_marker = unique_test_path("terminal-shutdown-late");
        manager
            .create(&child_request("session-1", &started, Some(&late_marker)))
            .await
            .expect("create terminal");
        wait_for_file(&started).await;

        tokio::time::timeout(Duration::from_secs(2), manager.shutdown())
            .await
            .expect("terminal manager shutdown timed out")
            .expect("shut down terminal manager");
        manager.shutdown().await.expect("repeat manager shutdown");
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert!(!late_marker.exists());
        assert!(
            manager
                .create(&child_request("session-1", &started, None))
                .await
                .is_err(),
            "shut down terminal manager accepted new work"
        );
        let _ = fs::remove_file(started);
        let _ = fs::remove_file(late_marker);
    }

    #[tokio::test]
    async fn canceling_manager_shutdown_wait_still_terminates_terminals() {
        let manager = TerminalManager::new();
        let started = unique_test_path("terminal-canceled-shutdown-started");
        let terminal_id = manager
            .create(&child_request("session-1", &started, None))
            .await
            .expect("create terminal")
            .terminal_id;
        wait_for_file(&started).await;
        let session_id: SessionId = "session-1".into();
        let terminal = manager
            .terminal(&session_id, &terminal_id)
            .await
            .expect("managed terminal");

        let mut shutdown = Box::pin(manager.shutdown());
        let mut context = Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(matches!(
            shutdown.as_mut().poll(&mut context),
            Poll::Pending
        ));
        drop(shutdown);

        tokio::time::timeout(Duration::from_secs(2), terminal.wait_for_exit())
            .await
            .expect("canceled manager shutdown left terminal running")
            .expect("wait for terminal shutdown");
        assert!(manager.state.lock().await.terminals.is_empty());

        let _ = fs::remove_file(started);
    }

    #[tokio::test]
    async fn create_rejects_relative_working_directory() {
        let manager = TerminalManager::new();
        let request = CreateTerminalRequest::new("session-1", "unused")
            .cwd(std::path::PathBuf::from("relative"));

        assert!(manager.create(&request).await.is_err());
    }

    #[test]
    fn output_buffer_retains_newest_bytes_at_utf8_boundaries() {
        let mut output = OutputBuffer::new(3);
        output.append("ab\u{e9}cd");

        assert_eq!(output.snapshot(), ("cd".to_string(), true));

        output.append("XYZ");
        assert_eq!(output.snapshot(), ("XYZ".to_string(), true));
    }

    #[test]
    fn output_buffer_always_respects_byte_limit_for_utf8_suffixes() {
        let input = "a\u{e9}\u{20ac}\u{1f4a9}z";

        for limit in 0..=input.len() + 1 {
            let mut output = OutputBuffer::new(limit as u64);
            output.append(input);
            let (retained, truncated) = output.snapshot();

            assert!(retained.len() <= limit, "limit {limit}: {retained:?}");
            assert!(input.ends_with(&retained), "limit {limit}: {retained:?}");
            assert_eq!(truncated, input.len() > limit, "limit {limit}");
        }
    }

    #[test]
    fn output_decoder_preserves_split_utf8_characters() {
        let mut bytes = vec![0xe2, 0x82];
        assert_eq!(decode_available(&mut bytes, false), "");
        assert_eq!(bytes, vec![0xe2, 0x82]);

        bytes.push(0xac);
        assert_eq!(decode_available(&mut bytes, false), "\u{20ac}");
        assert!(bytes.is_empty());
    }

    #[test]
    fn output_decoder_matches_lossy_utf8_for_every_chunk_boundary() {
        let input = b"ok\xe2\x82\xac\xf0\x9f\x92\xa9\xfftail\xf0\x9f";
        let expected = String::from_utf8_lossy(input);

        for split in 0..=input.len() {
            let mut undecoded = input[..split].to_vec();
            let mut actual = decode_available(&mut undecoded, false);
            undecoded.extend_from_slice(&input[split..]);
            actual.push_str(&decode_available(&mut undecoded, true));

            assert_eq!(actual, expected, "mismatch at byte split {split}");
            assert!(undecoded.is_empty());
        }
    }

    #[tokio::test]
    async fn output_collectors_share_one_drain_deadline() {
        let stdout_task = tokio::spawn(std::future::pending::<io::Result<()>>());
        let stderr_task = tokio::spawn(std::future::pending::<io::Result<()>>());
        let started = tokio::time::Instant::now();

        finish_output_collectors("test", stdout_task, stderr_task).await;

        assert!(
            started.elapsed() < OUTPUT_DRAIN_TIMEOUT + Duration::from_millis(500),
            "stdout and stderr each consumed a full drain timeout"
        );
    }

    #[tokio::test]
    async fn output_drain_retains_a_completed_collector_result_at_timeout() {
        let stdout_task = tokio::spawn(async { Ok(()) });
        let stderr_task = tokio::spawn(std::future::pending::<io::Result<()>>());

        finish_output_collectors("test", stdout_task, stderr_task).await;
    }
}
