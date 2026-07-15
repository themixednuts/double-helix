use crate::runtime::PluginNotification;
use helix_plugin::rpc::{HostResponse, LogLevel, PluginRequest};
use helix_plugin_api::events;
use helix_plugin_api::host::{
    PluginAssistantMutationHost, PluginAssistantQueryHost, PluginCommandHost, PluginEventHost,
    PluginFacadeMutationHost, PluginFacadeQueryHost, PluginFloatHost, PluginKeymapHost,
    PluginMutationHost, PluginPanelHost, PluginQueryHost, PluginSplitHost, PluginTabHost,
    PluginUiHost, PluginWorkspaceQueryHost,
};
use helix_plugin_api::{ContractError, ContractResult};
use helix_plugin_editor::adapt;
use helix_plugin_editor::bridge::{EditorMutationBridge, EditorQueryBridge};
use helix_view::Editor;

fn with_query<T>(
    editor: &Editor,
    f: impl FnOnce(&EditorQueryBridge<'_>) -> ContractResult<T>,
) -> ContractResult<T> {
    f(&EditorQueryBridge::new(editor))
}

fn with_mutation<T>(
    editor: &mut Editor,
    f: impl FnOnce(&mut EditorMutationBridge<'_>) -> ContractResult<T>,
) -> ContractResult<T> {
    let mut bridge = EditorMutationBridge::new(editor);
    f(&mut bridge)
}

fn unit(result: ContractResult<()>) -> ContractResult<HostResponse> {
    result.map(|()| HostResponse::Unit)
}

fn service_plugin_host_request(
    editor: &mut Editor,
    state: crate::plugin_registry::PluginHostState,
    request: PluginRequest,
) -> ContractResult<HostResponse> {
    match request {
        PluginRequest::ApiMetadata => Ok(HostResponse::ApiMetadata(Default::default())),
        PluginRequest::FocusedDocument => Ok(HostResponse::OptionDocumentHandle(with_query(
            editor,
            |host| Ok(host.focused_document()),
        )?)),
        PluginRequest::FocusedView => Ok(HostResponse::OptionViewHandle(with_query(
            editor,
            |host| Ok(host.focused_view()),
        )?)),
        PluginRequest::ListDocuments => {
            Ok(HostResponse::DocumentHandles(with_query(editor, |host| {
                Ok(host.list_documents())
            })?))
        }
        PluginRequest::ListViews => Ok(HostResponse::ViewHandles(with_query(editor, |host| {
            Ok(host.list_views())
        })?)),
        PluginRequest::LanguageServers => {
            Ok(HostResponse::LanguageServers(with_query(editor, |host| {
                host.language_servers()
            })?))
        }
        PluginRequest::EditorConfig => {
            Ok(HostResponse::EditorConfig(with_query(editor, |host| {
                PluginFacadeQueryHost::editor_config(host)
            })?))
        }
        PluginRequest::TerminalSize => {
            Ok(HostResponse::TerminalSize(with_query(editor, |host| {
                PluginFacadeQueryHost::terminal_size(host)
            })?))
        }
        PluginRequest::ReadRegister(name) => {
            Ok(HostResponse::Strings(with_query(editor, |host| {
                PluginFacadeQueryHost::read_register(host, name)
            })?))
        }
        PluginRequest::WriteRegister { name, values } => unit(with_mutation(editor, |host| {
            PluginFacadeMutationHost::write_register(host, name, values)
        })),
        PluginRequest::RequestRedraw => unit(with_mutation(editor, |host| {
            PluginFacadeMutationHost::request_redraw(host);
            Ok(())
        })),
        PluginRequest::DocumentSnapshot(handle) => Ok(HostResponse::DocumentSnapshot(with_query(
            editor,
            |host| host.document_snapshot(handle),
        )?)),
        PluginRequest::ViewSnapshot(handle) => {
            Ok(HostResponse::ViewSnapshot(with_query(editor, |host| {
                host.view_snapshot(handle)
            })?))
        }
        PluginRequest::WorkspaceSnapshot => Ok(HostResponse::WorkspaceSnapshot(with_query(
            editor,
            |host| Ok(host.workspace_snapshot()),
        )?)),
        PluginRequest::ThemeSnapshot => {
            Ok(HostResponse::ThemeSnapshot(with_query(editor, |host| {
                Ok(host.theme_snapshot())
            })?))
        }
        PluginRequest::Diagnostics(handle) => Ok(HostResponse::DiagnosticSnapshot(with_query(
            editor,
            |host| host.diagnostics(handle),
        )?)),
        PluginRequest::DocumentText(handle) => {
            Ok(HostResponse::DocumentText(with_query(editor, |host| {
                host.document_text(handle)
            })?))
        }
        PluginRequest::DocumentLine { document, line } => {
            Ok(HostResponse::DocumentLine(with_query(editor, |host| {
                host.document_line(document, line)
            })?))
        }
        PluginRequest::StartTask { .. } | PluginRequest::CancelTask { .. } => {
            unreachable!("tasks are routed through the asynchronous task dispatcher")
        }
        PluginRequest::ApplyEdit(req) => unit(with_mutation(editor, |host| host.apply_edit(req))),
        PluginRequest::SetSelection(req) => {
            unit(with_mutation(editor, |host| host.set_selection(req)))
        }
        PluginRequest::SaveDocument(req) => {
            unit(with_mutation(editor, |host| host.save_document(req)))
        }
        PluginRequest::FocusView(req) => unit(with_mutation(editor, |host| host.focus_view(req))),
        PluginRequest::SetAnnotations(req) => {
            unit(with_mutation(editor, |host| host.set_annotations(req)))
        }
        PluginRequest::SetStatus(req) => unit(with_mutation(editor, |host| host.set_status(req))),
        PluginRequest::Undo(req) => Ok(HostResponse::Bool(with_mutation(editor, |host| {
            host.undo(req)
        })?)),
        PluginRequest::Redo(req) => Ok(HostResponse::Bool(with_mutation(editor, |host| {
            host.redo(req)
        })?)),
        PluginRequest::SelectAll(req) => unit(with_mutation(editor, |host| host.select_all(req))),
        PluginRequest::SetMode(req) => unit(with_mutation(editor, |host| host.set_mode(req))),
        PluginRequest::CloseView(req) => unit(with_mutation(editor, |host| host.close_view(req))),
        PluginRequest::Notify(req) => {
            let mut state = state.try_lock()?;
            unit(state.ui.notify(req))
        }
        PluginRequest::Prompt { plugin, request } => {
            let mut state = state.try_lock()?;
            Ok(HostResponse::UiCallback(state.ui.prompt(plugin, request)?))
        }
        PluginRequest::Confirm { plugin, request } => {
            let mut state = state.try_lock()?;
            Ok(HostResponse::UiCallback(state.ui.confirm(plugin, request)?))
        }
        PluginRequest::Picker { plugin, request } => {
            let mut state = state.try_lock()?;
            Ok(HostResponse::UiCallback(state.ui.picker(plugin, request)?))
        }
        PluginRequest::RegisterPanel {
            plugin,
            registration,
        } => {
            state.track_plugin(plugin)?;
            let mut state = state.try_lock()?;
            Ok(HostResponse::PanelHandle(
                state
                    .panel
                    .service(editor)
                    .register_panel(plugin, registration)?,
            ))
        }
        PluginRequest::UpdatePanel { plugin, request } => {
            let mut state = state.try_lock()?;
            unit(state.panel.service(editor).update_panel(plugin, request))
        }
        PluginRequest::ClosePanel { plugin, request } => {
            let mut state = state.try_lock()?;
            unit(state.panel.service(editor).close_panel(plugin, request))
        }
        PluginRequest::TogglePanel { plugin, request } => {
            let mut state = state.try_lock()?;
            unit(state.panel.service(editor).toggle_panel(plugin, request))
        }
        PluginRequest::FocusPanel { plugin, request } => {
            let mut state = state.try_lock()?;
            unit(state.panel.service(editor).focus_panel(plugin, request))
        }
        PluginRequest::ResizePanel { plugin, request } => {
            let mut state = state.try_lock()?;
            unit(state.panel.service(editor).resize_panel(plugin, request))
        }
        PluginRequest::ListPanels => {
            let mut state = state.try_lock()?;
            Ok(HostResponse::PanelSnapshots(
                state.panel.service(editor).list_panels(),
            ))
        }
        PluginRequest::CommandCatalog => {
            let state = state.try_lock()?;
            Ok(HostResponse::CommandCatalog(
                state.command.command_catalog(),
            ))
        }
        PluginRequest::RegisterCommand { plugin, definition } => {
            state.track_plugin(plugin)?;
            let mut state = state.try_lock()?;
            Ok(HostResponse::CommandHandle(
                state.command.register_command(plugin, definition)?,
            ))
        }
        PluginRequest::UpdateCommand { plugin, request } => {
            let mut state = state.try_lock()?;
            unit(state.command.update_command(plugin, request))
        }
        PluginRequest::RemoveCommand { plugin, request } => {
            let mut state = state.try_lock()?;
            unit(state.command.remove_command(plugin, request))
        }
        PluginRequest::ReleaseResources { plugin } => unit(state.release_plugin_resources(plugin)),
        PluginRequest::RegisterKeymap { plugin, definition } => {
            state.track_plugin(plugin)?;
            let mut state = state.try_lock()?;
            Ok(HostResponse::KeymapHandle(
                state.keymap.register_keymap(plugin, definition)?,
            ))
        }
        PluginRequest::UpdateKeymap { plugin, request } => {
            let mut state = state.try_lock()?;
            unit(state.keymap.update_keymap(plugin, request))
        }
        PluginRequest::RemoveKeymap { plugin, request } => {
            let mut state = state.try_lock()?;
            unit(state.keymap.remove_keymap(plugin, request))
        }
        PluginRequest::Subscribe { plugin, kind } => {
            state.track_plugin(plugin)?;
            let mut state = state.try_lock()?;
            Ok(HostResponse::SubscriptionHandle(
                state.event.subscribe(plugin, kind)?,
            ))
        }
        PluginRequest::Unsubscribe { plugin, handle } => {
            let mut state = state.try_lock()?;
            unit(state.event.unsubscribe(plugin, handle))
        }
        PluginRequest::EventCatalog => {
            let state = state.try_lock()?;
            Ok(HostResponse::EventCatalog(state.event.event_catalog()))
        }
        PluginRequest::SplitView(req) => {
            Ok(HostResponse::ViewHandle(with_mutation(editor, |host| {
                host.split_view(req)
            })?))
        }
        PluginRequest::FocusDirection(req) => Ok(HostResponse::OptionViewHandleResult(
            with_mutation(editor, |host| host.focus_direction(req))?,
        )),
        PluginRequest::SwapSplit(req) => unit(with_mutation(editor, |host| host.swap_split(req))),
        PluginRequest::ResizeSplit(req) => {
            unit(with_mutation(editor, |host| host.resize_split(req)))
        }
        PluginRequest::Transpose(req) => unit(with_mutation(editor, |host| host.transpose(req))),
        PluginRequest::SplitTree => Ok(HostResponse::SplitTree(with_mutation(editor, |host| {
            Ok(PluginSplitHost::split_tree(host))
        })?)),
        PluginRequest::OpenTab(req) => unit(with_mutation(editor, |host| host.open_tab(req))),
        PluginRequest::CloseTab(req) => unit(with_mutation(editor, |host| host.close_tab(req))),
        PluginRequest::FocusTab(req) => unit(with_mutation(editor, |host| host.focus_tab(req))),
        PluginRequest::CycleTab(req) => unit(with_mutation(editor, |host| host.cycle_tab(req))),
        PluginRequest::ListTabs(view) => {
            Ok(HostResponse::TabGroup(with_mutation(editor, |host| {
                host.list_tabs(view)
            })?))
        }
        PluginRequest::CreateFloat { plugin, request } => {
            state.track_plugin(plugin)?;
            let float = with_mutation(editor, |host| host.create_float(plugin, request))?;
            Ok(HostResponse::FloatHandle(float))
        }
        PluginRequest::UpdateFloat { plugin, request } => unit(with_mutation(editor, |host| {
            host.update_float(plugin, request)
        })),
        PluginRequest::CloseFloat { plugin, request } => unit(with_mutation(editor, |host| {
            host.close_float(plugin, request)
        })),
        PluginRequest::ListFloats(plugin) => Ok(HostResponse::FloatSnapshots(with_mutation(
            editor,
            |host| Ok(host.list_floats(plugin)),
        )?)),
        PluginRequest::AssistantSnapshot => Ok(HostResponse::AssistantSnapshot(with_query(
            editor,
            |host| Ok(host.assistant_snapshot()),
        )?)),
        PluginRequest::ThreadSnapshot(thread) => Ok(HostResponse::AssistantThreadSnapshot(
            with_query(editor, |host| host.thread_snapshot(thread))?,
        )),
        PluginRequest::ThreadEntries(thread) => Ok(HostResponse::AssistantEntries(with_query(
            editor,
            |host| host.thread_entries(thread),
        )?)),
        PluginRequest::ThreadContext(thread) => Ok(HostResponse::AssistantContext(with_query(
            editor,
            |host| host.thread_context(thread),
        )?)),
        PluginRequest::SubmitPrompt { thread, text } => unit(with_mutation(editor, |host| {
            host.submit_prompt(thread, text)
        })),
        PluginRequest::CancelThread(thread) => {
            unit(with_mutation(editor, |host| host.cancel_thread(thread)))
        }
        PluginRequest::WorkspaceDetail => {
            Ok(HostResponse::WorkspaceDetail(with_query(editor, |host| {
                Ok(host.workspace_detail())
            })?))
        }
        PluginRequest::Log { level, plugin, msg } => {
            match level {
                LogLevel::Error => log::error!(target: "helix_plugin_host", "{plugin}: {msg}"),
                LogLevel::Warn => log::warn!(target: "helix_plugin_host", "{plugin}: {msg}"),
                LogLevel::Info => log::info!(target: "helix_plugin_host", "{plugin}: {msg}"),
                LogLevel::Debug => log::debug!(target: "helix_plugin_host", "{plugin}: {msg}"),
                LogLevel::Trace => log::trace!(target: "helix_plugin_host", "{plugin}: {msg}"),
            }
            Ok(HostResponse::Unit)
        }
    }
}

pub(crate) fn apply_plugin_host_request(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
    state: crate::plugin_registry::PluginHostState,
    request: PluginRequest,
    respond_to: crate::plugin_registry::PluginHostResponder,
) {
    if let Err(error) = state.ensure_active() {
        respond_to.send(Err(error));
        return;
    }
    if let PluginRequest::StartTask {
        plugin,
        operation,
        request,
    } = request
    {
        match request {
            helix_plugin_api::PluginTaskRequest::OpenDocument(request) => {
                let (_, completion) = match state.begin_task(plugin, operation) {
                    Ok(active) => active,
                    Err(error) => {
                        respond_to.send(Err(error));
                        return;
                    }
                };
                respond_to.send(Ok(HostResponse::Unit));
                crate::runtime::ui::document::queue_document_open(
                    editor,
                    &ingress,
                    &foreground,
                    crate::runtime::DocumentOpenRequest {
                        path: request.path.into(),
                        action: if request.focus {
                            helix_view::editor::Action::Replace
                        } else {
                            helix_view::editor::Action::Load
                        },
                        lane: crate::runtime::DocumentOpenLane::Plugin(operation.raw().get()),
                        target: crate::runtime::DocumentOpenTarget::View(editor.focused_view_id()),
                        selection: crate::runtime::DocumentOpenSelection::None,
                        alignment: crate::runtime::DocumentOpenAlignment::None,
                        default_folding_if_new: false,
                        fff_record: None,
                        external_if_binary: None,
                        post_action: crate::runtime::DocumentOpenPostAction::None,
                        completion: crate::runtime::DocumentOpenCompletionTarget::Plugin(
                            completion,
                        ),
                    },
                );
            }
            helix_plugin_api::PluginTaskRequest::SyntaxQuery(request) => {
                let (runtime, snapshot) =
                    match crate::plugin_registry::prepare_syntax_query(editor, request) {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            respond_to.send(Err(error));
                            return;
                        }
                    };
                let (cancellation, completion) = match state.begin_task(plugin, operation) {
                    Ok(active) => active,
                    Err(error) => {
                        respond_to.send(Err(error));
                        return;
                    }
                };
                respond_to.send(Ok(HostResponse::Unit));
                let blocking = runtime
                    .block()
                    .spawn(move || crate::plugin_registry::execute_syntax_query(snapshot));
                let work = runtime.work().clone();
                work.spawn(async move {
                    let result = blocking
                        .await
                        .map_err(|error| ContractError::internal(error.to_string()))
                        .and_then(|result| result);
                    if !cancellation.is_canceled() {
                        completion.send(result);
                    }
                })
                .detach();
            }
            helix_plugin_api::PluginTaskRequest::LspCall(request) => {
                let (work, server) =
                    match crate::plugin_registry::prepare_lsp_call(editor, &request) {
                        Ok(prepared) => prepared,
                        Err(error) => {
                            respond_to.send(Err(error));
                            return;
                        }
                    };
                let (cancellation, completion) = match state.begin_task(plugin, operation) {
                    Ok(active) => active,
                    Err(error) => {
                        respond_to.send(Err(error));
                        return;
                    }
                };
                respond_to.send(Ok(HostResponse::Unit));
                work.spawn(async move {
                    if let Some(result) =
                        crate::plugin_registry::execute_lsp_call(server, request, cancellation)
                            .await
                    {
                        completion.send(result);
                    }
                })
                .detach();
            }
            helix_plugin_api::PluginTaskRequest::RunCommand(request) => {
                let (_, completion) = match state.begin_task(plugin, operation) {
                    Ok(active) => active,
                    Err(error) => {
                        respond_to.send(Err(error));
                        return;
                    }
                };
                let admitted = foreground.ui(crate::runtime::UiCommand::Plugin(
                    crate::runtime::ui::command::PluginCommand::RunCommand {
                        request,
                        completion: completion.into(),
                    },
                ));
                match admitted {
                    Ok(()) => respond_to.send(Ok(HostResponse::Unit)),
                    Err(error) => {
                        let _ = state.cancel_task(plugin, operation);
                        respond_to.send(Err(ContractError::internal(error.to_string())));
                    }
                }
            }
            helix_plugin_api::PluginTaskRequest::SetTheme(name) => {
                let runtime = editor.runtime().clone();
                let loader = std::sync::Arc::clone(&editor.theme_loader);
                let (cancellation, completion) = match state.begin_task(plugin, operation) {
                    Ok(active) => active,
                    Err(error) => {
                        respond_to.send(Err(error));
                        return;
                    }
                };
                respond_to.send(Ok(HostResponse::Unit));
                crate::plugin_registry::spawn_theme_load(
                    runtime,
                    loader,
                    name,
                    cancellation,
                    ingress.clone(),
                    completion.into(),
                );
            }
        }
        return;
    }
    if let PluginRequest::CancelTask { plugin, operation } = request {
        let result = state.cancel_task(plugin, operation);
        if result.is_ok() {
            ingress.cancel_document_open(crate::runtime::DocumentOpenLane::Plugin(
                operation.raw().get(),
            ));
        }
        respond_to.send(result.map(|()| HostResponse::Unit));
        return;
    }
    let result = service_plugin_host_request(editor, state, request);
    respond_to.send(result);
}

/// Convert a [`PluginNotification`] (lightweight channel signal) to a full
/// [`events::PluginEvent`] (contract event with enriched editor context).
///
/// Returns `None` when the notification requires editor context that isn't
/// available (e.g. the focused view doesn't exist for selection changes).
pub(crate) fn notification_to_event(
    notification: &PluginNotification,
    editor: &Editor,
) -> Option<events::PluginEvent> {
    match notification {
        PluginNotification::BufferOpen {
            document_id, path, ..
        } => {
            let lang = editor
                .documents
                .get(document_id)
                .and_then(|d| d.language_name().map(|s| s.to_string()));
            Some(events::PluginEvent::DocumentOpened(
                events::DocumentOpenedEvent {
                    document: adapt::document_handle(*document_id),
                    path: path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    language: lang,
                },
            ))
        }
        PluginNotification::BufferChanged { document_id } => Some(
            events::PluginEvent::DocumentChanged(events::DocumentChangedEvent {
                document: adapt::document_handle(*document_id),
            }),
        ),
        PluginNotification::BufferClosed { document_id } => Some(
            events::PluginEvent::DocumentClosed(events::DocumentClosedEvent {
                document: adapt::document_handle(*document_id),
            }),
        ),
        PluginNotification::SelectionChange { document_id, .. } => {
            let focused_view_id = editor.tree.focus;
            let view = editor.tree.try_get(focused_view_id)?;
            let doc = editor.documents.get(document_id)?;
            let cursor_char = doc
                .selection(view.id)
                .primary()
                .cursor(doc.text().slice(..));
            Some(events::PluginEvent::SelectionChanged(
                events::SelectionChangedEvent {
                    document: adapt::document_handle(*document_id),
                    view: adapt::view_handle(view.id),
                    primary_cursor: adapt::char_to_position(doc.text(), cursor_char),
                },
            ))
        }
        PluginNotification::ModeChange { old_mode, new_mode } => {
            Some(events::PluginEvent::ModeChanged(events::ModeChangedEvent {
                old: adapt::mode_str_to_contract(old_mode),
                new: adapt::mode_str_to_contract(new_mode),
            }))
        }
        PluginNotification::KeyPress { key } => {
            Some(events::PluginEvent::KeyPressed(events::KeyPressedEvent {
                key: key.clone(),
                mode: adapt::mode_to_contract(editor.mode),
            }))
        }
        PluginNotification::LspDiagnostic {
            document_id,
            diagnostic_count,
        } => Some(events::PluginEvent::DiagnosticsUpdated(
            events::DiagnosticsUpdatedEvent {
                document: adapt::document_handle(*document_id),
                count: *diagnostic_count,
            },
        )),
    }
}
