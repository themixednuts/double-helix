use helix_plugin::contract::bridge::{EditorMutationBridge, EditorQueryBridge};
use helix_plugin::contract::host::{
    PluginAssistantMutationHost, PluginAssistantQueryHost, PluginCommandHost, PluginEventHost,
    PluginFloatHost, PluginMutationHost, PluginPanelHost, PluginQueryHost, PluginSplitHost,
    PluginTabHost, PluginUiHost, PluginWorkspaceQueryHost,
};
use helix_plugin::contract::UiCallbackToken;
use helix_plugin::contract::{adapt, events};
use helix_plugin::contract::{ContractError, ContractResult};
use helix_plugin::rpc::{HostResponse, LogLevel, PluginRequest};
use helix_plugin::{PluginManager, PluginNotification};
use helix_view::Editor;

fn internal_error(message: impl Into<String>) -> ContractError {
    ContractError::internal(message)
}

fn with_query<T>(f: impl FnOnce(&EditorQueryBridge<'_>) -> ContractResult<T>) -> ContractResult<T> {
    helix_plugin::lua::with_current_editor(|editor| f(&EditorQueryBridge::new(editor)))
        .map_err(|err| internal_error(err.to_string()))?
}

fn with_mutation<T>(
    f: impl FnOnce(&mut EditorMutationBridge<'_>) -> ContractResult<T>,
) -> ContractResult<T> {
    helix_plugin::lua::with_current_editor_mut(|editor| {
        let mut bridge = EditorMutationBridge::new(editor);
        f(&mut bridge)
    })
    .map_err(|err| internal_error(err.to_string()))?
}

fn unit(result: ContractResult<()>) -> ContractResult<HostResponse> {
    result.map(|()| HostResponse::Unit)
}

fn service_plugin_host_request(
    state: crate::plugin_registry::RemoteHostState,
    request: PluginRequest,
) -> ContractResult<HostResponse> {
    match request {
        PluginRequest::ApiMetadata => Ok(HostResponse::ApiMetadata(Default::default())),
        PluginRequest::FocusedDocument => {
            Ok(HostResponse::OptionDocumentHandle(with_query(|host| {
                Ok(host.focused_document())
            })?))
        }
        PluginRequest::FocusedView => Ok(HostResponse::OptionViewHandle(with_query(|host| {
            Ok(host.focused_view())
        })?)),
        PluginRequest::ListDocuments => Ok(HostResponse::DocumentHandles(with_query(|host| {
            Ok(host.list_documents())
        })?)),
        PluginRequest::ListViews => Ok(HostResponse::ViewHandles(with_query(|host| {
            Ok(host.list_views())
        })?)),
        PluginRequest::DocumentSnapshot(handle) => {
            Ok(HostResponse::DocumentSnapshot(with_query(|host| {
                host.document_snapshot(handle)
            })?))
        }
        PluginRequest::ViewSnapshot(handle) => {
            Ok(HostResponse::ViewSnapshot(with_query(|host| {
                host.view_snapshot(handle)
            })?))
        }
        PluginRequest::WorkspaceSnapshot => {
            Ok(HostResponse::WorkspaceSnapshot(with_query(|host| {
                Ok(host.workspace_snapshot())
            })?))
        }
        PluginRequest::ThemeSnapshot => Ok(HostResponse::ThemeSnapshot(with_query(|host| {
            Ok(host.theme_snapshot())
        })?)),
        PluginRequest::Diagnostics(handle) => {
            Ok(HostResponse::DiagnosticSnapshot(with_query(|host| {
                host.diagnostics(handle)
            })?))
        }
        PluginRequest::DocumentText(handle) => {
            Ok(HostResponse::DocumentText(with_query(|host| {
                host.document_text(handle)
            })?))
        }
        PluginRequest::DocumentLine { document, line } => {
            Ok(HostResponse::DocumentLine(with_query(|host| {
                host.document_line(document, line)
            })?))
        }
        PluginRequest::OpenDocument(req) => {
            Ok(HostResponse::DocumentHandle(with_mutation(|host| {
                host.open_document(req)
            })?))
        }
        PluginRequest::ApplyEdit(req) => unit(with_mutation(|host| host.apply_edit(req))),
        PluginRequest::SetSelection(req) => unit(with_mutation(|host| host.set_selection(req))),
        PluginRequest::SaveDocument(req) => unit(with_mutation(|host| host.save_document(req))),
        PluginRequest::FocusView(req) => unit(with_mutation(|host| host.focus_view(req))),
        PluginRequest::SetAnnotations(req) => unit(with_mutation(|host| host.set_annotations(req))),
        PluginRequest::SetStatus(req) => unit(with_mutation(|host| host.set_status(req))),
        PluginRequest::Undo(req) => Ok(HostResponse::Bool(with_mutation(|host| host.undo(req))?)),
        PluginRequest::Redo(req) => Ok(HostResponse::Bool(with_mutation(|host| host.redo(req))?)),
        PluginRequest::SetMode(req) => unit(with_mutation(|host| host.set_mode(req))),
        PluginRequest::CloseView(req) => unit(with_mutation(|host| host.close_view(req))),
        PluginRequest::Notify(req) => {
            let mut state = state.lock();
            unit(state.ui.notify(req))
        }
        PluginRequest::Prompt { plugin, request } => {
            let mut state = state.lock();
            Ok(HostResponse::UiCallback(
                state.ui.prompt(plugin, request)?.raw().get(),
            ))
        }
        PluginRequest::Confirm { plugin, request } => {
            let mut state = state.lock();
            Ok(HostResponse::UiCallback(
                state.ui.confirm(plugin, request)?.raw().get(),
            ))
        }
        PluginRequest::Picker { plugin, request } => {
            let mut state = state.lock();
            Ok(HostResponse::UiCallback(
                state.ui.picker(plugin, request)?.raw().get(),
            ))
        }
        PluginRequest::RegisterPanel {
            plugin,
            registration,
        } => {
            let mut state = state.lock();
            Ok(HostResponse::PanelHandle(
                state.panel.register_panel(plugin, registration)?,
            ))
        }
        PluginRequest::UpdatePanel { plugin, request } => {
            let mut state = state.lock();
            unit(state.panel.update_panel(plugin, request))
        }
        PluginRequest::ClosePanel { plugin, request } => {
            let mut state = state.lock();
            unit(state.panel.close_panel(plugin, request))
        }
        PluginRequest::TogglePanel { plugin, request } => {
            let mut state = state.lock();
            unit(state.panel.toggle_panel(plugin, request))
        }
        PluginRequest::FocusPanel { plugin, request } => {
            let mut state = state.lock();
            unit(state.panel.focus_panel(plugin, request))
        }
        PluginRequest::ResizePanel { plugin, request } => {
            let mut state = state.lock();
            unit(state.panel.resize_panel(plugin, request))
        }
        PluginRequest::ListPanels => {
            let state = state.lock();
            Ok(HostResponse::PanelSnapshots(state.panel.list_panels()))
        }
        PluginRequest::RegisterCommand { plugin, definition } => {
            let mut state = state.lock();
            Ok(HostResponse::CommandHandle(
                state.command.register_command(plugin, definition)?,
            ))
        }
        PluginRequest::UpdateCommand { plugin, request } => {
            let mut state = state.lock();
            unit(state.command.update_command(plugin, request))
        }
        PluginRequest::RemoveCommand { plugin, request } => {
            let mut state = state.lock();
            unit(state.command.remove_command(plugin, request))
        }
        PluginRequest::RunCommand(req) => {
            let mut state = state.lock();
            unit(state.command.run_command(req))
        }
        PluginRequest::Subscribe { plugin, kind } => {
            let mut state = state.lock();
            Ok(HostResponse::SubscriptionHandle(
                state.event.subscribe(plugin, kind)?,
            ))
        }
        PluginRequest::Unsubscribe { plugin, handle } => {
            let mut state = state.lock();
            unit(state.event.unsubscribe(plugin, handle))
        }
        PluginRequest::EventCatalog => {
            let state = state.lock();
            Ok(HostResponse::EventCatalog(state.event.event_catalog()))
        }
        PluginRequest::SplitView(req) => Ok(HostResponse::ViewHandle(with_mutation(|host| {
            host.split_view(req)
        })?)),
        PluginRequest::FocusDirection(req) => Ok(HostResponse::OptionViewHandleResult(
            with_mutation(|host| host.focus_direction(req))?,
        )),
        PluginRequest::SwapSplit(req) => unit(with_mutation(|host| host.swap_split(req))),
        PluginRequest::ResizeSplit(req) => unit(with_mutation(|host| host.resize_split(req))),
        PluginRequest::Transpose(req) => unit(with_mutation(|host| host.transpose(req))),
        PluginRequest::SplitTree => Ok(HostResponse::SplitTree(with_mutation(|host| {
            Ok(PluginSplitHost::split_tree(host))
        })?)),
        PluginRequest::OpenTab(req) => unit(with_mutation(|host| host.open_tab(req))),
        PluginRequest::CloseTab(req) => unit(with_mutation(|host| host.close_tab(req))),
        PluginRequest::FocusTab(req) => unit(with_mutation(|host| host.focus_tab(req))),
        PluginRequest::CycleTab(req) => unit(with_mutation(|host| host.cycle_tab(req))),
        PluginRequest::ListTabs(view) => Ok(HostResponse::TabGroup(with_mutation(|host| {
            host.list_tabs(view)
        })?)),
        PluginRequest::CreateFloat { plugin, request } => {
            Ok(HostResponse::FloatHandle(with_mutation(|host| {
                host.create_float(plugin, request)
            })?))
        }
        PluginRequest::UpdateFloat(req) => unit(with_mutation(|host| host.update_float(req))),
        PluginRequest::CloseFloat(req) => unit(with_mutation(|host| host.close_float(req))),
        PluginRequest::ListFloats => Ok(HostResponse::FloatSnapshots(with_mutation(|host| {
            Ok(host.list_floats())
        })?)),
        PluginRequest::AssistantSnapshot => {
            Ok(HostResponse::AssistantSnapshot(with_query(|host| {
                Ok(host.assistant_snapshot())
            })?))
        }
        PluginRequest::ThreadSnapshot(thread) => {
            Ok(HostResponse::AssistantThreadSnapshot(with_query(|host| {
                host.thread_snapshot(thread)
            })?))
        }
        PluginRequest::ThreadEntries(thread) => {
            Ok(HostResponse::AssistantEntries(with_query(|host| {
                host.thread_entries(thread)
            })?))
        }
        PluginRequest::ThreadContext(thread) => {
            Ok(HostResponse::AssistantContext(with_query(|host| {
                host.thread_context(thread)
            })?))
        }
        PluginRequest::SubmitPrompt { thread, text } => {
            unit(with_mutation(|host| host.submit_prompt(thread, text)))
        }
        PluginRequest::CancelThread(thread) => {
            unit(with_mutation(|host| host.cancel_thread(thread)))
        }
        PluginRequest::WorkspaceDetail => Ok(HostResponse::WorkspaceDetail(with_query(|host| {
            Ok(host.workspace_detail())
        })?)),
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
    state: crate::plugin_registry::RemoteHostState,
    request: PluginRequest,
    respond_to: crate::plugin_registry::PluginHostResponder,
) {
    let result = helix_plugin::lua::with_editor_context(editor, || {
        service_plugin_host_request(state, request)
    });
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

pub(crate) fn apply_plugin_ui_callback(
    editor: &mut Editor,
    plugin_manager: std::sync::Arc<PluginManager>,
    callback: UiCallbackToken,
    value: helix_plugin::contract::DynamicValue,
) {
    let Some(callback_id) = helix_plugin::UiCallbackId::new(callback.raw().get()) else {
        editor.set_error("invalid plugin UI callback token");
        return;
    };
    if let Err(err) = plugin_manager.handle_ui_callback(editor, callback_id, value) {
        editor.set_error(err.to_string());
    }
}
