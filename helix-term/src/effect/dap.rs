use std::path::PathBuf;

use helix_core::syntax::config::DebugAdapterConfig;
use helix_dap::{
    events, registry::DebugAdapterId, requests, ConnectionType, Event, Request, ServerEvent,
    ServerMessageError, StackFrame, ThreadId as DebugThreadId,
};
use helix_view::{editor::Action, Editor};
use serde_json::json;

use crate::runtime::{
    ingress::{DapConfiguredBreakpoints, DapOperation, DapParentRequest, DapSessionRequest},
    send_task_event_with, RuntimeIngress, RuntimeTaskEvent,
};

pub(crate) fn handle_message(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    client_id: DebugAdapterId,
    event: ServerEvent,
) -> bool {
    match event {
        ServerEvent::Event(raw) => {
            let event = match raw.event {
                Ok(event) => event,
                Err(ServerMessageError::Unhandled) => {
                    log::info!("Discarding unknown DAP event '{}'", raw.name);
                    return false;
                }
                Err(ServerMessageError::Invalid(error)) => {
                    log::warn!("Discarding invalid DAP event '{}': {error}", raw.name);
                    return false;
                }
            };
            handle_event(editor, ingress, client_id, event)
        }
        ServerEvent::Request(raw) => {
            let parent = DapParentRequest {
                client_id,
                sequence: raw.sequence,
                command: raw.command,
            };
            match raw.request {
                Ok(Request::StartDebugging(arguments)) => {
                    start_child_debugger(editor, ingress, parent, arguments)
                }
                Ok(request) => handle_adapter_request(editor, ingress, parent, request),
                Err(ServerMessageError::Unhandled) => reply_to_adapter(
                    editor,
                    ingress,
                    parent,
                    Err("Unhandled debugger request".to_owned()),
                ),
                Err(ServerMessageError::Invalid(error)) => {
                    reply_to_adapter(editor, ingress, parent, Err(error))
                }
            }
            true
        }
        ServerEvent::UnexpectedResponse(_) => {
            log::warn!("discarding unexpected raw DAP response for {client_id}");
            false
        }
    }
}

fn handle_event(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    client_id: DebugAdapterId,
    event: Event,
) -> bool {
    match event {
        Event::Stopped(body) => {
            let all_threads_stopped = body.all_threads_stopped.unwrap_or_default();
            let Some(debugger) = editor.debug_adapters.get_client_mut(client_id) else {
                return false;
            };
            if let Some(thread_id) = body.thread_id {
                debugger
                    .thread_states
                    .insert(thread_id, body.reason.clone());
            }
            let request = debugger.request_handle();
            let generation = ingress.begin_dap_operation(client_id, DapOperation::StackTrace);
            let work = editor.work();
            let preferred_thread_id = body.thread_id;
            work.spawn(async move {
                let mut stacks = Vec::new();
                let mut errors = Vec::new();
                let thread_ids = if all_threads_stopped {
                    match request
                        .request::<requests::Threads>(Some(requests::ThreadsArguments {}))
                        .await
                    {
                        Ok(response) => response
                            .threads
                            .into_iter()
                            .map(|thread| thread.id)
                            .collect(),
                        Err(error) => {
                            errors.push(format!("Failed to list debugger threads: {error}"));
                            Vec::new()
                        }
                    }
                } else {
                    preferred_thread_id.into_iter().collect()
                };

                let results =
                    futures_util::future::join_all(thread_ids.into_iter().map(|thread_id| {
                        let request = request.clone();
                        async move {
                            let arguments = requests::StackTraceArguments {
                                thread_id,
                                start_frame: None,
                                levels: None,
                                format: None,
                            };
                            (
                                thread_id,
                                request.request::<requests::StackTrace>(arguments).await,
                            )
                        }
                    }))
                    .await;
                for (thread_id, result) in results {
                    match result {
                        Ok(response) => stacks.push((thread_id, response.stack_frames)),
                        Err(error) => errors.push(format!(
                            "Failed to fetch stack trace for thread {thread_id}: {error}"
                        )),
                    }
                }

                send_task_event_with(
                    RuntimeTaskEvent::DapStoppedCompleted {
                        client_id,
                        generation,
                        preferred_thread_id,
                        stacks,
                        errors,
                    },
                    ingress,
                )
                .await;
            })
            .detach();

            editor.set_status(stopped_status(&body, all_threads_stopped));
        }
        Event::Continued(events::ContinuedBody { thread_id, .. }) => {
            ingress.begin_dap_operation(client_id, DapOperation::StackTrace);
            let Some(debugger) = editor.debug_adapters.get_client_mut(client_id) else {
                return false;
            };
            debugger
                .thread_states
                .insert(thread_id, "running".to_owned());
            if debugger.thread_id == Some(thread_id) {
                debugger.resume_application();
                clear_inline_values(editor);
            }
        }
        Event::Thread(thread) => {
            editor.set_status(format!("Thread {}: {}", thread.thread_id, thread.reason));
            let Some(debugger) = editor.debug_adapters.get_client_mut(client_id) else {
                return false;
            };
            debugger.thread_id = Some(thread.thread_id);
        }
        Event::Breakpoint(events::BreakpointBody { reason, breakpoint }) => {
            apply_breakpoint_event(editor, &reason, breakpoint);
        }
        Event::Output(events::OutputBody {
            category, output, ..
        }) => {
            if category.as_deref() == Some("telemetry") {
                log::debug!("DAP telemetry: {output}");
                return false;
            }
            let prefix = category
                .map(|category| format!("Debug ({category}):"))
                .unwrap_or_else(|| "Debug:".to_owned());
            log::info!("{output}");
            editor.set_status(format!("{prefix} {output}"));
        }
        Event::Initialized(_) => request_configuration(editor, ingress, client_id),
        Event::Terminated(terminated) => request_termination(
            editor,
            ingress,
            client_id,
            terminated.and_then(|body| body.restart),
        ),
        Event::Exited(response) => {
            ingress.begin_dap_operation(client_id, DapOperation::StackTrace);
            if response.exit_code != 0 {
                editor.set_error(format!(
                    "Debuggee failed to exit successfully (exit code: {}).",
                    response.exit_code
                ));
            }
        }
        event => {
            log::warn!("Unhandled DAP event {event:?}");
            return false;
        }
    }
    true
}

fn stopped_status(body: &events::StoppedBody, all_threads_stopped: bool) -> String {
    let scope = body
        .thread_id
        .map(|id| format!("Thread {id}"))
        .unwrap_or_else(|| "Target".to_owned());
    let mut status = format!("{scope} stopped because of {}", body.reason);
    if let Some(description) = &body.description {
        status.push(' ');
        status.push_str(description);
    }
    if let Some(text) = &body.text {
        status.push(' ');
        status.push_str(text);
    }
    if all_threads_stopped {
        status.push_str(" (all threads stopped)");
    }
    status
}

fn apply_breakpoint_event(editor: &mut Editor, reason: &str, breakpoint: helix_dap::Breakpoint) {
    match reason {
        "new" => {
            let Some(path) = breakpoint.source.and_then(|source| source.path) else {
                log::warn!("ignoring new DAP breakpoint without a source path");
                return;
            };
            let Some(line) = breakpoint.line else {
                log::warn!("ignoring new DAP breakpoint without a source line");
                return;
            };
            editor
                .breakpoints
                .entry(path)
                .or_default()
                .push(helix_view::editor::Breakpoint {
                    id: breakpoint.id,
                    verified: breakpoint.verified,
                    message: breakpoint.message,
                    line: line.saturating_sub(1),
                    column: breakpoint.column,
                    ..Default::default()
                });
        }
        "changed" => {
            for breakpoints in editor.breakpoints.values_mut() {
                let Some(current) = breakpoints
                    .iter_mut()
                    .find(|current| current.id == breakpoint.id)
                else {
                    continue;
                };
                current.verified = breakpoint.verified;
                if breakpoint.message.is_some() {
                    current.message = breakpoint.message.clone();
                }
                if let Some(line) = breakpoint.line {
                    current.line = line.saturating_sub(1);
                }
                if breakpoint.column.is_some() {
                    current.column = breakpoint.column;
                }
            }
        }
        "removed" => {
            for breakpoints in editor.breakpoints.values_mut() {
                breakpoints.retain(|current| current.id != breakpoint.id);
            }
        }
        reason => log::warn!("Unknown breakpoint event: {reason}"),
    }
}

fn request_configuration(editor: &mut Editor, ingress: RuntimeIngress, client_id: DebugAdapterId) {
    let Some(debugger) = editor.debug_adapters.get_client(client_id) else {
        return;
    };
    let request = debugger.request_handle();
    let snapshots = editor
        .breakpoints
        .iter()
        .map(|(path, breakpoints)| {
            (
                path.clone(),
                breakpoints.clone(),
                helix_view::handlers::dap::source_breakpoints(breakpoints),
            )
        })
        .collect::<Vec<_>>();
    let generation = ingress.begin_dap_operation(client_id, DapOperation::Configuration);
    let work = editor.work();
    editor.set_status("Debugger initialized...");
    work.spawn(async move {
        let configured = futures_util::future::join_all(snapshots.into_iter().map(
            |(path, expected, source)| {
                let request = request.clone();
                async move {
                    let result = request
                        .set_breakpoints(path.clone(), source)
                        .await
                        .map_err(|error| error.to_string());
                    DapConfiguredBreakpoints {
                        path,
                        expected,
                        result,
                    }
                }
            },
        ))
        .await;
        let configuration_result = request
            .request::<requests::ConfigurationDone>(Some(requests::ConfigurationDoneArguments {}))
            .await
            .map(|_| ())
            .map_err(|error| error.to_string());
        send_task_event_with(
            RuntimeTaskEvent::DapInitializedCompleted {
                client_id,
                generation,
                breakpoints: configured,
                configuration_result,
            },
            ingress,
        )
        .await;
    })
    .detach();
}

fn request_termination(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    client_id: DebugAdapterId,
    restart: Option<serde_json::Value>,
) {
    ingress.begin_dap_operation(client_id, DapOperation::StackTrace);
    ingress.begin_dap_operation(client_id, DapOperation::Configuration);
    let generation = ingress.begin_dap_operation(client_id, DapOperation::Termination);
    let Some(debugger) = editor.debug_adapters.get_client_mut(client_id) else {
        return;
    };
    let connection_type = debugger.connection_type();
    let restart_requested = restart
        .as_ref()
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let request = debugger.disconnect(Some(requests::DisconnectArguments {
        restart: Some(restart_requested),
        terminate_debuggee: None,
        suspend_debuggee: None,
    }));
    let work = editor.work();
    work.spawn(async move {
        let result = request.await.map(|_| ()).map_err(|error| error.to_string());
        send_task_event_with(
            RuntimeTaskEvent::DapDisconnectCompleted {
                client_id,
                generation,
                restart,
                connection_type,
                result,
            },
            ingress,
        )
        .await;
    })
    .detach();
}

fn handle_adapter_request(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    parent: DapParentRequest,
    request: Request,
) {
    match request {
        Request::RunInTerminal(arguments) => {
            let Some(config) = editor.config().terminal.clone() else {
                editor.set_error("No external terminal defined");
                reply_to_adapter(
                    editor,
                    ingress,
                    parent,
                    Err("No external terminal defined".into()),
                );
                return;
            };
            let work = editor.work();
            work.spawn(async move {
                let mut command = tokio::process::Command::new(config.command);
                command.args(config.args).arg(arguments.args.join(" "));
                if !arguments.cwd.is_empty() {
                    command.current_dir(arguments.cwd);
                }
                if let Some(environment) = arguments.env {
                    for (name, value) in environment {
                        match value {
                            Some(value) => {
                                command.env(name, value);
                            }
                            None => {
                                command.env_remove(name);
                            }
                        }
                    }
                }
                let result = command
                    .spawn()
                    .map(|child| {
                        json!(requests::RunInTerminalResponse {
                            process_id: child.id(),
                            shell_process_id: None,
                        })
                    })
                    .map_err(|error| format!("Error starting external terminal: {error}"));
                send_task_event_with(
                    RuntimeTaskEvent::DapAdapterReplyReady { parent, result },
                    ingress,
                )
                .await;
            })
            .detach();
        }
        Request::StartDebugging(_) => {
            unreachable!("startDebugging requests are intercepted before adapter dispatch")
        }
    }
}

pub(crate) fn start_client(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    socket: Option<std::net::SocketAddr>,
    config: DebugAdapterConfig,
    session: DapSessionRequest,
) -> DebugAdapterId {
    let startup = editor.debug_adapters.start_client(socket, config);
    let id = startup.id();
    let work = editor.work();
    editor.set_status("Starting debugger...");
    work.spawn(async move {
        let result = startup.run().await;
        send_task_event_with(
            RuntimeTaskEvent::DapClientStartupCompleted {
                id,
                result: Box::new(result),
                session,
            },
            ingress,
        )
        .await;
    })
    .detach();
    id
}

fn start_child_debugger(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    parent: DapParentRequest,
    arguments: requests::StartDebuggingArguments,
) {
    let Some(debugger) = editor.debug_adapters.get_client(parent.client_id) else {
        editor.set_error("No active debugger found.");
        return;
    };
    let Some(socket) = debugger.socket else {
        let message =
            "Child debugger can only be started if the parent debugger is using TCP transport.";
        editor.set_error(message);
        reply_to_parent(editor, ingress, parent, Err(message.to_owned()));
        return;
    };
    let Some(config) = debugger.config.clone() else {
        let message = "No configuration found for the debugger.";
        log::error!("{message}");
        reply_to_parent(editor, ingress, parent, Err(message.to_owned()));
        return;
    };

    start_client(
        editor,
        ingress,
        Some(socket),
        config,
        DapSessionRequest {
            connection_type: arguments.request,
            arguments: arguments.configuration,
            parent: Some(parent),
        },
    );
}

pub(crate) fn apply_client_startup_completed(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    id: DebugAdapterId,
    result: helix_dap::Result<helix_dap::registry::StartedClient>,
    session: DapSessionRequest,
) {
    if !editor.debug_adapters.is_client_initializing(id) {
        log::debug!("discarding stale debugger startup completion for {id}");
        return;
    }
    if session
        .parent
        .as_ref()
        .is_some_and(|parent| editor.debug_adapters.get_client(parent.client_id).is_none())
    {
        editor.debug_adapters.cancel_client_start(id);
        log::debug!("discarding child debugger startup after its parent was removed");
        return;
    }

    let started = match result {
        Ok(started) => started,
        Err(error) => {
            editor.debug_adapters.cancel_client_start(id);
            let message = if session.parent.is_some() {
                format!("Failed to create child debugger: {error}")
            } else {
                format!("Failed to start debug client: {error}")
            };
            editor.set_error(message.clone());
            if let Some(parent) = session.parent {
                reply_to_parent(editor, ingress, parent, Err(message));
            }
            return;
        }
    };

    if started.id() != id {
        editor.debug_adapters.cancel_client_start(id);
        log::warn!(
            "discarding debugger startup completion for mismatched reservation: expected {id}, got {}",
            started.id()
        );
        return;
    }
    if editor.debug_adapters.finish_client_start(started).is_none() {
        log::debug!("discarding stale debugger startup completion for {id}");
        return;
    }

    let work = editor.work();
    let cancel = editor
        .debug_adapters
        .session_start_cancellation(id)
        .expect("completed debugger initialization must retain startup cancellation");
    let debugger = editor
        .debug_adapters
        .get_client_mut(id)
        .expect("completed debugger startup must install its client");
    let request = match session.connection_type {
        ConnectionType::Launch => {
            futures_util::future::Either::Left(debugger.launch(session.arguments))
        }
        ConnectionType::Attach => {
            futures_util::future::Either::Right(debugger.attach(session.arguments))
        }
    };
    work.spawn(async move {
        let result = tokio::select! {
            biased;
            _ = cancel.canceled() => return,
            result = request => result.map(|_| ()).map_err(|error| error.to_string()),
        };
        send_task_event_with(
            RuntimeTaskEvent::DapSessionStartupCompleted {
                client_id: id,
                parent: session.parent,
                result,
            },
            ingress,
        )
        .await;
    })
    .detach();
}

pub(crate) fn apply_session_startup_completed(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    client_id: DebugAdapterId,
    parent: Option<DapParentRequest>,
    result: Result<(), String>,
) {
    if !editor.debug_adapters.is_session_starting(client_id) {
        log::debug!("discarding stale debugger session completion for {client_id}");
        return;
    }

    if let Err(message) = result {
        if !editor.debug_adapters.fail_session_start(client_id) {
            log::debug!("discarding stale debugger session failure for {client_id}");
            return;
        }
        editor.set_error(format!("Failed to start debugging session: {message}"));
        if let Some(parent) = parent {
            reply_to_parent(editor, ingress, parent, Err(message));
        }
        return;
    }

    if !editor.debug_adapters.finish_session_start(client_id) {
        log::debug!("discarding stale debugger session completion for {client_id}");
        return;
    }
    if let Some(parent) = parent {
        reply_to_parent(editor, ingress, parent, Ok(()));
    }
}

pub(crate) fn apply_stopped_completed(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
    client_id: DebugAdapterId,
    generation: u64,
    preferred_thread_id: Option<DebugThreadId>,
    stacks: Vec<(DebugThreadId, Vec<StackFrame>)>,
    errors: Vec<String>,
) {
    if !ingress.is_current_dap_operation(client_id, DapOperation::StackTrace, generation) {
        log::debug!("discarding superseded DAP stack completion for {client_id}");
        return;
    }
    let should_navigate = editor
        .debug_adapters
        .get_active_client()
        .is_some_and(|debugger| debugger.id() == client_id);
    let Some(debugger) = editor.debug_adapters.get_client_mut(client_id) else {
        log::debug!("discarding DAP stack completion for removed client {client_id}");
        return;
    };

    for error in errors {
        log::warn!("{error}");
    }
    let fallback_thread_id = stacks.first().map(|(thread_id, _)| *thread_id);
    let preferred_exists = preferred_thread_id
        .is_some_and(|preferred| stacks.iter().any(|(thread_id, _)| *thread_id == preferred));
    for (thread_id, frames) in stacks {
        debugger.stack_frames.insert(thread_id, frames);
    }

    let frame = if debugger.thread_id.is_none() {
        let thread_id = if preferred_exists {
            preferred_thread_id
        } else {
            fallback_thread_id
        };
        debugger.thread_id = thread_id;
        debugger.active_frame = thread_id.map(|_| 0);
        if should_navigate {
            thread_id.and_then(|thread_id| {
                debugger
                    .stack_frames
                    .get(&thread_id)
                    .and_then(|frames| frames.first())
                    .cloned()
            })
        } else {
            None
        }
    } else {
        if debugger
            .thread_id
            .is_some_and(|thread_id| debugger.stack_frames.contains_key(&thread_id))
        {
            debugger.active_frame = Some(0);
        }
        None
    };

    if let Some(frame) = frame {
        queue_stack_frame_open(editor, ingress, foreground, frame);
    }
}

pub(crate) fn apply_initialized_completed(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    client_id: DebugAdapterId,
    generation: u64,
    breakpoints: Vec<DapConfiguredBreakpoints>,
    configuration_result: Result<(), String>,
) {
    if !ingress.is_current_dap_operation(client_id, DapOperation::Configuration, generation) {
        log::debug!("discarding superseded DAP configuration completion for {client_id}");
        return;
    }
    if editor.debug_adapters.get_client(client_id).is_none() {
        log::debug!("discarding DAP configuration completion for removed client {client_id}");
        return;
    }

    for configured in breakpoints {
        match configured.result {
            Ok(response) => apply_breakpoints_response(
                editor,
                client_id,
                configured.path,
                configured.expected,
                response,
            ),
            Err(error) => log::warn!("failed to configure debugger breakpoints: {error}"),
        }
    }
    editor.debug_adapters.set_active_client(client_id);
    match configuration_result {
        Ok(()) => editor.set_status("Debugged application started"),
        Err(error) => editor.set_error(format!("Failed to finish debugger configuration: {error}")),
    }
}

pub(crate) fn apply_disconnect_completed(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    client_id: DebugAdapterId,
    generation: u64,
    restart: Option<serde_json::Value>,
    connection_type: Option<ConnectionType>,
    result: Result<(), String>,
) {
    if !ingress.is_current_dap_operation(client_id, DapOperation::Termination, generation) {
        log::debug!("discarding superseded DAP disconnect completion for {client_id}");
        return;
    }
    if editor.debug_adapters.get_client(client_id).is_none() {
        log::debug!("discarding DAP disconnect completion for removed client {client_id}");
        return;
    }
    if let Err(error) = result {
        editor.set_error(format!(
            "Cannot disconnect debugger upon terminated event: {error}"
        ));
        return;
    }

    if restart
        .as_ref()
        .is_none_or(|value| value == &serde_json::Value::Bool(false))
    {
        editor.debug_adapters.remove_client(client_id);
        ingress.clear_dap_client(client_id);
        for breakpoints in editor.breakpoints.values_mut() {
            for breakpoint in breakpoints {
                breakpoint.verified = false;
            }
        }
        clear_inline_values(editor);
        editor.set_status("Terminated debugging session and disconnected debugger.");
        return;
    }

    let Some(connection_type) = connection_type else {
        editor.set_error(
            "No starting request found, to be used in restarting the debugging session.",
        );
        return;
    };
    let arguments = restart.expect("restart was checked above");
    let debugger = editor
        .debug_adapters
        .get_client_mut(client_id)
        .expect("client identity was checked above");
    let request = match connection_type {
        ConnectionType::Launch => futures_util::future::Either::Left(debugger.launch(arguments)),
        ConnectionType::Attach => futures_util::future::Either::Right(debugger.attach(arguments)),
    };
    let work = editor.work();
    work.spawn(async move {
        let result = request.await.map(|_| ()).map_err(|error| error.to_string());
        send_task_event_with(
            RuntimeTaskEvent::DapRelaunchCompleted {
                client_id,
                generation,
                result,
            },
            ingress,
        )
        .await;
    })
    .detach();
}

pub(crate) fn apply_relaunch_completed(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    client_id: DebugAdapterId,
    generation: u64,
    result: Result<(), String>,
) {
    if !ingress.is_current_dap_operation(client_id, DapOperation::Termination, generation)
        || editor.debug_adapters.get_client(client_id).is_none()
    {
        log::debug!("discarding stale DAP relaunch completion for {client_id}");
        return;
    }
    match result {
        Ok(()) => editor.set_status("Debugging session restarted"),
        Err(error) => editor.set_error(format!("Failed to restart debugging session: {error}")),
    }
}

pub(crate) fn apply_adapter_reply(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    parent: DapParentRequest,
    result: Result<serde_json::Value, String>,
) {
    if let Err(error) = &result {
        editor.set_error(error.clone());
    }
    reply_to_adapter(editor, ingress, parent, result);
}

pub(crate) fn apply_request_failed(
    editor: &mut Editor,
    client_id: DebugAdapterId,
    message: String,
) {
    if editor.debug_adapters.get_client(client_id).is_some() {
        editor.set_error(message);
    } else {
        log::debug!("discarding DAP request failure for removed client {client_id}");
    }
}

fn clear_inline_values(editor: &mut Editor) {
    for document in editor.documents_mut() {
        document.clear_inline_values();
    }
}

fn reply_to_adapter(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    parent: DapParentRequest,
    result: Result<serde_json::Value, String>,
) {
    let Some(debugger) = editor.debug_adapters.get_client(parent.client_id) else {
        log::debug!(
            "discarding DAP reply '{}' after client {} was removed",
            parent.command,
            parent.client_id
        );
        return;
    };
    let result = result.map_err(|message| helix_dap::Error::Other(anyhow::anyhow!(message)));
    let reply = debugger.reply(parent.sequence, &parent.command, result);
    editor
        .work()
        .spawn(async move {
            if let Err(error) = reply.await {
                send_task_event_with(
                    RuntimeTaskEvent::DapRequestFailed {
                        client_id: parent.client_id,
                        message: format!("Failed to reply to debugger: {error}"),
                    },
                    ingress,
                )
                .await;
            }
        })
        .detach();
}

fn reply_to_parent(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    parent: DapParentRequest,
    result: Result<(), String>,
) {
    let work = editor.work();
    let Some(debugger) = editor.debug_adapters.get_client(parent.client_id) else {
        log::debug!(
            "discarding debugger child-start reply after parent {} was removed",
            parent.client_id
        );
        return;
    };
    let result = result
        .map(|()| json!({ "success": true }))
        .map_err(|message| helix_dap::Error::Other(anyhow::anyhow!(message)));
    let reply = debugger.reply(parent.sequence, &parent.command, result);
    work.spawn(async move {
        if let Err(error) = reply.await {
            send_task_event_with(
                RuntimeTaskEvent::DapRequestFailed {
                    client_id: parent.client_id,
                    message: format!("Failed to reply to parent debugger: {error}"),
                },
                ingress,
            )
            .await;
        }
    })
    .detach();
}

pub(crate) fn apply_dap_restarted(editor: &mut Editor) {
    editor.set_status("Debugging session restarted");
}

pub(crate) fn apply_resume_debugger_application(editor: &mut Editor) {
    debugger!(editor).resume_application();
}

pub(crate) fn apply_unset_active_debug_client(editor: &mut Editor) {
    editor.debug_adapters.unset_active_client();
}

pub(crate) fn request_select_debug_thread(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    thread_id: DebugThreadId,
    policy: helix_view::editor::ThreadSelectPolicy,
) {
    let work = editor.work();
    let Some(debugger) = editor.debug_adapters.get_active_client_mut() else {
        editor.set_error("Debugger is not running");
        return;
    };

    if !policy.should_replace_current() && debugger.thread_id.is_some() {
        return;
    }

    let client_id = debugger.id();
    let generation = ingress.begin_dap_operation(client_id, DapOperation::StackTrace);
    debugger.thread_id = Some(thread_id);
    let request = debugger.stack_trace_request(thread_id);
    let ingress_for_error = ingress.clone();

    work.spawn(async move {
        match request.await {
            Ok((frames, _)) => {
                send_task_event_with(
                    RuntimeTaskEvent::ApplyStackFrames {
                        client_id,
                        generation,
                        thread_id,
                        frames,
                        selection: helix_view::editor::FrameSelection::SelectFirst,
                    },
                    ingress,
                )
                .await;
            }
            Err(err) => {
                send_task_event_with(
                    RuntimeTaskEvent::DapRequestFailed {
                        client_id,
                        message: format!("Failed to fetch stack trace: {}", err),
                    },
                    ingress_for_error,
                )
                .await;
            }
        }
    })
    .detach();
}

pub(crate) fn request_pause_debug_thread(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    thread_id: DebugThreadId,
) {
    let work = editor.work();
    let debugger = debugger!(editor);
    let client_id = debugger.id();
    let request = debugger.pause(thread_id);
    work.spawn(async move {
        if let Err(err) = request.await {
            send_task_event_with(
                RuntimeTaskEvent::DapRequestFailed {
                    client_id,
                    message: format!("Failed to pause: {}", err),
                },
                ingress,
            )
            .await;
        }
    })
    .detach();
}

pub(crate) fn apply_select_stack_frame(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
    thread_id: DebugThreadId,
    frame_id: usize,
) {
    let debugger = debugger!(editor);
    let Some(frames) = debugger.stack_frames.get(&thread_id) else {
        editor.set_error(format!(
            "Stack frames for thread {thread_id} are unavailable"
        ));
        return;
    };
    let Some(pos) = frames.iter().position(|frame| frame.id == frame_id) else {
        editor.set_error(format!(
            "Stack frame {frame_id} for thread {thread_id} is unavailable"
        ));
        return;
    };
    let frame = frames[pos].clone();
    debugger.active_frame = Some(pos);

    queue_stack_frame_open(editor, ingress, foreground, frame);
}

pub(crate) fn apply_stack_frames(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
    client_id: DebugAdapterId,
    generation: u64,
    thread_id: DebugThreadId,
    frames: Vec<StackFrame>,
    selection: helix_view::editor::FrameSelection,
) {
    if !ingress.is_current_dap_operation(client_id, DapOperation::StackTrace, generation) {
        log::debug!("discarding superseded stack-frame selection for {client_id}");
        return;
    }
    let should_navigate = editor
        .debug_adapters
        .get_active_client()
        .is_some_and(|debugger| debugger.id() == client_id);
    let Some(debugger) = editor.debug_adapters.get_client_mut(client_id) else {
        log::debug!("discarding stack-frame selection for removed client {client_id}");
        return;
    };
    debugger.stack_frames.insert(thread_id, frames);
    debugger.thread_id = Some(thread_id);
    debugger.active_frame = selection.should_select_first().then_some(0);

    if selection.should_select_first() && should_navigate {
        let frame = debugger
            .stack_frames
            .get(&thread_id)
            .and_then(|frames| frames.first())
            .cloned();
        if let Some(frame) = frame {
            queue_stack_frame_open(editor, ingress, foreground, frame);
        }
    }
}

fn queue_stack_frame_open(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
    frame: StackFrame,
) {
    let Some(path) = frame.source.and_then(|source| source.path) else {
        return;
    };
    let target = editor.focused_view_id();
    crate::runtime::ui::document::queue_document_open(
        editor,
        &ingress,
        foreground,
        crate::runtime::DocumentOpenRequest {
            path,
            action: Action::Replace,
            lane: crate::runtime::DocumentOpenLane::Debug,
            target: crate::runtime::DocumentOpenTarget::View(target),
            selection: crate::runtime::DocumentOpenSelection::OneBasedRange {
                line: frame.line,
                column: frame.column,
                end_line: frame.end_line,
                end_column: frame.end_column,
            },
            alignment: crate::runtime::DocumentOpenAlignment::Center,
            default_folding_if_new: false,
            fff_record: None,
            external_if_binary: None,
            post_action: crate::runtime::DocumentOpenPostAction::RequestInlineValues,
            completion: crate::runtime::DocumentOpenCompletionTarget::Editor,
        },
    );
}

pub(crate) fn apply_breakpoint_condition(
    editor: &mut Editor,
    path: PathBuf,
    index: usize,
    condition: Option<String>,
    ingress: crate::runtime::RuntimeIngress,
) {
    let Some(breakpoints) = editor.breakpoints.get_mut(&path) else {
        editor.set_error("Breakpoint file disappeared");
        return;
    };
    if index >= breakpoints.len() {
        editor.set_error("Breakpoint disappeared");
        return;
    }

    breakpoints[index].condition = condition;
    queue_breakpoints_changed(editor, path, ingress);
}

pub(crate) fn apply_breakpoint_log_message(
    editor: &mut Editor,
    path: PathBuf,
    index: usize,
    log_message: Option<String>,
    ingress: crate::runtime::RuntimeIngress,
) {
    let Some(breakpoints) = editor.breakpoints.get_mut(&path) else {
        editor.set_error("Breakpoint file disappeared");
        return;
    };
    if index >= breakpoints.len() {
        editor.set_error("Breakpoint disappeared");
        return;
    }

    breakpoints[index].log_message = log_message;
    queue_breakpoints_changed(editor, path, ingress);
}

pub(crate) fn apply_toggle_breakpoint(
    editor: &mut Editor,
    path: PathBuf,
    line: usize,
    ingress: crate::runtime::RuntimeIngress,
) {
    let breakpoints = editor.breakpoints.entry(path.clone()).or_default();

    if let Some(pos) = breakpoints
        .iter()
        .position(|breakpoint| breakpoint.line == line)
    {
        breakpoints.remove(pos);
    } else {
        breakpoints.push(helix_view::editor::Breakpoint {
            line,
            ..Default::default()
        });
    }

    queue_breakpoints_changed(editor, path, ingress);
}

fn queue_breakpoints_changed(
    editor: &mut Editor,
    path: PathBuf,
    ingress: crate::runtime::RuntimeIngress,
) {
    let Some(debugger) = editor.debug_adapters.get_active_client() else {
        return;
    };
    let client_id = debugger.id();
    let expected = editor.breakpoints.get(&path).cloned().unwrap_or_default();
    let source = helix_view::handlers::dap::source_breakpoints(&expected);
    let request = debugger.request_handle();
    editor.set_status("Updating breakpoints...");
    editor
        .work()
        .spawn(async move {
            let event = match request.set_breakpoints(path.clone(), source).await {
                Ok(response) => RuntimeTaskEvent::ApplyBreakpointsResponse {
                    client_id,
                    path,
                    expected,
                    response,
                },
                Err(error) => RuntimeTaskEvent::DapRequestFailed {
                    client_id,
                    message: format!("Failed to set breakpoints: {error}"),
                },
            };
            send_task_event_with(event, ingress).await;
        })
        .detach();
}

pub(crate) fn apply_breakpoints_response(
    editor: &mut Editor,
    client_id: DebugAdapterId,
    path: PathBuf,
    expected: Vec<helix_view::editor::Breakpoint>,
    response: Option<Vec<helix_dap::Breakpoint>>,
) {
    if editor.debug_adapters.get_client(client_id).is_none() {
        log::debug!("discarding breakpoint response for removed client {client_id}");
        return;
    }
    let Some(current) = editor.breakpoints.get_mut(&path) else {
        return;
    };
    if *current != expected {
        log::debug!(
            "discarding stale breakpoint response for {}",
            path.display()
        );
        return;
    }
    helix_view::handlers::dap::apply_breakpoints_response(current, response);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_status_preserves_adapter_context() {
        let body = events::StoppedBody {
            reason: "breakpoint".into(),
            description: Some("paused".into()),
            thread_id: Some(helix_dap::ThreadId::new(7)),
            preserve_focus_hint: None,
            text: Some("at main".into()),
            all_threads_stopped: Some(true),
            hit_breakpoint_ids: None,
        };

        assert_eq!(
            stopped_status(&body, true),
            "Thread 7 stopped because of breakpoint paused at main (all threads stopped)"
        );
    }
}
