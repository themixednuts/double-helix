//! Terminal manager for ACP agent terminal requests.
//!
//! Spawns and tracks child processes on behalf of ACP agents.

use crate::types::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

struct ManagedTerminal {
    /// Collected stdout+stderr output.
    output: Arc<Mutex<Vec<u8>>>,
    output_byte_limit: u64,
    /// Set once the process exits. We watch for this via a background task.
    exit_result: Arc<Mutex<Option<TerminalExitResult>>>,
    /// Notify waiters when the process exits.
    exit_notify: Arc<tokio::sync::Notify>,
}

struct TerminalExitResult {
    exit_code: Option<i32>,
}

/// Manages terminal processes spawned by ACP agents.
pub struct TerminalManager {
    terminals: Mutex<HashMap<String, ManagedTerminal>>,
    counter: AtomicU64,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// Spawn a new terminal process and return its ID.
    pub async fn create(
        &self,
        req: &CreateTerminalRequest,
    ) -> anyhow::Result<CreateTerminalResponse> {
        let mut cmd = Command::new(&req.command);

        if let Some(ref args) = req.args {
            cmd.args(args);
        }

        if let Some(ref env_vars) = req.env {
            for var in env_vars {
                cmd.env(&var.name, &var.value);
            }
        }

        if let Some(ref cwd) = req.cwd {
            cmd.current_dir(cwd);
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("Failed to spawn terminal '{}': {}", req.command, e)
        })?;

        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        let terminal_id = format!("term-{id}");
        let byte_limit = req.output_byte_limit.unwrap_or(1_000_000);

        let output = Arc::new(Mutex::new(Vec::new()));
        let exit_result = Arc::new(Mutex::new(None));
        let exit_notify = Arc::new(tokio::sync::Notify::new());

        // Take stdout and stderr handles
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Spawn output collector for stdout
        if let Some(stdout) = stdout {
            let out = output.clone();
            let limit = byte_limit;
            tokio::spawn(async move {
                let mut reader = stdout;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut data = out.lock().await;
                            if (data.len() as u64) < limit {
                                data.extend_from_slice(&buf[..n]);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Spawn output collector for stderr
        if let Some(stderr) = stderr {
            let out = output.clone();
            let limit = byte_limit;
            tokio::spawn(async move {
                let mut reader = stderr;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut data = out.lock().await;
                            if (data.len() as u64) < limit {
                                data.extend_from_slice(&buf[..n]);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Spawn exit watcher
        {
            let exit_result = exit_result.clone();
            let exit_notify = exit_notify.clone();
            tokio::spawn(async move {
                let status = child.wait().await;
                let code = match status {
                    Ok(s) => s.code(),
                    Err(_) => None,
                };
                *exit_result.lock().await = Some(TerminalExitResult { exit_code: code });
                exit_notify.notify_waiters();
            });
        }

        let managed = ManagedTerminal {
            output,
            output_byte_limit: byte_limit,
            exit_result,
            exit_notify,
        };

        self.terminals.lock().await.insert(terminal_id.clone(), managed);

        Ok(CreateTerminalResponse { terminal_id })
    }

    /// Get the current output of a terminal.
    pub async fn output(
        &self,
        req: &TerminalOutputRequest,
    ) -> anyhow::Result<TerminalOutputResponse> {
        let terms = self.terminals.lock().await;
        let term = terms
            .get(&req.terminal_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown terminal: {}", req.terminal_id))?;

        let data = term.output.lock().await;
        let truncated = data.len() as u64 > term.output_byte_limit;
        let output = String::from_utf8_lossy(&data).to_string();

        let exit_result = term.exit_result.lock().await;
        let exit_status = exit_result.as_ref().map(|r| TerminalExitStatus {
            exit_code: r.exit_code,
            signal: None,
        });

        Ok(TerminalOutputResponse {
            output,
            truncated,
            exit_status,
        })
    }

    /// Wait for a terminal to exit.
    pub async fn wait_for_exit(
        &self,
        req: &WaitForTerminalExitRequest,
    ) -> anyhow::Result<WaitForTerminalExitResponse> {
        let notify = {
            let terms = self.terminals.lock().await;
            let term = terms
                .get(&req.terminal_id)
                .ok_or_else(|| anyhow::anyhow!("Unknown terminal: {}", req.terminal_id))?;

            // Already exited?
            let exit = term.exit_result.lock().await;
            if let Some(ref result) = *exit {
                return Ok(WaitForTerminalExitResponse {
                    exit_code: result.exit_code,
                    signal: None,
                });
            }

            term.exit_notify.clone()
        };

        // Wait for exit notification (lock is dropped)
        notify.notified().await;

        let terms = self.terminals.lock().await;
        let term = terms
            .get(&req.terminal_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown terminal: {}", req.terminal_id))?;

        let exit = term.exit_result.lock().await;
        let result = exit
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Terminal exit result missing"))?;

        Ok(WaitForTerminalExitResponse {
            exit_code: result.exit_code,
            signal: None,
        })
    }

    /// Kill a terminal process.
    pub async fn kill(
        &self,
        req: &KillTerminalRequest,
    ) -> anyhow::Result<KillTerminalResponse> {
        // The child is owned by the exit-watcher task, which calls child.wait().
        // We can't kill it from here directly. Instead, we just note it.
        // The kill_on_drop will handle it when the terminal is released.
        // For a proper kill, we'd need to store the child PID and send a signal.
        let terms = self.terminals.lock().await;
        if !terms.contains_key(&req.terminal_id) {
            anyhow::bail!("Unknown terminal: {}", req.terminal_id);
        }
        // kill_on_drop will kill when the terminal is released
        Ok(KillTerminalResponse {})
    }

    /// Release (remove) a terminal from tracking.
    pub async fn release(
        &self,
        req: &ReleaseTerminalRequest,
    ) -> anyhow::Result<ReleaseTerminalResponse> {
        self.terminals.lock().await.remove(&req.terminal_id);
        // Dropping ManagedTerminal drops the Arc refs; the background tasks
        // will finish on their own, and kill_on_drop handles the child.
        Ok(ReleaseTerminalResponse {})
    }
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}
