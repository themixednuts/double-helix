use crate::client::{Client, ClientEvent, ClientEvents};
use percent_encoding::percent_decode_str;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::File,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
    sync::mpsc,
    task::JoinHandle,
    time::Instant,
};

const STDERR_CAPACITY: usize = 128;
const DIAGNOSTIC_CHUNK_BYTES: usize = 8 * 1024;
const SSH_FIELD_BYTES: usize = 1024;
const COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const REMOTE_SERVER_DIR: &str = ".cache/double-helix/server";
const RELEASES_URL: &str = "https://github.com/themixednuts/double-helix/releases/download";
const RELEASE_MANIFEST_BYTES: u64 = 1024 * 1024;
const RELEASE_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const SERVER_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const RELEASE_TIMEOUT: Duration = Duration::from_secs(120);
const SERVER_OVERRIDE_ENV: &str = "DOUBLE_HELIX_SERVER";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUri {
    pub target: SshTarget,
    pub workspace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
}

impl SshTarget {
    pub fn destination(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match &self.user {
            Some(user) => format!("{user}@{host}"),
            None => host,
        }
    }

    fn host_argument(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    fn validate(&self) -> Result<(), SshError> {
        validate_field("host", &self.host, false)?;
        if let Some(user) = &self.user {
            validate_field("user", user, false)?;
        }
        Ok(())
    }
}

impl RemoteUri {
    pub fn parse(input: &str) -> Result<Self, SshError> {
        let uri =
            url::Url::parse(input).map_err(|error| SshError::InvalidUri(error.to_string()))?;
        if uri.scheme() != "ssh" {
            return Err(SshError::InvalidUri(
                "remote workspace URI must use ssh://".to_owned(),
            ));
        }
        if uri.query().is_some() || uri.fragment().is_some() {
            return Err(SshError::InvalidUri(
                "remote workspace URI may not contain a query or fragment".to_owned(),
            ));
        }
        if uri.password().is_some() {
            return Err(SshError::InvalidUri(
                "remote workspace URI may not contain a password".to_owned(),
            ));
        }
        let host = uri
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| SshError::InvalidUri("remote workspace host is missing".to_owned()))?
            .to_owned();
        let user = (!uri.username().is_empty())
            .then(|| {
                percent_decode_str(uri.username())
                    .decode_utf8()
                    .map(|user| user.into_owned())
                    .map_err(|_| SshError::InvalidUri("SSH user is not UTF-8".to_owned()))
            })
            .transpose()?;
        let workspace = percent_decode_str(uri.path())
            .decode_utf8()
            .map_err(|_| SshError::InvalidUri("workspace path is not UTF-8".to_owned()))?
            .into_owned();
        if !workspace.starts_with('/') {
            return Err(SshError::InvalidUri(
                "remote workspace path must be absolute".to_owned(),
            ));
        }
        if workspace.len() > crate::MAX_WORKSPACE_ROOT_BYTES
            || workspace.contains('\0')
            || workspace.chars().any(char::is_control)
        {
            return Err(SshError::InvalidUri(
                "remote workspace path is too long or contains a control character".to_owned(),
            ));
        }
        let target = SshTarget {
            host,
            user,
            port: uri.port(),
        };
        target.validate()?;
        Ok(Self { target, workspace })
    }
}

#[derive(Debug, Clone)]
pub struct SshConfig {
    pub target: SshTarget,
    pub ssh_program: PathBuf,
    pub connect_timeout: Duration,
    pub server_alive_interval: Duration,
    pub server_alive_count_max: u8,
    pub operation_timeout: Duration,
}

impl SshConfig {
    pub fn new(target: SshTarget) -> Self {
        Self {
            target,
            ssh_program: PathBuf::from("ssh"),
            connect_timeout: Duration::from_secs(15),
            server_alive_interval: Duration::from_secs(15),
            server_alive_count_max: 3,
            operation_timeout: Duration::from_secs(120),
        }
    }

    pub async fn prepare_server(&self, build: &ServerBuild) -> Result<PreparedServer, SshError> {
        let expected_identity = build.server_identity();
        let probe = self.probe_server().await?;
        if let Some(server) = accepted_server(
            "exec dhx-server".to_owned(),
            Arc::from(build.identity(probe.platform)),
            &expected_identity,
            probe.path_identity.as_deref(),
        ) {
            return Ok(server);
        }

        let install_id = build.install_id(probe.platform).await?;
        let remote_binary = format!("$HOME/{REMOTE_SERVER_DIR}/{install_id}/dhx-server");
        let cached_identity = self.remote_identity(&remote_binary).await?;
        if let Some(server) = accepted_server(
            format!("exec \"{remote_binary}\""),
            Arc::from(install_id.clone()),
            &expected_identity,
            cached_identity.as_deref(),
        ) {
            return Ok(server);
        }

        let executable = build.resolve(probe.platform).await?;
        let digest = executable.digest().await?;
        let install = install_script(&install_id, &digest);
        let output = self
            .run_command("install server", &install, Some(executable.path()))
            .await?;
        if !String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim() == "__DHX_SERVER_INSTALLED__")
        {
            return Err(SshError::InvalidInstallOutput);
        }
        let installed = self.remote_identity(&remote_binary).await?;
        if let Some(server) = accepted_server(
            format!("exec \"{remote_binary}\""),
            Arc::from(install_id),
            &expected_identity,
            installed.as_deref(),
        ) {
            return Ok(server);
        }
        Err(SshError::InstalledIdentityMismatch {
            expected: expected_identity,
            actual: installed.unwrap_or_else(|| "<no identity>".to_owned()),
        })
    }

    async fn probe_server(&self) -> Result<RemoteProbe, SshError> {
        let output = self
            .run_command(
                "probe remote platform",
                "printf '__DHX_PLATFORM__\\n'; uname -s; uname -m; if command -v dhx-server >/dev/null 2>&1; then printf '__DHX_PATH_IDENTITY__\\n'; dhx-server --identity 2>/dev/null || true; fi",
                None,
            )
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines = stdout.lines().map(str::trim).collect::<Vec<_>>();
        let marker = lines
            .iter()
            .position(|line| *line == "__DHX_PLATFORM__")
            .ok_or(SshError::InvalidProbeOutput)?;
        let os = lines.get(marker + 1).copied().unwrap_or_default();
        let arch = lines.get(marker + 2).copied().unwrap_or_default();
        let platform = RemotePlatform::detect(os, arch)?;
        let path_identity = marker_value(&lines, "__DHX_PATH_IDENTITY__").map(str::to_owned);
        Ok(RemoteProbe {
            platform,
            path_identity,
        })
    }

    async fn remote_identity(&self, remote_binary: &str) -> Result<Option<String>, SshError> {
        let command = format!(
            "if [ -x \"{remote_binary}\" ]; then printf '__DHX_IDENTITY__\\n'; \"{remote_binary}\" --identity 2>/dev/null || true; fi"
        );
        let output = self.run_command("verify server", &command, None).await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines = stdout.lines().map(str::trim).collect::<Vec<_>>();
        Ok(marker_value(&lines, "__DHX_IDENTITY__").map(str::to_owned))
    }

    async fn run_command(
        &self,
        phase: &'static str,
        remote_command: &str,
        input: Option<&Path>,
    ) -> Result<CapturedOutput, SshError> {
        let mut child =
            self.command(remote_command)?
                .spawn()
                .map_err(|source| SshError::Spawn {
                    program: self.ssh_program.clone(),
                    source,
                })?;
        let stdin = child.stdin.take().ok_or(SshError::MissingPipe("stdin"))?;
        let stdout = child.stdout.take().ok_or(SshError::MissingPipe("stdout"))?;
        let stderr = child.stderr.take().ok_or(SshError::MissingPipe("stderr"))?;
        let input = input.map(Path::to_path_buf);
        let input_task = tokio::spawn(async move {
            let mut stdin = stdin;
            if let Some(path) = input {
                let mut file = tokio::fs::File::open(path).await?;
                tokio::io::copy(&mut file, &mut stdin).await?;
            }
            drop(stdin);
            Ok::<_, std::io::Error>(())
        });
        let stdout_task = tokio::spawn(read_bounded(stdout, COMMAND_OUTPUT_BYTES));
        let stderr_task = tokio::spawn(read_bounded(stderr, COMMAND_OUTPUT_BYTES));

        let status = match tokio::time::timeout(self.operation_timeout, child.wait()).await {
            Ok(status) => status.map_err(SshError::Io)?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = input_task.await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(SshError::CommandTimeout {
                    phase,
                    timeout: self.operation_timeout,
                });
            }
        };
        let input_result = input_task.await.map_err(SshError::Task)?;
        let stdout = stdout_task
            .await
            .map_err(SshError::Task)?
            .map_err(SshError::Io)?;
        let stderr = stderr_task
            .await
            .map_err(SshError::Task)?
            .map_err(SshError::Io)?;
        if !status.success() {
            return Err(SshError::CommandFailed {
                phase,
                status,
                stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
            });
        }
        input_result.map_err(SshError::Io)?;
        Ok(CapturedOutput { stdout })
    }

    fn command(&self, remote_command: &str) -> Result<tokio::process::Command, SshError> {
        self.target.validate()?;
        let mut command = tokio::process::Command::new(&self.ssh_program);
        command
            .arg("-T")
            .arg("-o")
            .arg(format!(
                "ConnectTimeout={}",
                self.connect_timeout.as_secs().max(1)
            ))
            .arg("-o")
            .arg(format!(
                "ServerAliveInterval={}",
                self.server_alive_interval.as_secs().max(1)
            ))
            .arg("-o")
            .arg(format!(
                "ServerAliveCountMax={}",
                self.server_alive_count_max.max(1)
            ))
            .args([
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ForwardX11=no",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "RequestTTY=no",
                "-o",
                "EscapeChar=none",
                "-o",
                "StrictHostKeyChecking=ask",
            ]);
        if let Some(port) = self.target.port {
            command.arg("-p").arg(port.to_string());
        }
        if let Some(user) = &self.target.user {
            command.arg("-l").arg(user);
        }
        command
            .arg("--")
            .arg(self.target.host_argument())
            .arg(remote_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.kill_on_drop(true);
        Ok(command)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePlatform {
    LinuxX86_64,
    LinuxAarch64,
    MacosX86_64,
    MacosAarch64,
}

impl RemotePlatform {
    fn detect(os: &str, arch: &str) -> Result<Self, SshError> {
        match (
            os.trim().to_ascii_lowercase().as_str(),
            arch.trim().to_ascii_lowercase().as_str(),
        ) {
            ("linux", "x86_64" | "amd64") => Ok(Self::LinuxX86_64),
            ("linux", "aarch64" | "arm64") => Ok(Self::LinuxAarch64),
            ("darwin", "x86_64" | "amd64") => Ok(Self::MacosX86_64),
            ("darwin", "aarch64" | "arm64") => Ok(Self::MacosAarch64),
            _ => Err(SshError::UnsupportedPlatform {
                remote: format!("{os}/{arch}"),
            }),
        }
    }

    fn release_name(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "x86_64-linux",
            Self::LinuxAarch64 => "aarch64-linux",
            Self::MacosX86_64 => "x86_64-macos",
            Self::MacosAarch64 => "aarch64-macos",
        }
    }
}

struct RemoteProbe {
    platform: RemotePlatform,
    path_identity: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ServerBuild {
    version: Arc<str>,
    release_tag: Arc<str>,
    override_executable: Option<PathBuf>,
}

impl ServerBuild {
    pub fn current(version: impl Into<Arc<str>>, release_tag: impl Into<Arc<str>>) -> Self {
        Self {
            version: version.into(),
            release_tag: release_tag.into(),
            override_executable: std::env::var_os(SERVER_OVERRIDE_ENV).map(PathBuf::from),
        }
    }

    fn identity(&self, platform: RemotePlatform) -> String {
        format!(
            "{}:protocol-{}:{}",
            version_without_hash(&self.version),
            crate::PROTOCOL_VERSION,
            platform.release_name()
        )
    }

    fn server_identity(&self) -> String {
        crate::server_identity(&self.version)
    }

    async fn install_id(&self, platform: RemotePlatform) -> Result<String, SshError> {
        if let Some(path) = self.local_executable() {
            return hash_file(path).await;
        }
        Ok(sha256_bytes(self.identity(platform).as_bytes()))
    }

    fn local_executable(&self) -> Option<&Path> {
        self.override_executable.as_deref()
    }

    async fn resolve(&self, platform: RemotePlatform) -> Result<ServerExecutable, SshError> {
        if let Some(path) = self.local_executable() {
            if !path.is_file() {
                return Err(SshError::ServerOverrideMissing(path.to_path_buf()));
            }
            return Ok(ServerExecutable::local(path.to_path_buf()));
        }

        let release_tag = self.release_tag.clone();
        tokio::task::spawn_blocking(move || download_release_server(&release_tag, platform))
            .await
            .map_err(SshError::ArtifactTask)?
    }
}

struct ServerExecutable {
    path: PathBuf,
    _temp: Option<TempDir>,
}

impl ServerExecutable {
    fn local(path: PathBuf) -> Self {
        Self { path, _temp: None }
    }

    fn temporary(path: PathBuf, temp: TempDir) -> Self {
        Self {
            path,
            _temp: Some(temp),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    async fn digest(&self) -> Result<String, SshError> {
        hash_file(self.path.clone()).await
    }
}

#[derive(Debug, Clone)]
pub struct PreparedServer {
    remote_command: String,
    build_id: Arc<str>,
    warnings: Vec<String>,
}

impl PreparedServer {
    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

fn accepted_server(
    remote_command: String,
    build_id: Arc<str>,
    expected: &str,
    actual: Option<&str>,
) -> Option<PreparedServer> {
    let actual = actual?;
    let warning = match crate::server_identity_match(expected, actual) {
        crate::ServerIdentityMatch::Exact | crate::ServerIdentityMatch::Compatible => None,
        crate::ServerIdentityMatch::Drifted => Some(identity_drift_warning(expected, actual)),
        crate::ServerIdentityMatch::Incompatible => return None,
    };
    if let Some(warning) = &warning {
        log::warn!("{warning}");
    }
    Some(PreparedServer {
        remote_command,
        build_id,
        warnings: warning.into_iter().collect(),
    })
}

fn identity_drift_warning(expected: &str, actual: &str) -> String {
    format!(
        "remote server identity drift accepted: local identity '{expected}', remote identity '{actual}'; remove ~/.cache/double-helix/server to force a clean reinstall"
    )
}

fn version_without_hash(version: &str) -> &str {
    let version = version.trim();
    version
        .strip_suffix(')')
        .and_then(|version_and_hash| version_and_hash.rsplit_once(" ("))
        .and_then(|(version, hash)| (!hash.is_empty()).then_some(version.trim()))
        .unwrap_or(version)
}

struct CapturedOutput {
    stdout: Vec<u8>,
}

pub struct SshSession {
    pub client: Client,
    pub events: ClientEvents,
    config: SshConfig,
    server: PreparedServer,
    active: Option<ActiveSshTransport>,
    reconnect: Option<ReconnectState>,
    pending: VecDeque<SshSessionEvent>,
    closed: bool,
}

struct ActiveSshTransport {
    diagnostics: mpsc::Receiver<Arc<str>>,
    diagnostics_task: JoinHandle<()>,
    child: Child,
    exited: bool,
    diagnostics_open: bool,
}

struct ReconnectState {
    workspace: Arc<crate::backend::RemoteWorkspaceClient>,
    attempt: u32,
    task: Option<JoinHandle<Result<ActiveSshTransport, Arc<str>>>>,
    retry_at: Option<Instant>,
}

#[derive(Debug)]
pub enum SshSessionEvent {
    Remote(ClientEvent),
    Diagnostic(Arc<str>),
    Exited(std::process::ExitStatus),
    Reconnecting {
        attempt: u32,
    },
    ReconnectFailed {
        attempt: u32,
        error: Arc<str>,
        retry_in: Duration,
    },
    Reconnected,
}

impl SshSession {
    pub async fn connect(config: &SshConfig, server: &PreparedServer) -> Result<Self, SshError> {
        let (client, events) = Client::detached();
        let active = connect_transport(config, server, &client).await?;
        Ok(Self {
            client,
            events,
            config: config.clone(),
            server: server.clone(),
            active: Some(active),
            reconnect: None,
            pending: VecDeque::new(),
            closed: false,
        })
    }

    pub fn enable_reconnect(&mut self, workspace: Arc<crate::backend::RemoteWorkspaceClient>) {
        self.reconnect = Some(ReconnectState {
            workspace,
            attempt: 0,
            task: None,
            retry_at: None,
        });
    }

    pub async fn diagnostic(&mut self) -> Option<Arc<str>> {
        match self.active.as_mut() {
            Some(active) => active.diagnostics.recv().await,
            None => None,
        }
    }

    pub fn try_diagnostic(&mut self) -> Result<Arc<str>, mpsc::error::TryRecvError> {
        match self.active.as_mut() {
            Some(active) => active.diagnostics.try_recv(),
            None => Err(mpsc::error::TryRecvError::Disconnected),
        }
    }

    pub async fn next_event(&mut self) -> Option<SshSessionEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.closed {
                return None;
            }

            if self.active.is_some() {
                enum ActiveEvent {
                    Remote(Option<ClientEvent>),
                    Diagnostic(Option<Arc<str>>),
                    Exited(std::io::Result<std::process::ExitStatus>),
                }
                let event = {
                    let active = self.active.as_mut().expect("active transport disappeared");
                    tokio::select! {
                        event = self.events.recv() => ActiveEvent::Remote(event),
                        diagnostic = active.diagnostics.recv(), if active.diagnostics_open => {
                            ActiveEvent::Diagnostic(diagnostic)
                        },
                        status = active.child.wait(), if !active.exited => ActiveEvent::Exited(status),
                    }
                };
                match event {
                    ActiveEvent::Remote(Some(event)) => {
                        return Some(SshSessionEvent::Remote(event));
                    }
                    ActiveEvent::Remote(None) => {
                        self.closed = true;
                    }
                    ActiveEvent::Diagnostic(Some(diagnostic)) => {
                        return Some(SshSessionEvent::Diagnostic(diagnostic));
                    }
                    ActiveEvent::Diagnostic(None) => {
                        if let Some(active) = self.active.as_mut() {
                            active.diagnostics_open = false;
                        }
                    }
                    ActiveEvent::Exited(status) => self.handle_exit(status),
                }
                continue;
            }

            if self
                .reconnect
                .as_ref()
                .and_then(|state| state.task.as_ref())
                .is_some()
            {
                enum ReconnectEvent {
                    Remote(Option<ClientEvent>),
                    Finished(
                        Box<Result<Result<ActiveSshTransport, Arc<str>>, tokio::task::JoinError>>,
                    ),
                }
                let event = {
                    let task = self
                        .reconnect
                        .as_mut()
                        .and_then(|state| state.task.as_mut())
                        .expect("reconnect task disappeared");
                    tokio::select! {
                        event = self.events.recv() => ReconnectEvent::Remote(event),
                        result = task => ReconnectEvent::Finished(Box::new(result)),
                    }
                };
                match event {
                    ReconnectEvent::Remote(Some(event)) => {
                        return Some(SshSessionEvent::Remote(event));
                    }
                    ReconnectEvent::Remote(None) => self.closed = true,
                    ReconnectEvent::Finished(result) => self.finish_reconnect(*result),
                }
                continue;
            }

            let retry_at = self.reconnect.as_ref().and_then(|state| state.retry_at);
            if let Some(retry_at) = retry_at {
                tokio::select! {
                    event = self.events.recv() => match event {
                        Some(event) => return Some(SshSessionEvent::Remote(event)),
                        None => self.closed = true,
                    },
                    _ = tokio::time::sleep_until(retry_at) => self.start_reconnect(),
                }
                continue;
            }

            self.closed = true;
        }
    }

    fn handle_exit(&mut self, status: std::io::Result<std::process::ExitStatus>) {
        if let Some(mut active) = self.active.take() {
            while let Ok(diagnostic) = active.diagnostics.try_recv() {
                self.pending
                    .push_back(SshSessionEvent::Diagnostic(diagnostic));
            }
            active.diagnostics_task.abort();
        }
        let reason = match status {
            Ok(status) => {
                self.pending.push_back(SshSessionEvent::Exited(status));
                Arc::from(format!("remote SSH process exited with {status}"))
            }
            Err(error) => Arc::from(format!("failed to wait for remote SSH process: {error}")),
        };
        self.client.disconnect_current(reason);
        if self.reconnect.is_some() {
            self.start_reconnect();
        } else {
            self.closed = true;
        }
    }

    fn start_reconnect(&mut self) {
        let Some(state) = self.reconnect.as_mut() else {
            self.closed = true;
            return;
        };
        if state.task.is_some() {
            return;
        }
        state.attempt = state.attempt.saturating_add(1);
        state.retry_at = None;
        let attempt = state.attempt;
        let config = self.config.clone();
        let server = self.server.clone();
        let client = self.client.clone();
        let workspace = state.workspace.clone();
        state.task = Some(tokio::spawn(async move {
            let mut active = connect_transport(&config, &server, &client)
                .await
                .map_err(|error| Arc::from(error.to_string()))?;
            if let Err(error) = workspace.reopen().await {
                client.disconnect_current(Arc::from(format!(
                    "remote workspace reconnect handshake failed: {error}"
                )));
                active.terminate().await;
                return Err(Arc::from(error.to_string()));
            }
            Ok(active)
        }));
        self.pending
            .push_back(SshSessionEvent::Reconnecting { attempt });
    }

    fn finish_reconnect(
        &mut self,
        result: Result<Result<ActiveSshTransport, Arc<str>>, tokio::task::JoinError>,
    ) {
        let state = self
            .reconnect
            .as_mut()
            .expect("reconnect state disappeared");
        state.task = None;
        match result {
            Ok(Ok(active)) => {
                self.active = Some(active);
                state.attempt = 0;
                state.retry_at = None;
                self.pending.push_back(SshSessionEvent::Reconnected);
            }
            Ok(Err(error)) => {
                let retry_in = reconnect_delay(state.attempt);
                state.retry_at = Some(Instant::now() + retry_in);
                self.pending.push_back(SshSessionEvent::ReconnectFailed {
                    attempt: state.attempt,
                    error,
                    retry_in,
                });
            }
            Err(error) => {
                let retry_in = reconnect_delay(state.attempt);
                state.retry_at = Some(Instant::now() + retry_in);
                self.pending.push_back(SshSessionEvent::ReconnectFailed {
                    attempt: state.attempt,
                    error: Arc::from(format!("remote reconnect worker failed: {error}")),
                    retry_in,
                });
            }
        }
    }

    pub async fn shutdown(mut self) {
        if let Some(task) = self.reconnect.as_mut().and_then(|state| state.task.take()) {
            task.abort();
            let _ = task.await;
        }
        self.client.shutdown().await;
        if let Some(mut active) = self.active.take() {
            active.terminate().await;
        }
    }
}

impl ActiveSshTransport {
    async fn terminate(&mut self) {
        match tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
            }
        }
        self.diagnostics_task.abort();
        let _ = (&mut self.diagnostics_task).await;
    }
}

async fn connect_transport(
    config: &SshConfig,
    server: &PreparedServer,
    client: &Client,
) -> Result<ActiveSshTransport, SshError> {
    let mut child = config
        .command(&server.remote_command)?
        .spawn()
        .map_err(|source| SshError::Spawn {
            program: config.ssh_program.clone(),
            source,
        })?;
    let stdin = child.stdin.take().ok_or(SshError::MissingPipe("stdin"))?;
    let stdout = child.stdout.take().ok_or(SshError::MissingPipe("stdout"))?;
    let stderr = child.stderr.take().ok_or(SshError::MissingPipe("stderr"))?;
    let (diagnostics_tx, diagnostics) = mpsc::channel(STDERR_CAPACITY);
    let diagnostics_task = tokio::spawn(drain_diagnostics(stderr, diagnostics_tx));
    if let Err(error) = client.attach_io(stdout, stdin) {
        diagnostics_task.abort();
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(SshError::Client(error));
    }
    Ok(ActiveSshTransport {
        diagnostics,
        diagnostics_task,
        child,
        exited: false,
        diagnostics_open: true,
    })
}

fn reconnect_delay(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(5);
    Duration::from_secs(1u64 << exponent).min(MAX_RECONNECT_DELAY)
}

async fn read_bounded(
    mut input: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, std::io::Error> {
    let mut output = Vec::with_capacity(limit.min(DIAGNOSTIC_CHUNK_BYTES));
    let mut bytes = [0; DIAGNOSTIC_CHUNK_BYTES];
    loop {
        let read = input.read(&mut bytes).await?;
        if read == 0 {
            break;
        }
        let retained = read.min(limit.saturating_sub(output.len()));
        output.extend_from_slice(&bytes[..retained]);
    }
    Ok(output)
}

fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        hasher.update(&bytes[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn hash_file(path: impl Into<PathBuf>) -> Result<String, SshError> {
    let path = path.into();
    tokio::task::spawn_blocking(move || sha256_file(&path))
        .await
        .map_err(SshError::HashTask)?
        .map_err(SshError::Io)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn marker_value<'a>(lines: &'a [&str], marker: &str) -> Option<&'a str> {
    lines
        .iter()
        .position(|line| *line == marker)
        .and_then(|index| lines.get(index + 1).copied())
        .filter(|value| !value.is_empty() && !value.starts_with("__DHX_"))
}

fn download_release_server(
    release_tag: &str,
    platform: RemotePlatform,
) -> Result<ServerExecutable, SshError> {
    validate_release_tag(release_tag)?;
    let asset = format!(
        "dhx-server-{release_tag}-{}.tar.xz",
        platform.release_name()
    );
    let release = format!("{RELEASES_URL}/{release_tag}");
    let manifest_url = format!("{release}/SHA256SUMS");
    let manifest = download_bytes(&manifest_url, RELEASE_MANIFEST_BYTES)?;
    let manifest = std::str::from_utf8(&manifest).map_err(|_| SshError::InvalidManifest {
        url: manifest_url.clone(),
        message: "manifest is not UTF-8".to_owned(),
    })?;
    let expected = checksum_for(manifest, &asset).ok_or_else(|| SshError::InvalidManifest {
        url: manifest_url,
        message: format!("no checksum for {asset}"),
    })?;

    let temp = tempfile::tempdir().map_err(SshError::Io)?;
    let archive = temp.path().join(&asset);
    let asset_url = format!("{release}/{asset}");
    let actual = download_file(&asset_url, &archive, RELEASE_ARCHIVE_BYTES)?;
    if actual != expected {
        return Err(SshError::ChecksumMismatch {
            asset,
            expected,
            actual,
        });
    }

    let executable = temp.path().join("dhx-server");
    extract_server(&archive, &executable)?;
    Ok(ServerExecutable::temporary(executable, temp))
}

fn validate_release_tag(tag: &str) -> Result<(), SshError> {
    if tag.is_empty()
        || tag.len() > 64
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(SshError::InvalidReleaseTag(tag.to_owned()));
    }
    Ok(())
}

fn checksum_for(manifest: &str, asset: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let checksum = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        (name == asset
            && fields.next().is_none()
            && checksum.len() == 64
            && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| checksum.to_ascii_lowercase())
    })
}

fn release_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(5)
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(RELEASE_TIMEOUT)
        .timeout_write(RELEASE_TIMEOUT)
        .build()
}

fn response(url: &str) -> Result<ureq::Response, SshError> {
    if !url.starts_with("https://") {
        return Err(SshError::InsecureReleaseUrl(url.to_owned()));
    }
    let response = release_agent()
        .get(url)
        .set("User-Agent", "double-helix-remote")
        .call()
        .map_err(|source| SshError::Http {
            url: url.to_owned(),
            source: Box::new(source),
        })?;
    if !response.get_url().starts_with("https://") {
        return Err(SshError::InsecureReleaseUrl(response.get_url().to_owned()));
    }
    Ok(response)
}

fn response_length(response: &ureq::Response) -> Option<u64> {
    response
        .header("Content-Length")
        .and_then(|value| value.parse().ok())
}

fn download_bytes(url: &str, limit: u64) -> Result<Vec<u8>, SshError> {
    let response = response(url)?;
    reject_oversized_response(url, response_length(&response), limit)?;
    let mut reader = response.into_reader();
    let mut output = Vec::new();
    copy_bounded(url, &mut reader, &mut output, limit, None)?;
    Ok(output)
}

fn download_file(url: &str, path: &Path, limit: u64) -> Result<String, SshError> {
    let response = response(url)?;
    reject_oversized_response(url, response_length(&response), limit)?;
    let mut reader = response.into_reader();
    let mut output = File::create(path).map_err(SshError::Io)?;
    let mut hasher = Sha256::new();
    copy_bounded(url, &mut reader, &mut output, limit, Some(&mut hasher))?;
    output.flush().map_err(SshError::Io)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn reject_oversized_response(url: &str, length: Option<u64>, limit: u64) -> Result<(), SshError> {
    if length.is_some_and(|length| length > limit) {
        return Err(SshError::DownloadTooLarge {
            url: url.to_owned(),
            limit,
        });
    }
    Ok(())
}

fn copy_bounded(
    url: &str,
    input: &mut impl Read,
    output: &mut impl Write,
    limit: u64,
    mut hasher: Option<&mut Sha256>,
) -> Result<(), SshError> {
    let mut bytes = [0; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = input.read(&mut bytes).map_err(SshError::Io)?;
        if read == 0 {
            return Ok(());
        }
        total = total.saturating_add(read as u64);
        if total > limit {
            return Err(SshError::DownloadTooLarge {
                url: url.to_owned(),
                limit,
            });
        }
        output.write_all(&bytes[..read]).map_err(SshError::Io)?;
        if let Some(hasher) = hasher.as_deref_mut() {
            hasher.update(&bytes[..read]);
        }
    }
}

fn extract_server(archive_path: &Path, output_path: &Path) -> Result<(), SshError> {
    let archive = File::open(archive_path).map_err(SshError::Io)?;
    let decoder = xz2::read::XzDecoder::new(BufReader::new(archive));
    let mut archive = tar::Archive::new(decoder);
    let mut found = false;
    for entry in archive.entries().map_err(SshError::Io)? {
        let mut entry = entry.map_err(SshError::Io)?;
        let path = entry.path().map_err(SshError::Io)?;
        if path.as_ref() != Path::new("dhx-server")
            || !entry.header().entry_type().is_file()
            || found
            || entry.size() > SERVER_BINARY_BYTES
        {
            return Err(SshError::InvalidServerArchive(
                "archive must contain exactly one regular file named dhx-server".to_owned(),
            ));
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output_path)
            .map_err(SshError::Io)?;
        let copied = std::io::copy(
            &mut entry.by_ref().take(SERVER_BINARY_BYTES + 1),
            &mut output,
        )
        .map_err(SshError::Io)?;
        if copied > SERVER_BINARY_BYTES {
            return Err(SshError::InvalidServerArchive(
                "server binary exceeds the extraction limit".to_owned(),
            ));
        }
        output.flush().map_err(SshError::Io)?;
        found = true;
    }
    if !found {
        return Err(SshError::InvalidServerArchive(
            "archive does not contain dhx-server".to_owned(),
        ));
    }
    Ok(())
}

fn install_script(install_id: &str, digest: &str) -> String {
    format!(
        "set -eu; umask 077; base=\"$HOME/{REMOTE_SERVER_DIR}/{install_id}\"; mkdir -p \"$base\"; chmod 700 \"$base\"; tmp=\"$base/.dhx-server.$$.tmp\"; trap 'rm -f \"$tmp\"' EXIT HUP INT TERM; cat > \"$tmp\"; chmod 700 \"$tmp\"; if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum \"$tmp\"); actual=${{actual%% *}}; elif command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 \"$tmp\"); actual=${{actual%% *}}; else printf 'no SHA-256 utility found\\n' >&2; exit 69; fi; if [ \"$actual\" != \"{digest}\" ]; then printf 'server checksum mismatch\\n' >&2; exit 65; fi; mv -f \"$tmp\" \"$base/dhx-server\"; trap - EXIT HUP INT TERM; printf '__DHX_SERVER_INSTALLED__\\n'"
    )
}

async fn drain_diagnostics(mut stderr: impl AsyncRead + Unpin, output: mpsc::Sender<Arc<str>>) {
    let mut bytes = [0; DIAGNOSTIC_CHUNK_BYTES];
    loop {
        let read = match stderr.read(&mut bytes).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let diagnostic = String::from_utf8_lossy(&bytes[..read]);
        if output.try_send(Arc::from(diagnostic.as_ref())).is_err() && output.is_closed() {
            break;
        }
    }
}

fn validate_field(name: &'static str, value: &str, allow_empty: bool) -> Result<(), SshError> {
    if (!allow_empty && value.is_empty())
        || value.len() > SSH_FIELD_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(SshError::InvalidTarget(format!(
            "SSH {name} is empty, too long, or contains a control character"
        )));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("invalid remote SSH URI: {0}")]
    InvalidUri(String),
    #[error("invalid remote SSH target: {0}")]
    InvalidTarget(String),
    #[error("failed to hash the remote server binary: {0}")]
    HashTask(#[source] tokio::task::JoinError),
    #[error("remote server artifact worker failed: {0}")]
    ArtifactTask(#[source] tokio::task::JoinError),
    #[error("remote server probe returned an invalid response")]
    InvalidProbeOutput,
    #[error("server install returned an invalid response")]
    InvalidInstallOutput,
    #[error("remote host platform {remote} is not supported")]
    UnsupportedPlatform { remote: String },
    #[error("remote server override does not exist or is not a file: '{0}'")]
    ServerOverrideMissing(PathBuf),
    #[error("invalid remote server release tag: {0}")]
    InvalidReleaseTag(String),
    #[error("refusing insecure remote server release URL: {0}")]
    InsecureReleaseUrl(String),
    #[error("remote server download failed for {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: Box<ureq::Error>,
    },
    #[error("remote server download exceeds {limit} bytes: {url}")]
    DownloadTooLarge { url: String, limit: u64 },
    #[error("invalid checksum manifest at {url}: {message}")]
    InvalidManifest { url: String, message: String },
    #[error(
        "checksum mismatch for remote server artifact {asset}: expected {expected}, got {actual}"
    )]
    ChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
    #[error("invalid remote server archive: {0}")]
    InvalidServerArchive(String),
    #[error("installed server identity mismatch: local build expects {expected}, but the published release asset reports {actual}; the published release assets do not match the local build's version/protocol, and refreshing them requires pushing the release tag")]
    InstalledIdentityMismatch { expected: String, actual: String },
    #[error("SSH {phase} timed out after {timeout:?}")]
    CommandTimeout {
        phase: &'static str,
        timeout: Duration,
    },
    #[error("SSH {phase} failed with {status}: {stderr}")]
    CommandFailed {
        phase: &'static str,
        status: std::process::ExitStatus,
        stderr: String,
    },
    #[error("SSH I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("SSH worker failed: {0}")]
    Task(#[source] tokio::task::JoinError),
    #[error("failed to start SSH program '{}': {source}", program.display())]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("SSH child did not expose its {0} pipe")]
    MissingPipe(&'static str),
    #[error("remote client rejected the SSH transport: {0}")]
    Client(#[source] crate::client::ClientError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_remote_uri() {
        let uri = RemoteUri::parse("ssh://jon%40work@example.com:2222/srv/my%20repo").unwrap();
        assert_eq!(uri.target.host, "example.com");
        assert_eq!(uri.target.user.as_deref(), Some("jon@work"));
        assert_eq!(uri.target.port, Some(2222));
        assert_eq!(uri.workspace, "/srv/my repo");
    }

    #[test]
    fn rejects_passwords_and_control_characters_in_remote_uris() {
        assert!(matches!(
            RemoteUri::parse("ssh://user:secret@example.com/work"),
            Err(SshError::InvalidUri(_))
        ));
        assert!(matches!(
            RemoteUri::parse("ssh://example.com/work%0Aspace"),
            Err(SshError::InvalidUri(_))
        ));
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(reconnect_delay(1), Duration::from_secs(1));
        assert_eq!(reconnect_delay(2), Duration::from_secs(2));
        assert_eq!(reconnect_delay(100), MAX_RECONNECT_DELAY);
    }

    #[test]
    fn formats_ipv6_destination() {
        assert_eq!(
            SshTarget {
                host: "2001:db8::1".to_owned(),
                user: Some("dev".to_owned()),
                port: None,
            }
            .destination(),
            "dev@[2001:db8::1]"
        );
    }

    #[test]
    fn maps_supported_remote_platforms_to_release_assets() {
        let platform = RemotePlatform::detect("Linux", "x86_64").unwrap();
        assert_eq!(platform, RemotePlatform::LinuxX86_64);
        assert_eq!(platform.release_name(), "x86_64-linux");

        assert_eq!(
            RemotePlatform::detect("Darwin", "arm64").unwrap(),
            RemotePlatform::MacosAarch64
        );
        assert!(matches!(
            RemotePlatform::detect("FreeBSD", "x86_64"),
            Err(SshError::UnsupportedPlatform { .. })
        ));
    }

    #[test]
    fn parses_only_exact_checksum_manifest_entries() {
        let digest = "a".repeat(64);
        let manifest = format!(
            "{digest}  dhx-server-25.7.1-x86_64-linux.tar.xz\n{}  other.tar.xz\n",
            "b".repeat(64)
        );
        assert_eq!(
            checksum_for(&manifest, "dhx-server-25.7.1-x86_64-linux.tar.xz").as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(checksum_for("abcd  file\n", "file"), None);
        assert_eq!(checksum_for(&format!("{digest}  ../file\n"), "file"), None);
    }

    #[test]
    fn validates_release_tags_before_building_urls() {
        assert!(validate_release_tag("25.7.1").is_ok());
        assert!(validate_release_tag("build-20260803").is_ok());
        assert!(validate_release_tag("../latest").is_err());
        assert!(validate_release_tag("release/tag").is_err());
    }

    #[test]
    fn server_identity_includes_the_remote_protocol_revision() {
        let build = ServerBuild::current("25.07.1 (test)", "25.7.1");

        assert_eq!(
            build.server_identity(),
            format!(
                "double-helix 25.07.1 (test) remote-protocol {}",
                crate::PROTOCOL_VERSION
            )
        );
        assert!(build
            .identity(RemotePlatform::LinuxX86_64)
            .contains(&format!("protocol-{}", crate::PROTOCOL_VERSION)));
    }

    #[test]
    fn accepts_exact_server_identity() {
        let identity = crate::server_identity("25.07.1 (test)");

        assert_eq!(
            crate::server_identity_match(&identity, &identity),
            crate::ServerIdentityMatch::Exact
        );
    }

    #[test]
    fn detects_server_commit_drift() {
        let expected = crate::server_identity("25.07.1 (local)");
        let actual = crate::server_identity("25.07.1 (remote)");

        assert_eq!(
            crate::server_identity_match(&expected, &actual),
            crate::ServerIdentityMatch::Drifted
        );
    }

    #[test]
    fn detects_hashless_server_identity_drift() {
        let expected = crate::server_identity("25.07.1 (remote)");
        let actual = crate::server_identity("25.07.1");

        assert_eq!(
            crate::server_identity_match(&expected, &actual),
            crate::ServerIdentityMatch::Drifted
        );
    }

    #[test]
    fn rejects_server_identity_protocol_mismatch() {
        let expected = crate::server_identity("25.07.1 (test)");
        let actual = format!(
            "double-helix 25.07.1 (test) remote-protocol {}",
            crate::PROTOCOL_VERSION.saturating_add(1)
        );

        assert_eq!(
            crate::server_identity_match(&expected, &actual),
            crate::ServerIdentityMatch::Incompatible
        );
    }

    #[test]
    fn rejects_server_identity_version_mismatch() {
        let expected = crate::server_identity("25.07.1 (test)");
        let actual = crate::server_identity("25.07.2 (test)");

        assert_eq!(
            crate::server_identity_match(&expected, &actual),
            crate::ServerIdentityMatch::Incompatible
        );
    }

    #[test]
    fn rejects_garbage_server_identity() {
        let expected = crate::server_identity("25.07.1 (test)");

        assert_eq!(
            crate::server_identity_match(&expected, "not a server identity"),
            crate::ServerIdentityMatch::Incompatible
        );
    }

    #[test]
    fn cache_identity_ignores_commit_hash() {
        let first = ServerBuild {
            version: Arc::from("25.07.1 (first)"),
            release_tag: Arc::from("25.7.1"),
            override_executable: None,
        };
        let second = ServerBuild {
            version: Arc::from("25.07.1 (second)"),
            release_tag: Arc::from("25.7.1"),
            override_executable: None,
        };
        let hashless = ServerBuild {
            version: Arc::from("25.07.1"),
            release_tag: Arc::from("25.7.1"),
            override_executable: None,
        };

        assert_eq!(
            first.identity(RemotePlatform::LinuxX86_64),
            second.identity(RemotePlatform::LinuxX86_64)
        );
        assert_eq!(
            first.identity(RemotePlatform::LinuxX86_64),
            hashless.identity(RemotePlatform::LinuxX86_64)
        );
    }

    #[test]
    fn attaches_warning_for_server_identity_drift() {
        let expected = crate::server_identity("25.07.1 (local)");
        let actual = crate::server_identity("25.07.1 (remote)");
        let server = accepted_server(
            "exec dhx-server".to_owned(),
            Arc::from("build-id"),
            &expected,
            Some(actual.as_str()),
        )
        .expect("drifted identities are accepted");

        assert_eq!(server.warnings().len(), 1);
        assert!(server.warnings()[0].contains(&expected));
        assert!(server.warnings()[0].contains(&actual));
        assert!(server.warnings()[0].contains("~/.cache/double-helix/server"));
    }

    #[test]
    fn server_build_has_no_implicit_local_executable() {
        let build = ServerBuild {
            version: Arc::from("25.07.1 (test)"),
            release_tag: Arc::from("25.7.1"),
            override_executable: None,
        };

        assert!(build.local_executable().is_none());
    }

    #[test]
    fn extracts_only_the_server_binary() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("server.tar.xz");
        write_test_archive(&archive, &[("dhx-server", b"server")]);
        let output = temp.path().join("installed-dhx-server");
        extract_server(&archive, &output).unwrap();
        assert_eq!(std::fs::read(output).unwrap(), b"server");
    }

    #[test]
    fn rejects_archives_with_additional_entries() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("server.tar.xz");
        write_test_archive(&archive, &[("dhx-server", b"server"), ("extra", b"no")]);
        let output = temp.path().join("installed-dhx-server");
        assert!(matches!(
            extract_server(&archive, &output),
            Err(SshError::InvalidServerArchive(_))
        ));
    }

    #[tokio::test]
    #[ignore = "requires DHX_TEST_SSH_URI and an SSH host"]
    async fn ssh_end_to_end_opens_a_remote_workspace() -> Result<(), Box<dyn std::error::Error>> {
        let uri = RemoteUri::parse(&std::env::var("DHX_TEST_SSH_URI")?)?;
        let config = SshConfig::new(uri.target.clone());
        let build =
            ServerBuild::current(helix_ipc::VERSION_AND_GIT_HASH, env!("CARGO_PKG_VERSION"));
        let server = config.prepare_server(&build).await?;
        let mut session = SshSession::connect(&config, &server).await?;
        let workspace = crate::backend::RemoteWorkspaceClient::open(
            session.client.clone(),
            uri.target.destination(),
            helix_ipc::VERSION_AND_GIT_HASH,
            uri.workspace,
        )
        .await?;
        session.enable_reconnect(Arc::new(workspace));
        session.shutdown().await;
        Ok(())
    }

    fn write_test_archive(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let encoder = xz2::write::XzEncoder::new(file, 6);
        let mut archive = tar::Builder::new(encoder);
        for (path, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(&mut header, path, Cursor::new(*bytes))
                .unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap();
    }
}
