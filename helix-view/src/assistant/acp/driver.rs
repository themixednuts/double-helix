use std::collections::{hash_map::DefaultHasher, BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;

use helix_acp::client::{AgentConfig, IncomingReceiver};
use helix_acp::types as acp;
use helix_acp::AcpAgent;

use super::super::{auth, backend, host, permission, review, thread};
use super::{translate, Session};

pub struct Driver {
    config: AgentConfig,
    client_info: acp::Implementation,
}

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("command", &self.config.command)
            .finish()
    }
}

impl Driver {
    #[must_use]
    pub fn new(config: AgentConfig, client_info: acp::Implementation) -> Self {
        Self {
            config,
            client_info,
        }
    }
}

impl backend::Driver for Driver {
    fn kind(&self) -> backend::Kind {
        backend::kind::ACP
    }

    fn spawn(
        &self,
        runtime: &helix_runtime::Runtime,
        host: host::Set,
        tx: helix_runtime::Sender<backend::Update>,
        connect: backend::Connect,
    ) -> Result<backend::Handle, backend::Error> {
        if self.config.command.is_empty() {
            return Err(backend::Error::Other(anyhow::anyhow!(
                "ACP command is empty"
            )));
        }

        let backend_id = backend::Id::new(Arc::<str>::from(self.config.command.clone()));
        let (handle_tx, mut handle_rx) = helix_runtime::channel(64);
        let (agent, mut incoming) = AcpAgent::start_standalone(&self.config)
            .map_err(|err| backend::Error::Other(anyhow::anyhow!(err.to_string())))?;

        let tx_loop = tx.clone();
        let host_loop = host.clone();
        let connect_loop = connect.clone();
        let client_info = self.client_info.clone();
        let auth_command = self.config.command.clone();
        let backend_loop = backend_id.clone();
        let work = runtime.work().clone();
        runtime
            .work()
            .spawn(async move {
                run_agent(
                    RunAgent {
                        backend_id: backend_loop,
                        work,
                        agent,
                        tx: tx_loop,
                        host: host_loop,
                        connect: connect_loop,
                        client_info,
                        auth_command,
                    },
                    &mut handle_rx,
                    &mut incoming,
                )
                .await;
            })
            .detach();

        Ok(backend::Handle::new(backend_id, handle_tx))
    }
}

struct State {
    sessions: HashMap<thread::Id, Session>,
    terminals: HashMap<String, (thread::Id, host::TerminalId)>,
    permissions: HashMap<permission::RequestId, PendingPermission>,
    elicitations: HashMap<String, PendingElicitation>,
    review_modes: HashMap<thread::Id, review::Mode>,
    staged: HashMap<thread::Id, HashMap<PathBuf, review::File>>,
    rules: permission::Rules,
    caps: helix_acp::AgentCaps,
    auth_methods: Vec<auth::Method>,
    pending_auth: Option<backend::Command>,
}

struct PendingPermission {
    rpc: helix_acp::jsonrpc::Id,
    agent: String,
    tool: String,
    choices: Vec<permission::Choice>,
}

struct PendingElicitation {
    rpc: helix_acp::jsonrpc::Id,
    thread: thread::Id,
}

impl State {
    fn new(caps: helix_acp::AgentCaps, auth_methods: Vec<auth::Method>) -> Self {
        Self {
            sessions: HashMap::new(),
            terminals: HashMap::new(),
            permissions: HashMap::new(),
            elicitations: HashMap::new(),
            review_modes: HashMap::new(),
            staged: HashMap::new(),
            rules: permission::Rules::load(),
            caps,
            auth_methods,
            pending_auth: None,
        }
    }

    fn review_mode(&self, thread: thread::Id) -> review::Mode {
        self.review_modes.get(&thread).copied().unwrap_or_default()
    }

    fn staged_text(&self, thread: thread::Id, path: &std::path::Path) -> Option<String> {
        self.staged
            .get(&thread)
            .and_then(|files| files.get(path))
            .filter(|file| file.status.is_pending())
            .map(|file| file.after.clone())
    }

    fn stage(&mut self, thread: thread::Id, file: review::File) {
        self.staged
            .entry(thread)
            .or_default()
            .insert(file.path.clone(), file);
    }

    fn resolve(
        &mut self,
        thread: thread::Id,
        target: &review::Target,
        decision: review::Decision,
    ) -> Vec<review::File> {
        let Some(files) = self.staged.get_mut(&thread) else {
            return Vec::new();
        };
        let mut resolved = Vec::new();
        let paths: Vec<_> = files
            .keys()
            .filter(|path| match target {
                review::Target::All => true,
                review::Target::File(target) => *path == target,
            })
            .cloned()
            .collect();

        for path in paths {
            let Some(mut file) = files.remove(&path) else {
                continue;
            };
            if file.status.is_pending() {
                file.resolve(decision);
                resolved.push(file);
            }
        }
        resolved
    }
}

fn thread_for_session(state: &State, session_id: impl std::fmt::Display) -> Option<thread::Id> {
    let session = Session::new(session_id.to_string());
    state
        .sessions
        .iter()
        .find(|(_, current)| **current == session)
        .map(|(&thread, _)| thread)
}

fn thread_id_for_remote(remote: &str) -> thread::Id {
    let mut hasher = DefaultHasher::new();
    remote.hash(&mut hasher);
    let id = hasher.finish().max(1);
    thread::Id::new(NonZeroU64::new(id).expect("hash id is non-zero"))
}

fn elicitation_action(response: thread::ElicitationResponse) -> acp::ElicitationAction {
    match response {
        thread::ElicitationResponse::Accept(values) => {
            let content = values
                .into_iter()
                .map(|(key, value)| (key, elicitation_value(value)))
                .collect::<BTreeMap<_, _>>();
            acp::ElicitationAction::Accept(acp::ElicitationAcceptAction::new().content(content))
        }
        thread::ElicitationResponse::Decline => acp::ElicitationAction::Decline,
        thread::ElicitationResponse::Cancel => acp::ElicitationAction::Cancel,
    }
}

fn elicitation_value(value: thread::ElicitationValue) -> acp::ElicitationContentValue {
    match value {
        thread::ElicitationValue::String(value) => acp::ElicitationContentValue::String(value),
        thread::ElicitationValue::Integer(value) => acp::ElicitationContentValue::Integer(value),
        thread::ElicitationValue::Number(value) => value
            .parse::<f64>()
            .map(acp::ElicitationContentValue::Number)
            .unwrap_or(acp::ElicitationContentValue::String(value)),
        thread::ElicitationValue::Boolean(value) => acp::ElicitationContentValue::Boolean(value),
        thread::ElicitationValue::StringArray(value) => {
            acp::ElicitationContentValue::StringArray(value)
        }
    }
}

fn auth_methods(init: &acp::InitializeResponse, default_command: &str) -> Vec<auth::Method> {
    init.auth_methods
        .iter()
        .map(|method| auth::Method {
            id: method.id().to_string(),
            name: method.name().to_string(),
            terminal: terminal_auth(method, default_command),
        })
        .collect()
}

fn terminal_auth(method: &acp::AuthMethod, default_command: &str) -> Option<auth::Terminal> {
    if let Some(meta) = method.meta() {
        if let Some(value) = meta
            .get("terminal-auth")
            .or_else(|| meta.get("terminal_auth"))
        {
            let command = value
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(default_command)
                .to_string();
            let args = value
                .get("args")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect();
            let env = value
                .get("env")
                .and_then(serde_json::Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.to_string(), value.to_string()))
                })
                .collect();
            return Some(auth::Terminal { command, args, env });
        }
    }

    match method {
        acp::AuthMethod::Terminal(term) => Some(auth::Terminal {
            command: default_command.to_string(),
            args: term.args.clone(),
            env: term
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        }),
        _ => None,
    }
}

fn is_auth_required(err: &helix_acp::Error) -> bool {
    matches!(
        err,
        helix_acp::Error::AgentError(error)
            if matches!(error.code, helix_acp::jsonrpc::ErrorCode::ServerError(-32000))
    )
}

async fn send_auth_required(
    tx: &helix_runtime::Sender<backend::Update>,
    thread: thread::Id,
    methods: &[auth::Method],
    pending_prompt: Option<String>,
    error: Option<String>,
) {
    let _ = tx
        .send(backend::Update::Auth {
            thread,
            event: auth::Event::Required {
                methods: methods.to_vec(),
                pending_prompt,
                error,
            },
        })
        .await;
}

async fn run_terminal_auth(
    host: &host::Set,
    tx: &helix_runtime::Sender<backend::Update>,
    thread: thread::Id,
    method: &auth::Method,
    terminal: &auth::Terminal,
) -> Result<(), helix_acp::Error> {
    let Some(host_terminal) = &host.terminal else {
        return Err(helix_acp::Error::Other(anyhow::anyhow!(
            "terminal host unavailable"
        )));
    };
    let host_id = host_terminal
        .create(host::CreateTerminal {
            command: terminal.command.clone().into(),
            args: terminal.args.clone(),
            cwd: std::env::current_dir().ok(),
            env: terminal
                .env
                .iter()
                .map(|(key, value)| host::Env {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
        })
        .await
        .map_err(|err| helix_acp::Error::Other(anyhow::anyhow!(err.to_string())))?;
    let terminal_id = super::super::terminal::Id::new(host_id.to_string());
    let _ = tx
        .send(backend::Update::Terminal {
            thread,
            event: super::super::terminal::Event::Open(super::super::terminal::Terminal {
                id: terminal_id.clone(),
                title: Some(method.name.clone()),
                state: super::super::terminal::State::Running,
                output: String::new(),
            }),
        })
        .await;
    let status = host_terminal
        .wait(&host_id)
        .await
        .map_err(|err| helix_acp::Error::Other(anyhow::anyhow!(err.to_string())))?;
    let state = match status {
        host::ExitStatus::Code(code) => super::super::terminal::State::Exited { code },
        host::ExitStatus::Other => super::super::terminal::State::Failed {
            message: "terminal exited without status".to_string(),
        },
    };
    let _ = tx
        .send(backend::Update::Terminal {
            thread,
            event: super::super::terminal::Event::Exit {
                id: terminal_id,
                state,
            },
        })
        .await;
    Ok(())
}

struct RunAgent {
    backend_id: backend::Id,
    work: helix_runtime::Work,
    agent: Arc<AcpAgent>,
    tx: helix_runtime::Sender<backend::Update>,
    host: host::Set,
    connect: backend::Connect,
    client_info: acp::Implementation,
    auth_command: String,
}

async fn run_agent(
    runner: RunAgent,
    handle_rx: &mut helix_runtime::Receiver<backend::Command>,
    incoming: &mut IncomingReceiver,
) {
    let RunAgent {
        backend_id,
        work,
        agent,
        tx,
        host,
        connect,
        client_info,
        auth_command,
    } = runner;

    let init = agent
        .initialize(client_info, client_caps(&host))
        .await
        .map(|response| {
            let acp_caps = helix_acp::agent_caps(&response);
            let auth_methods = auth_methods(&response, &auth_command);
            let caps = translate::caps(&response, &connect);
            (caps, acp_caps, auth_methods)
        });

    let (acp_caps, auth_methods) = match init {
        Ok((caps, acp_caps, auth_methods)) => {
            let _ = tx
                .send(backend::Update::Backend {
                    backend: backend_id.clone(),
                    event: backend::Event::Ready { caps },
                })
                .await;
            (acp_caps, auth_methods)
        }
        Err(err) => {
            let _ = tx
                .send(backend::Update::Error {
                    at: backend::Target::Backend(backend_id.clone()),
                    error: backend::Error::Other(anyhow::anyhow!(err.to_string())),
                })
                .await;
            return;
        }
    };

    let mut state = State::new(acp_caps, auth_methods);

    loop {
        tokio::select! {
            cmd = handle_rx.recv() => {
                match cmd {
                    Some(cmd) => handle_command(&backend_id, &work, &agent, &tx, &host, &mut state, cmd).await,
                    None => break,
                }
            }
            msg = incoming.recv() => {
                match msg {
                    Some((_id, call)) => handle_call(&backend_id, &agent, &tx, &host, &mut state, call).await,
                    None => break,
                }
            }
        }
    }

    let _ = tx
        .send(backend::Update::Backend {
            backend: backend_id,
            event: backend::Event::Stopped,
        })
        .await;
}

fn client_caps(host: &host::Set) -> acp::ClientCapabilities {
    acp::ClientCapabilities::new()
        .fs(acp::FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(true))
        .terminal(host.terminal.is_some())
}

async fn handle_command(
    backend_id: &backend::Id,
    work: &helix_runtime::Work,
    agent: &Arc<AcpAgent>,
    tx: &helix_runtime::Sender<backend::Update>,
    host: &host::Set,
    state: &mut State,
    cmd: backend::Command,
) {
    match cmd {
        backend::Command::NewThread { thread, scope } => {
            match agent.new_session(scope.cwd.clone()).await {
                Ok(resp) => {
                    let session = Session::new(resp.session_id.to_string());
                    state.sessions.insert(thread, session.clone());
                    let _ = tx
                        .send(backend::Update::Backend {
                            backend: backend_id.clone(),
                            event: backend::Event::Bound {
                                thread,
                                remote: (&session).into(),
                            },
                        })
                        .await;
                    let _ = tx
                        .send(backend::Update::Thread {
                            thread,
                            event: thread::Event::Caps(state.caps.clone()),
                        })
                        .await;
                    if let Some(modes) = resp.modes.as_ref() {
                        if let Ok(mode_set) = translate::mode_set(modes) {
                            let _ = tx
                                .send(backend::Update::Thread {
                                    thread,
                                    event: thread::Event::Mode(mode_set),
                                })
                                .await;
                        }
                    }
                    if let Some(config) = resp
                        .config_options
                        .as_ref()
                        .and_then(|items| translate::config_state(items).ok())
                    {
                        let _ = tx
                            .send(backend::Update::Thread {
                                thread,
                                event: thread::Event::Config(config),
                            })
                            .await;
                    }
                    let _ = tx
                        .send(backend::Update::Thread {
                            thread,
                            event: thread::Event::Run(thread::Run::Idle),
                        })
                        .await;
                }
                Err(err) => {
                    if is_auth_required(&err) && !state.auth_methods.is_empty() {
                        state.pending_auth = Some(backend::Command::NewThread { thread, scope });
                        send_auth_required(tx, thread, &state.auth_methods, None, None).await;
                        return;
                    }
                    let _ = tx
                        .send(backend::Update::Error {
                            at: backend::Target::Thread(thread),
                            error: backend::Error::Other(anyhow::anyhow!(err.to_string())),
                        })
                        .await;
                }
            }
        }
        backend::Command::LoadThread { thread, remote } => {
            match agent.load_session(remote.to_string().into()).await {
                Ok(resp) => {
                    let session = Session::new(remote.to_string());
                    state.sessions.insert(thread, session.clone());
                    let _ = tx
                        .send(backend::Update::Backend {
                            backend: backend_id.clone(),
                            event: backend::Event::Bound {
                                thread,
                                remote: (&session).into(),
                            },
                        })
                        .await;
                    let _ = tx
                        .send(backend::Update::Thread {
                            thread,
                            event: thread::Event::Caps(state.caps.clone()),
                        })
                        .await;
                    if let Some(modes) = resp.modes.as_ref() {
                        if let Ok(mode_set) = translate::mode_set(modes) {
                            let _ = tx
                                .send(backend::Update::Thread {
                                    thread,
                                    event: thread::Event::Mode(mode_set),
                                })
                                .await;
                        }
                    }
                    if let Some(config) = resp
                        .config_options
                        .as_ref()
                        .and_then(|items| translate::config_state(items).ok())
                    {
                        let _ = tx
                            .send(backend::Update::Thread {
                                thread,
                                event: thread::Event::Config(config),
                            })
                            .await;
                    }
                    let _ = tx
                        .send(backend::Update::Thread {
                            thread,
                            event: thread::Event::Run(thread::Run::Idle),
                        })
                        .await;
                }
                Err(err) => {
                    let _ = tx
                        .send(backend::Update::Error {
                            at: backend::Target::Thread(thread),
                            error: backend::Error::Other(anyhow::anyhow!(err.to_string())),
                        })
                        .await;
                }
            }
        }
        backend::Command::Submit { thread, prompt } => {
            let Some(session) = state.sessions.get(&thread).cloned() else {
                let _ = tx
                    .send(backend::Update::Error {
                        at: backend::Target::Thread(thread),
                        error: backend::Error::Other(anyhow::anyhow!(
                            "thread is not bound to ACP session"
                        )),
                    })
                    .await;
                return;
            };
            let _ = tx
                .send(backend::Update::Thread {
                    thread,
                    event: thread::Event::Run(thread::Run::Running),
                })
                .await;
            let tx2 = tx.clone();
            let agent = agent.clone();
            let auth_methods = state.auth_methods.clone();
            state.pending_auth = Some(backend::Command::Submit {
                thread,
                prompt: prompt.clone(),
            });
            work.spawn(async move {
                let result = agent
                    .prompt(
                        session.to_string().into(),
                        prompt
                            .parts()
                            .iter()
                            .cloned()
                            .map(translate::content_block)
                            .collect(),
                    )
                    .await;
                let event = match result {
                    Ok(_) => backend::Update::Thread {
                        thread,
                        event: thread::Event::Run(thread::Run::Idle),
                    },
                    Err(err) if is_auth_required(&err) => backend::Update::Auth {
                        thread,
                        event: auth::Event::Required {
                            methods: auth_methods,
                            pending_prompt: None,
                            error: Some(err.to_string()),
                        },
                    },
                    Err(err) => backend::Update::Error {
                        at: backend::Target::Thread(thread),
                        error: backend::Error::Other(anyhow::anyhow!(err.to_string())),
                    },
                };
                let _ = tx2.send(event).await;
            })
            .detach();
        }
        backend::Command::Cancel { thread } => {
            if let Some(session) = state.sessions.get(&thread) {
                agent.cancel(session.to_string().into());
                let _ = tx
                    .send(backend::Update::Thread {
                        thread,
                        event: thread::Event::Content(thread::Content::Append(thread::NewEntry {
                            turn: None,
                            kind: thread::EntryKind::Status {
                                text: "Assistant run canceled".to_string(),
                            },
                            locations: Vec::new(),
                        })),
                    })
                    .await;
                let _ = tx
                    .send(backend::Update::Thread {
                        thread,
                        event: thread::Event::Run(thread::Run::Idle),
                    })
                    .await;
            }
        }
        backend::Command::SetMode { thread, mode } => {
            if let Some(session) = state.sessions.get(&thread) {
                let tx2 = tx.clone();
                let agent = agent.clone();
                let session = session.clone();
                work.spawn(async move {
                    let result = agent
                        .set_session_mode(session.to_string().into(), mode.to_string())
                        .await;
                    if let Err(err) = result {
                        let _ = tx2
                            .send(backend::Update::Error {
                                at: backend::Target::Thread(thread),
                                error: backend::Error::Other(anyhow::anyhow!(err.to_string())),
                            })
                            .await;
                    }
                })
                .detach();
            }
        }
        backend::Command::SetConfig {
            thread,
            option,
            value,
        } => {
            if let Some(session) = state.sessions.get(&thread) {
                let tx2 = tx.clone();
                let agent = agent.clone();
                let session = session.clone();
                work.spawn(async move {
                    let result = agent
                        .set_session_config_option(
                            session.to_string().into(),
                            option.to_string(),
                            translate::config_value(&value),
                        )
                        .await;
                    if let Err(err) = result {
                        let _ = tx2
                            .send(backend::Update::Error {
                                at: backend::Target::Thread(thread),
                                error: backend::Error::Other(anyhow::anyhow!(err.to_string())),
                            })
                            .await;
                    }
                })
                .detach();
            }
        }
        backend::Command::CompleteElicitation {
            thread,
            id,
            response,
        } => {
            if let Some(pending) = state.elicitations.remove(&id) {
                let action = elicitation_action(response);
                let status = match &action {
                    acp::ElicitationAction::Accept(_) => thread::ElicitationStatus::Completed,
                    acp::ElicitationAction::Decline => thread::ElicitationStatus::Declined,
                    acp::ElicitationAction::Cancel => thread::ElicitationStatus::Canceled,
                    _ => thread::ElicitationStatus::Canceled,
                };
                agent.reply(
                    pending.rpc,
                    serde_json::to_value(acp::CreateElicitationResponse::new(action)).unwrap(),
                );
                let _ = tx
                    .send(backend::Update::Thread {
                        thread: pending.thread,
                        event: thread::Event::Elicitation(thread::ElicitationEvent::Complete {
                            id,
                            status,
                        }),
                    })
                    .await;
            } else {
                let _ = tx
                    .send(backend::Update::Error {
                        at: backend::Target::Thread(thread),
                        error: backend::Error::Other(anyhow::anyhow!(
                            "elicitation request is no longer pending"
                        )),
                    })
                    .await;
            }
        }
        backend::Command::Authenticate { thread, method } => {
            let Some(auth_method) = state
                .auth_methods
                .iter()
                .find(|item| item.id == method)
                .cloned()
            else {
                let _ = tx
                    .send(backend::Update::Auth {
                        thread,
                        event: auth::Event::Failed {
                            methods: state.auth_methods.clone(),
                            error: "unknown authentication method".to_string(),
                        },
                    })
                    .await;
                return;
            };

            let _ = tx
                .send(backend::Update::Auth {
                    thread,
                    event: auth::Event::Authenticating {
                        method: auth_method.clone(),
                    },
                })
                .await;

            let result = if let Some(terminal) = &auth_method.terminal {
                match run_terminal_auth(host, tx, thread, &auth_method, terminal).await {
                    Ok(()) => agent.authenticate(method.clone()).await.map(|_| ()),
                    Err(err) => Err(err),
                }
            } else {
                agent.authenticate(method.clone()).await.map(|_| ())
            };

            match result {
                Ok(()) => {
                    let _ = tx
                        .send(backend::Update::Auth {
                            thread,
                            event: auth::Event::Succeeded,
                        })
                        .await;
                    if let Some(pending) = state.pending_auth.take() {
                        Box::pin(handle_command(
                            backend_id, work, agent, tx, host, state, pending,
                        ))
                        .await;
                    }
                }
                Err(err) => {
                    let _ = tx
                        .send(backend::Update::Auth {
                            thread,
                            event: auth::Event::Failed {
                                methods: state.auth_methods.clone(),
                                error: err.to_string(),
                            },
                        })
                        .await;
                }
            }
        }
        backend::Command::ResolvePermission {
            request, decision, ..
        } => {
            if let Some(pending) = state.permissions.remove(&request) {
                let outcome = match decision {
                    permission::Decision::Choose(choice) => {
                        if let Some(selected) =
                            pending.choices.iter().find(|item| item.id == choice)
                        {
                            if let Err(err) =
                                state
                                    .rules
                                    .remember(&pending.agent, &pending.tool, selected)
                            {
                                log::warn!("assistant permission rule save failed: {err}");
                            }
                        }
                        acp::RequestPermissionOutcome::Selected(
                            acp::SelectedPermissionOutcome::new(choice.to_string()),
                        )
                    }
                    permission::Decision::Dismiss => acp::RequestPermissionOutcome::Cancelled,
                };
                agent.reply(
                    pending.rpc,
                    serde_json::to_value(acp::RequestPermissionResponse::new(outcome)).unwrap(),
                );
            }
        }
        backend::Command::Review { thread, command } => match command {
            review::Command::SetMode(mode) => {
                state.review_modes.insert(thread, mode);
                let _ = tx
                    .send(backend::Update::Thread {
                        thread,
                        event: thread::Event::Review(review::Event::Mode(mode)),
                    })
                    .await;
            }
            review::Command::Resolve { target, decision } => {
                let resolved = state.resolve(thread, &target, decision);
                for file in &resolved {
                    if decision == review::Decision::Accept {
                        let _ = tx
                            .send(backend::Update::ReviewAcceptedFile {
                                thread,
                                path: file.path.clone(),
                                text: file.after.clone(),
                            })
                            .await;
                    }
                }
                let _ = tx
                    .send(backend::Update::Thread {
                        thread,
                        event: thread::Event::Review(review::Event::Resolve { target, decision }),
                    })
                    .await;
            }
        },
        backend::Command::ListThreads { scope, cursor } => {
            if state.caps.list_sessions {
                match agent
                    .list_sessions(cursor.as_ref().map(|cursor| cursor.as_str().to_string()))
                    .await
                {
                    Ok(resp) => {
                        let entries = resp
                            .sessions
                            .into_iter()
                            .map(|session| {
                                let session_id = session.session_id.to_string();
                                let id = state
                                    .sessions
                                    .iter()
                                    .find(|(_, current)| current.to_string() == session_id)
                                    .map(|(&thread, _)| thread)
                                    .unwrap_or_else(|| thread_id_for_remote(&session_id));
                                super::super::history::Stub {
                                    id,
                                    title: session.title,
                                    scope: thread::Scope {
                                        cwd: session.cwd,
                                        worktrees: session.additional_directories,
                                    },
                                    unread: false,
                                    run: thread::Run::Idle,
                                }
                            })
                            .collect();
                        let _ = tx
                            .send(backend::Update::History {
                                scope,
                                entries,
                                next: resp.next_cursor.map(super::super::history::Cursor::new),
                            })
                            .await;
                    }
                    Err(err) => {
                        let _ = tx
                            .send(backend::Update::Error {
                                at: backend::Target::Backend(backend_id.clone()),
                                error: backend::Error::Other(anyhow::anyhow!(err.to_string())),
                            })
                            .await;
                    }
                }
            } else {
                let entries = state
                    .sessions
                    .keys()
                    .copied()
                    .map(|thread| super::super::history::Stub {
                        id: thread,
                        title: None,
                        scope: scope.clone(),
                        unread: false,
                        run: thread::Run::Idle,
                    })
                    .collect();
                let _ = tx
                    .send(backend::Update::History {
                        scope,
                        entries,
                        next: None,
                    })
                    .await;
            }
        }
        backend::Command::CloseThread { thread } => {
            state.sessions.remove(&thread);
            state.terminals.retain(|_, (owner, _)| *owner != thread);
            state.review_modes.remove(&thread);
            state.staged.remove(&thread);
        }
        backend::Command::DeleteThread { remote } => {
            if state.caps.delete_session {
                if let Err(err) = agent.delete_session(remote.to_string().into()).await {
                    let _ = tx
                        .send(backend::Update::Error {
                            at: backend::Target::Backend(backend_id.clone()),
                            error: backend::Error::Other(anyhow::anyhow!(err.to_string())),
                        })
                        .await;
                }
            }
        }
    }
}

async fn handle_call(
    backend_id: &backend::Id,
    agent: &Arc<AcpAgent>,
    tx: &helix_runtime::Sender<backend::Update>,
    host: &host::Set,
    state: &mut State,
    call: helix_acp::jsonrpc::Call,
) {
    use helix_acp::jsonrpc::Call;
    use helix_acp::types::{AgentMethodCall, AgentNotification};

    match call {
        Call::Notification(helix_acp::jsonrpc::Notification { method, params, .. }) => {
            match AgentNotification::parse(&method, params) {
                Ok(AgentNotification::SessionUpdate(notif)) => {
                    let session = Session::new(notif.session_id.to_string());
                    if let Some((&thread, _)) = state
                        .sessions
                        .iter()
                        .find(|(_, current)| **current == session)
                    {
                        let locations = translate::update_locations(&notif.update);
                        if let Some(event) = translate::thread_event(notif.update) {
                            let _ = tx.send(backend::Update::Thread { thread, event }).await;
                        }
                        for location in locations {
                            let _ = tx
                                .send(backend::Update::Location { thread, location })
                                .await;
                        }
                    }
                }
                Ok(AgentNotification::CompleteElicitation(notif)) => {
                    let id = notif.elicitation_id.to_string();
                    for &thread in state.sessions.keys() {
                        let _ = tx
                            .send(backend::Update::Thread {
                                thread,
                                event: thread::Event::Elicitation(
                                    thread::ElicitationEvent::Complete {
                                        id: id.clone(),
                                        status: thread::ElicitationStatus::Completed,
                                    },
                                ),
                            })
                            .await;
                    }
                }
                Err(_) => {}
            }
        }
        Call::MethodCall(helix_acp::jsonrpc::MethodCall {
            method, params, id, ..
        }) => match AgentMethodCall::parse(&method, params) {
            Ok(AgentMethodCall::ReadTextFile(req)) => {
                let thread = thread_for_session(state, &req.session_id);
                let staged = thread
                    .and_then(|thread| state.staged_text(thread, std::path::Path::new(&req.path)));
                match if let Some(content) = staged {
                    Ok(content)
                } else {
                    host.fs.read_text(std::path::Path::new(&req.path)).await
                } {
                    Ok(content) => {
                        if let Some(thread) = thread {
                            let _ = tx
                                .send(backend::Update::Location {
                                    thread,
                                    location: crate::collab::Location::new(
                                        req.path.clone(),
                                        crate::collab::location::Source::Read,
                                    ),
                                })
                                .await;
                        }
                        agent.reply(
                            id,
                            serde_json::to_value(acp::ReadTextFileResponse::new(content)).unwrap(),
                        )
                    }
                    Err(err) => agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error(err.to_string()),
                    ),
                }
            }
            Ok(AgentMethodCall::WriteTextFile(req)) => {
                let thread = thread_for_session(state, &req.session_id);
                let path = PathBuf::from(req.path.clone());
                let previous = match thread.and_then(|thread| state.staged_text(thread, &path)) {
                    Some(content) => Some(content),
                    None => host.fs.read_text(&path).await.ok(),
                };
                let content = req.content;
                if let Some(thread) = thread {
                    let mode = state.review_mode(thread);
                    if mode == review::Mode::Review {
                        let file = review::File::staged(
                            path.clone(),
                            previous.clone().unwrap_or_default(),
                            content.clone(),
                        );
                        state.stage(thread, file.clone());
                        let _ = tx
                            .send(backend::Update::Thread {
                                thread,
                                event: thread::Event::Review(review::Event::Stage { file, mode }),
                            })
                            .await;
                        agent.reply(
                            id,
                            serde_json::to_value(acp::WriteTextFileResponse::new()).unwrap(),
                        );
                        return;
                    }
                }
                match host
                    .fs
                    .write_text(host::Write {
                        path: path.clone(),
                        text: content.clone(),
                    })
                    .await
                {
                    Ok(()) => {
                        if let Some(thread) = thread {
                            let file = review::File::written(
                                path.clone(),
                                previous.clone().unwrap_or_default(),
                                content.clone(),
                            );
                            let _ = tx
                                .send(backend::Update::Thread {
                                    thread,
                                    event: thread::Event::Review(review::Event::Stage {
                                        file,
                                        mode: review::Mode::Write,
                                    }),
                                })
                                .await;
                            let _ = tx
                                .send(backend::Update::Location {
                                    thread,
                                    location: write_location(path, previous.as_deref(), &content),
                                })
                                .await;
                        }
                        agent.reply(
                            id,
                            serde_json::to_value(acp::WriteTextFileResponse::new()).unwrap(),
                        )
                    }
                    Err(err) => agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error(err.to_string()),
                    ),
                }
            }
            Ok(AgentMethodCall::CreateTerminal(req)) => {
                let Some(thread) = thread_for_session(state, &req.session_id) else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::invalid_params("unknown session"),
                    );
                    return;
                };
                let title = req.command.clone();
                let Some(terminal) = &host.terminal else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error("terminal host unavailable"),
                    );
                    return;
                };
                let create = host::CreateTerminal {
                    command: req.command.into(),
                    args: req.args,
                    cwd: req.cwd.map(Into::into),
                    env: req
                        .env
                        .into_iter()
                        .map(|env| host::Env {
                            key: env.name,
                            value: env.value,
                        })
                        .collect(),
                };
                match terminal.create(create).await {
                    Ok(host_id) => {
                        state
                            .terminals
                            .insert(host_id.to_string(), (thread, host_id.clone()));
                        let _ = tx
                            .send(backend::Update::Terminal {
                                thread,
                                event: super::super::terminal::Event::Open(
                                    super::super::terminal::Terminal {
                                        id: super::super::terminal::Id::new(host_id.to_string()),
                                        title: Some(title),
                                        state: super::super::terminal::State::Running,
                                        output: String::new(),
                                    },
                                ),
                            })
                            .await;
                        agent.reply(
                            id,
                            serde_json::to_value(acp::CreateTerminalResponse::new(
                                host_id.to_string(),
                            ))
                            .unwrap(),
                        );
                    }
                    Err(err) => agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error(err.to_string()),
                    ),
                }
            }
            Ok(AgentMethodCall::TerminalOutput(req)) => {
                let terminal_id = req.terminal_id.to_string();
                let Some((thread, term)) = state.terminals.get(&terminal_id) else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::invalid_params("unknown terminal"),
                    );
                    return;
                };
                let Some(terminal) = &host.terminal else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error("terminal host unavailable"),
                    );
                    return;
                };
                match terminal.output(term).await {
                    Ok(output) => {
                        let _ = tx
                            .send(backend::Update::Terminal {
                                thread: *thread,
                                event: super::super::terminal::Event::Output {
                                    id: super::super::terminal::Id::new(terminal_id.clone()),
                                    chunk: output.clone(),
                                },
                            })
                            .await;
                        agent.reply(
                            id,
                            serde_json::to_value(acp::TerminalOutputResponse::new(output, false))
                                .unwrap(),
                        )
                    }
                    Err(err) => agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error(err.to_string()),
                    ),
                }
            }
            Ok(AgentMethodCall::WaitForTerminalExit(req)) => {
                let terminal_id = req.terminal_id.to_string();
                let Some((thread, term)) = state.terminals.get(&terminal_id) else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::invalid_params("unknown terminal"),
                    );
                    return;
                };
                let Some(terminal) = &host.terminal else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error("terminal host unavailable"),
                    );
                    return;
                };
                match terminal.wait(term).await {
                    Ok(status) => {
                        let (exit_code, state) = match status {
                            host::ExitStatus::Code(code) => {
                                (Some(code), super::super::terminal::State::Exited { code })
                            }
                            host::ExitStatus::Other => (
                                None,
                                super::super::terminal::State::Failed {
                                    message: "terminal exited without status".to_string(),
                                },
                            ),
                        };
                        let _ = tx
                            .send(backend::Update::Terminal {
                                thread: *thread,
                                event: super::super::terminal::Event::Exit {
                                    id: super::super::terminal::Id::new(terminal_id.clone()),
                                    state,
                                },
                            })
                            .await;
                        agent.reply(
                            id,
                            serde_json::to_value(acp::WaitForTerminalExitResponse::new(
                                terminal_exit_status(exit_code),
                            ))
                            .unwrap(),
                        );
                    }
                    Err(err) => agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error(err.to_string()),
                    ),
                }
            }
            Ok(AgentMethodCall::KillTerminal(req)) => {
                let terminal_id = req.terminal_id.to_string();
                let Some((_, term)) = state.terminals.get(&terminal_id) else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::invalid_params("unknown terminal"),
                    );
                    return;
                };
                let Some(terminal) = &host.terminal else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error("terminal host unavailable"),
                    );
                    return;
                };
                match terminal.kill(term).await {
                    Ok(()) => agent.reply(
                        id,
                        serde_json::to_value(acp::KillTerminalResponse::new()).unwrap(),
                    ),
                    Err(err) => agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error(err.to_string()),
                    ),
                }
            }
            Ok(AgentMethodCall::ReleaseTerminal(req)) => {
                let terminal_id = req.terminal_id.to_string();
                let Some((_, term)) = state.terminals.remove(&terminal_id) else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::invalid_params("unknown terminal"),
                    );
                    return;
                };
                let Some(terminal) = &host.terminal else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error("terminal host unavailable"),
                    );
                    return;
                };
                match terminal.release(&term).await {
                    Ok(()) => agent.reply(
                        id,
                        serde_json::to_value(acp::ReleaseTerminalResponse::new()).unwrap(),
                    ),
                    Err(err) => agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error(err.to_string()),
                    ),
                }
            }
            Ok(AgentMethodCall::RequestPermission(req)) => {
                let session = Session::new(req.session_id.to_string());
                let Some((&thread, _)) = state
                    .sessions
                    .iter()
                    .find(|(_, current)| **current == session)
                else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::invalid_params("unknown session"),
                    );
                    return;
                };
                let mut options = req.options.into_iter();
                let Some(first) = options.next() else {
                    agent.reply(
                        id,
                        serde_json::to_value(acp::RequestPermissionResponse::new(
                            acp::RequestPermissionOutcome::Cancelled,
                        ))
                        .unwrap(),
                    );
                    return;
                };
                let request_id = permission::RequestId::new(format!("perm:{:?}", &id));
                let tool = req
                    .tool_call
                    .fields
                    .title
                    .clone()
                    .unwrap_or_else(|| req.tool_call.tool_call_id.to_string());
                let description = req
                    .tool_call
                    .fields
                    .content
                    .as_deref()
                    .map(permission_description)
                    .unwrap_or_default();
                let mut builder = permission::Request::builder(
                    request_id.clone(),
                    thread,
                    tool.clone(),
                    description,
                )
                .choice(permission_choice(first));
                for option in options {
                    builder = builder.choice(permission_choice(option));
                }
                let request = builder.build();
                if let Some(choice) =
                    state
                        .rules
                        .choice(backend_id.as_str(), &tool, request.choices())
                {
                    let verb = request
                        .choices()
                        .iter()
                        .find(|item| item.id == choice)
                        .map(|item| match item.kind {
                            permission::Kind::RejectAlways | permission::Kind::RejectOnce => {
                                "auto-rejected"
                            }
                            _ => "auto-allowed",
                        })
                        .unwrap_or("auto-allowed");
                    agent.reply(
                        id,
                        serde_json::to_value(acp::RequestPermissionResponse::new(
                            acp::RequestPermissionOutcome::Selected(
                                acp::SelectedPermissionOutcome::new(choice.to_string()),
                            ),
                        ))
                        .unwrap(),
                    );
                    let _ = tx
                        .send(backend::Update::Thread {
                            thread,
                            event: thread::Event::Content(thread::Content::Append(
                                thread::NewEntry {
                                    turn: None,
                                    kind: thread::EntryKind::Status {
                                        text: format!("{verb} {tool} (always)"),
                                    },
                                    locations: Vec::new(),
                                },
                            )),
                        })
                        .await;
                    return;
                }
                state.permissions.insert(
                    request_id,
                    PendingPermission {
                        rpc: id,
                        agent: backend_id.as_str().to_string(),
                        tool,
                        choices: request.choices().to_vec(),
                    },
                );
                let _ = tx
                    .send(backend::Update::Permission { thread, request })
                    .await;
            }
            Ok(AgentMethodCall::CreateElicitation(req)) => {
                let session_id = match req.scope() {
                    acp::ElicitationScope::Session(scope) => scope.session_id.to_string(),
                    acp::ElicitationScope::Request(_) => {
                        agent.reply_error(
                            id,
                            helix_acp::jsonrpc::Error::invalid_params("missing session scope"),
                        );
                        return;
                    }
                    _ => {
                        agent.reply_error(
                            id,
                            helix_acp::jsonrpc::Error::invalid_params("unknown elicitation scope"),
                        );
                        return;
                    }
                };
                let Some(thread) = thread_for_session(state, session_id) else {
                    agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::invalid_params("unknown session"),
                    );
                    return;
                };
                let elicitation = translate::elicitation(req);
                state.elicitations.insert(
                    elicitation.id.clone(),
                    PendingElicitation { rpc: id, thread },
                );
                let _ = tx
                    .send(backend::Update::Thread {
                        thread,
                        event: thread::Event::Elicitation(thread::ElicitationEvent::Request(
                            elicitation,
                        )),
                    })
                    .await;
            }
            Err(err) => {
                let _ = tx
                    .send(backend::Update::Error {
                        at: backend::Target::Backend(backend_id.clone()),
                        error: backend::Error::Other(anyhow::anyhow!(err.to_string())),
                    })
                    .await;
            }
        },
        _ => {}
    }
}

fn permission_choice(option: acp::PermissionOption) -> permission::Choice {
    let kind = match option.kind {
        acp::PermissionOptionKind::AllowOnce => permission::Kind::AllowOnce,
        acp::PermissionOptionKind::AllowAlways => permission::Kind::AllowAlways,
        acp::PermissionOptionKind::RejectOnce => permission::Kind::RejectOnce,
        acp::PermissionOptionKind::RejectAlways => permission::Kind::RejectAlways,
        _ => permission::Kind::from_label(&option.option_id.to_string(), &option.name),
    };
    permission::Choice::new(
        permission::ChoiceId::new(option.option_id.to_string()),
        option.name,
        kind,
    )
}

fn permission_description(content: &[acp::ToolCallContent]) -> String {
    content
        .iter()
        .map(|item| match item {
            acp::ToolCallContent::Content(content) => match &content.content {
                acp::ContentBlock::Text(text) => text.text.clone(),
                acp::ContentBlock::ResourceLink(link) => link.uri.clone(),
                acp::ContentBlock::Resource(resource) => match &resource.resource {
                    acp::EmbeddedResourceResource::TextResourceContents(resource) => {
                        resource.text.clone()
                    }
                    acp::EmbeddedResourceResource::BlobResourceContents(resource) => {
                        resource.uri.clone()
                    }
                    _ => String::new(),
                },
                _ => String::new(),
            },
            acp::ToolCallContent::Diff(diff) => diff.path.display().to_string(),
            acp::ToolCallContent::Terminal(term) => format!("terminal:{}", term.terminal_id),
            _ => String::new(),
        })
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn terminal_exit_status(exit_code: Option<i32>) -> acp::TerminalExitStatus {
    acp::TerminalExitStatus::new().exit_code(exit_code.and_then(|code| u32::try_from(code).ok()))
}

fn write_location(path: PathBuf, previous: Option<&str>, current: &str) -> crate::collab::Location {
    let mut location = crate::collab::Location::new(path, crate::collab::location::Source::Write);
    if let Some(range) = changed_range(previous.unwrap_or_default(), current) {
        location = location.with_range(range);
    }
    location
}

fn changed_range(before: &str, after: &str) -> Option<crate::collab::RangeAnchor> {
    let before: Vec<_> = before.chars().collect();
    let after: Vec<_> = after.chars().collect();

    let mut prefix = 0;
    while prefix < before.len() && prefix < after.len() && before[prefix] == after[prefix] {
        prefix += 1;
    }

    if prefix == before.len() && prefix == after.len() {
        return None;
    }

    let mut before_suffix = before.len();
    let mut after_suffix = after.len();
    while before_suffix > prefix
        && after_suffix > prefix
        && before[before_suffix - 1] == after[after_suffix - 1]
    {
        before_suffix -= 1;
        after_suffix -= 1;
    }

    Some(crate::collab::RangeAnchor::new(prefix, after_suffix))
}

#[cfg(test)]
mod tests {
    use super::{changed_range, write_location, State};
    use crate::assistant::{review, thread};
    use std::num::NonZeroU64;

    #[test]
    fn changed_range_tracks_insertions() {
        let range = changed_range("hello world", "hello brave world").expect("range");
        assert_eq!(range.anchor, 6);
        assert_eq!(range.head, 12);
    }

    #[test]
    fn changed_range_tracks_deletions_as_points() {
        let range = changed_range("hello brave world", "hello world").expect("range");
        assert_eq!(range.anchor, 6);
        assert_eq!(range.head, 6);
    }

    #[test]
    fn write_location_uses_full_new_file_for_creates() {
        let location = write_location(std::path::PathBuf::from("file.rs"), None, "abc");
        let range = location.range.expect("range");
        assert_eq!(range.anchor, 0);
        assert_eq!(range.head, 3);
    }

    #[test]
    fn staged_overlay_reads_through_until_resolved() {
        let mut state = State::new(helix_acp::AgentCaps::default(), Vec::new());
        let thread = thread::Id::new(NonZeroU64::new(1).unwrap());
        let path = std::path::PathBuf::from("file.rs");
        state.stage(
            thread,
            review::File::staged(path.clone(), "old".into(), "new".into()),
        );

        assert_eq!(state.staged_text(thread, &path).as_deref(), Some("new"));

        let resolved = state.resolve(
            thread,
            &review::Target::File(path.clone()),
            review::Decision::Reject,
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].status, review::Status::Rejected);
        assert_eq!(state.staged_text(thread, &path), None);
    }
}
