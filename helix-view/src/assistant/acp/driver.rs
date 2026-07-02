use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use helix_acp::client::{AgentConfig, IncomingReceiver};
use helix_acp::types as acp;
use helix_acp::AcpAgent;

use super::super::{backend, host, permission, thread};
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
    rules: permission::Rules,
}

struct PendingPermission {
    rpc: helix_acp::jsonrpc::Id,
    agent: String,
    tool: String,
    choices: Vec<permission::Choice>,
}

impl State {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            terminals: HashMap::new(),
            permissions: HashMap::new(),
            rules: permission::Rules::load(),
        }
    }
}

fn thread_for_session(state: &State, session_id: &str) -> Option<thread::Id> {
    let session = Session::new(session_id.to_string());
    state
        .sessions
        .iter()
        .find(|(_, current)| **current == session)
        .map(|(&thread, _)| thread)
}

struct RunAgent {
    backend_id: backend::Id,
    work: helix_runtime::Work,
    agent: Arc<AcpAgent>,
    tx: helix_runtime::Sender<backend::Update>,
    host: host::Set,
    connect: backend::Connect,
    client_info: acp::Implementation,
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
    } = runner;

    let init = agent
        .initialize(client_info, client_caps(&host))
        .await
        .map(|response| translate::caps(&response, &connect));

    match init {
        Ok(caps) => {
            let _ = tx
                .send(backend::Update::Backend {
                    backend: backend_id.clone(),
                    event: backend::Event::Ready { caps },
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
            return;
        }
    }

    let mut state = State::new();

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
    acp::ClientCapabilities {
        fs: Some(acp::FileSystemCapabilities {
            read_text_file: Some(true),
            write_text_file: Some(true),
        }),
        terminal: Some(host.terminal.is_some()),
    }
}

async fn handle_command(
    backend_id: &backend::Id,
    work: &helix_runtime::Work,
    agent: &Arc<AcpAgent>,
    tx: &helix_runtime::Sender<backend::Update>,
    _host: &host::Set,
    state: &mut State,
    cmd: backend::Command,
) {
    match cmd {
        backend::Command::NewThread { thread, scope } => match agent.new_session(scope.cwd).await {
            Ok(resp) => {
                let session = Session::new(resp.session_id.clone());
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
                if let Some(modes) = resp.session_modes.as_ref() {
                    if let Ok(mode_set) = translate::mode_set(
                        modes.first().map(|m| m.id.as_ref()).unwrap_or_default(),
                        modes,
                    ) {
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
        },
        backend::Command::LoadThread { thread, remote } => {
            match agent.load_session(remote.to_string()).await {
                Ok(resp) => {
                    let session = Session::new(resp.session_id.clone());
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
                    if let Some(modes) = resp.session_modes.as_ref() {
                        if let Ok(mode_set) = translate::mode_set(
                            modes.first().map(|m| m.id.as_ref()).unwrap_or_default(),
                            modes,
                        ) {
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
            work.spawn(async move {
                let result = agent
                    .prompt(
                        session.to_string(),
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
                agent.cancel(session.to_string());
                let _ = tx
                    .send(backend::Update::Thread {
                        thread,
                        event: thread::Event::Content(thread::Content::Append(
                            thread::NewEntry {
                                turn: None,
                                kind: thread::EntryKind::Status {
                                    text: "Assistant run canceled".to_string(),
                                },
                                locations: Vec::new(),
                            },
                        )),
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
                        .set_session_mode(session.to_string(), mode.to_string())
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
                            session.to_string(),
                            option.to_string(),
                            value.to_string(),
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
                        acp::RequestPermissionOutcome::Selected {
                            id: choice.to_string(),
                        }
                    }
                    permission::Decision::Dismiss => acp::RequestPermissionOutcome::Dismissed,
                };
                agent.reply(
                    pending.rpc,
                    serde_json::to_value(acp::RequestPermissionResponse { outcome }).unwrap(),
                );
            }
        }
        backend::Command::ListThreads { scope, cursor } => {
            let _ = cursor;
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
        backend::Command::CloseThread { thread } => {
            state.sessions.remove(&thread);
            state.terminals.retain(|_, (owner, _)| *owner != thread);
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
                    let session = Session::new(notif.session_id.clone());
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
                Err(_) => {}
            }
        }
        Call::MethodCall(helix_acp::jsonrpc::MethodCall {
            method, params, id, ..
        }) => match AgentMethodCall::parse(&method, params) {
            Ok(AgentMethodCall::ReadTextFile(req)) => {
                let thread = thread_for_session(state, &req.session_id);
                match host.fs.read_text(std::path::Path::new(&req.path)).await {
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
                            serde_json::to_value(acp::ReadTextFileResponse { content }).unwrap(),
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
                let previous = host.fs.read_text(&path).await.ok();
                let content = req.content;
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
                            let _ = tx
                                .send(backend::Update::Location {
                                    thread,
                                    location: write_location(path, previous.as_deref(), &content),
                                })
                                .await;
                        }
                        agent.reply(
                            id,
                            serde_json::to_value(acp::WriteTextFileResponse {}).unwrap(),
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
                    args: req.args.unwrap_or_default(),
                    cwd: req.cwd.map(Into::into),
                    env: req
                        .env
                        .unwrap_or_default()
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
                            serde_json::to_value(acp::CreateTerminalResponse {
                                terminal_id: host_id.to_string(),
                            })
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
                let Some((thread, term)) = state.terminals.get(&req.terminal_id) else {
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
                                    id: super::super::terminal::Id::new(req.terminal_id.clone()),
                                    chunk: output.clone(),
                                },
                            })
                            .await;
                        agent.reply(
                            id,
                            serde_json::to_value(acp::TerminalOutputResponse {
                                output,
                                truncated: false,
                                exit_status: None,
                            })
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
                let Some((thread, term)) = state.terminals.get(&req.terminal_id) else {
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
                        let (exit_code, signal, state) = match status {
                            host::ExitStatus::Code(code) => (
                                Some(code),
                                None,
                                super::super::terminal::State::Exited { code },
                            ),
                            host::ExitStatus::Other => (
                                None,
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
                                    id: super::super::terminal::Id::new(req.terminal_id.clone()),
                                    state,
                                },
                            })
                            .await;
                        agent.reply(
                            id,
                            serde_json::to_value(acp::WaitForTerminalExitResponse {
                                exit_code,
                                signal,
                            })
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
                let Some((_, term)) = state.terminals.get(&req.terminal_id) else {
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
                        serde_json::to_value(acp::KillTerminalResponse {}).unwrap(),
                    ),
                    Err(err) => agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error(err.to_string()),
                    ),
                }
            }
            Ok(AgentMethodCall::ReleaseTerminal(req)) => {
                let Some((_, term)) = state.terminals.remove(&req.terminal_id) else {
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
                        serde_json::to_value(acp::ReleaseTerminalResponse {}).unwrap(),
                    ),
                    Err(err) => agent.reply_error(
                        id,
                        helix_acp::jsonrpc::Error::internal_error(err.to_string()),
                    ),
                }
            }
            Ok(AgentMethodCall::RequestPermission(req)) => {
                let session = Session::new(req.session_id.clone());
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
                let mut options = req.permissions.into_iter();
                let Some(first) = options.next() else {
                    agent.reply(
                        id,
                        serde_json::to_value(acp::RequestPermissionResponse {
                            outcome: acp::RequestPermissionOutcome::Dismissed,
                        })
                        .unwrap(),
                    );
                    return;
                };
                let request_id = permission::RequestId::new(format!("perm:{:?}", &id));
                let tool = req.title;
                let mut builder = permission::Request::builder(
                    request_id.clone(),
                    thread,
                    tool.clone(),
                    req.description.unwrap_or_default(),
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
                        serde_json::to_value(acp::RequestPermissionResponse {
                            outcome: acp::RequestPermissionOutcome::Selected {
                                id: choice.to_string(),
                            },
                        })
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
    let kind = permission::Kind::from_label(&option.id, &option.title);
    permission::Choice::new(permission::ChoiceId::new(option.id), option.title, kind)
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
    use super::{changed_range, write_location};

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
}
