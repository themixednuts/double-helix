use crate::contract::host::*;
use crate::contract::*;
use crate::contract::{events, metadata, requests, snapshots};
use crate::lua::loader::PluginLoader;
use crate::rpc::*;
use crate::{LuaEngine, PluginConfig, UiCallbackId};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

type In = Frame<HostRequest, HostResponse>;
type Out = Frame<PluginRequest, HostResponse>;

fn internal(message: impl Into<String>) -> ContractError {
    ContractError::internal(message)
}

fn nonzero_one() -> NonZeroU64 {
    NonZeroU64::new(1).expect("1 is non-zero")
}

struct Peer {
    codec: FrameCodec,
    stdin: std::io::Stdin,
    stdout: std::io::Stdout,
    next_id: u64,
    deferred_requests: VecDeque<HostRequest>,
}

impl Peer {
    fn new() -> Self {
        Self {
            codec: FrameCodec::new(),
            stdin: std::io::stdin(),
            stdout: std::io::stdout(),
            next_id: 1,
            deferred_requests: VecDeque::new(),
        }
    }

    fn read(&mut self) -> Result<In, FrameError> {
        self.codec.read_sync(&mut self.stdin)
    }

    fn call(&mut self, body: PluginRequest) -> ContractResult<HostResponse> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.codec
            .write_sync(&mut self.stdout, &Out::Request { id, body })
            .map_err(|err| internal(err.to_string()))?;
        loop {
            match self.codec.read_sync::<In, _>(&mut self.stdin) {
                Ok(Frame::Response { id: res_id, result }) if res_id == id => return result,
                Ok(Frame::Notify {
                    body: HostRequest::Shutdown,
                }) => return Err(internal("plugin host shut down while waiting for response")),
                Ok(Frame::Notify { body }) => self.deferred_requests.push_back(body),
                Ok(_) => return Err(internal("unexpected rpc frame while waiting for response")),
                Err(err) => return Err(internal(err.to_string())),
            }
        }
    }

    fn respond(
        &mut self,
        id: u64,
        result: ContractResult<PluginResponse>,
    ) -> Result<(), FrameError> {
        self.codec.write_sync(
            &mut self.stdout,
            &Frame::<PluginRequest, PluginResponse>::Response { id, result },
        )
    }
}

#[derive(Clone)]
struct RpcHost {
    peer: Arc<Mutex<Peer>>,
    next_operation: Arc<AtomicU64>,
}

impl RpcHost {
    fn new(peer: Arc<Mutex<Peer>>) -> Self {
        Self {
            peer,
            next_operation: Arc::new(AtomicU64::new(1)),
        }
    }

    fn call(&self, req: PluginRequest) -> ContractResult<HostResponse> {
        self.peer.lock().call(req)
    }

    fn unit(&self, req: PluginRequest) -> ContractResult<()> {
        match self.call(req)? {
            HostResponse::Unit => Ok(()),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn next_operation(&self) -> PluginOperationToken {
        loop {
            let raw = self.next_operation.fetch_add(1, Ordering::Relaxed);
            if let Some(raw) = NonZeroU64::new(raw) {
                return PluginOperationToken::from_raw(raw);
            }
        }
    }

    fn take_deferred_request(&self) -> Option<HostRequest> {
        self.peer.lock().deferred_requests.pop_front()
    }
}

impl PluginTaskHost for RpcHost {
    fn start(
        &mut self,
        plugin: PluginId,
        request: PluginTaskRequest,
    ) -> ContractResult<PluginOperationToken> {
        let operation = self.next_operation();
        self.unit(PluginRequest::StartTask {
            plugin,
            operation,
            request,
        })?;
        Ok(operation)
    }

    fn cancel(&mut self, plugin: PluginId, operation: PluginOperationToken) {
        let _ = self.unit(PluginRequest::CancelTask { plugin, operation });
    }
}

impl PluginQueryHost for RpcHost {
    fn api_metadata(&self) -> metadata::ApiMetadata {
        match self.call(PluginRequest::ApiMetadata) {
            Ok(HostResponse::ApiMetadata(metadata)) => metadata,
            Err(err) => {
                eprintln!("helix-plugin-host: api_metadata failed: {err}");
                metadata::ApiMetadata::default()
            }
            Ok(other) => {
                eprintln!("helix-plugin-host: unexpected api_metadata response: {other:?}");
                metadata::ApiMetadata::default()
            }
        }
    }

    fn focused_document(&self) -> Option<DocumentHandle> {
        match self.call(PluginRequest::FocusedDocument).ok()? {
            HostResponse::OptionDocumentHandle(handle) => handle,
            _ => None,
        }
    }

    fn focused_view(&self) -> Option<ViewHandle> {
        match self.call(PluginRequest::FocusedView).ok()? {
            HostResponse::OptionViewHandle(handle) => handle,
            _ => None,
        }
    }

    fn list_documents(&self) -> Vec<DocumentHandle> {
        match self.call(PluginRequest::ListDocuments) {
            Ok(HostResponse::DocumentHandles(handles)) => handles,
            _ => Vec::new(),
        }
    }

    fn list_views(&self) -> Vec<ViewHandle> {
        match self.call(PluginRequest::ListViews) {
            Ok(HostResponse::ViewHandles(handles)) => handles,
            _ => Vec::new(),
        }
    }

    fn language_servers(&self) -> ContractResult<Vec<snapshots::LanguageServerSnapshot>> {
        match self.call(PluginRequest::LanguageServers)? {
            HostResponse::LanguageServers(servers) => Ok(servers),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn document_snapshot(
        &self,
        handle: DocumentHandle,
    ) -> ContractResult<snapshots::DocumentSnapshot> {
        match self.call(PluginRequest::DocumentSnapshot(handle))? {
            HostResponse::DocumentSnapshot(snapshot) => Ok(snapshot),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn view_snapshot(&self, handle: ViewHandle) -> ContractResult<snapshots::ViewSnapshot> {
        match self.call(PluginRequest::ViewSnapshot(handle))? {
            HostResponse::ViewSnapshot(snapshot) => Ok(snapshot),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn workspace_snapshot(&self) -> snapshots::WorkspaceSnapshot {
        match self.call(PluginRequest::WorkspaceSnapshot) {
            Ok(HostResponse::WorkspaceSnapshot(snapshot)) => snapshot,
            _ => snapshots::WorkspaceSnapshot {
                focused_document: None,
                focused_view: None,
                documents: Vec::new(),
                views: Vec::new(),
                mode: snapshots::EditMode::Normal,
            },
        }
    }

    fn theme_snapshot(&self) -> snapshots::ThemeSnapshot {
        match self.call(PluginRequest::ThemeSnapshot) {
            Ok(HostResponse::ThemeSnapshot(snapshot)) => snapshot,
            _ => snapshots::ThemeSnapshot {
                name: String::new(),
                bg: None,
                fg: None,
                selection: None,
                cursor: None,
            },
        }
    }

    fn diagnostics(&self, handle: DocumentHandle) -> ContractResult<snapshots::DiagnosticSnapshot> {
        match self.call(PluginRequest::Diagnostics(handle))? {
            HostResponse::DiagnosticSnapshot(snapshot) => Ok(snapshot),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn document_text(&self, handle: DocumentHandle) -> ContractResult<String> {
        match self.call(PluginRequest::DocumentText(handle))? {
            HostResponse::DocumentText(text) => Ok(text),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn document_line(&self, handle: DocumentHandle, line: usize) -> ContractResult<String> {
        match self.call(PluginRequest::DocumentLine {
            document: handle,
            line,
        })? {
            HostResponse::DocumentLine(text) => Ok(text),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }
}

impl PluginMutationHost for RpcHost {
    fn apply_edit(&mut self, req: requests::ApplyEditRequest) -> ContractResult<()> {
        self.unit(PluginRequest::ApplyEdit(req))
    }

    fn set_selection(&mut self, req: requests::SetSelectionRequest) -> ContractResult<()> {
        self.unit(PluginRequest::SetSelection(req))
    }

    fn save_document(&mut self, req: requests::SaveDocumentRequest) -> ContractResult<()> {
        self.unit(PluginRequest::SaveDocument(req))
    }

    fn focus_view(&mut self, req: requests::FocusViewRequest) -> ContractResult<()> {
        self.unit(PluginRequest::FocusView(req))
    }

    fn set_annotations(&mut self, req: requests::SetAnnotationsRequest) -> ContractResult<()> {
        self.unit(PluginRequest::SetAnnotations(req))
    }

    fn set_status(&mut self, req: requests::SetStatusRequest) -> ContractResult<()> {
        self.unit(PluginRequest::SetStatus(req))
    }

    fn undo(&mut self, req: requests::UndoRequest) -> ContractResult<bool> {
        match self.call(PluginRequest::Undo(req))? {
            HostResponse::Bool(value) => Ok(value),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn redo(&mut self, req: requests::RedoRequest) -> ContractResult<bool> {
        match self.call(PluginRequest::Redo(req))? {
            HostResponse::Bool(value) => Ok(value),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn select_all(&mut self, req: requests::SelectAllRequest) -> ContractResult<()> {
        self.unit(PluginRequest::SelectAll(req))
    }

    fn set_mode(&mut self, req: requests::SetModeRequest) -> ContractResult<()> {
        self.unit(PluginRequest::SetMode(req))
    }

    fn close_view(&mut self, req: requests::CloseViewRequest) -> ContractResult<()> {
        self.unit(PluginRequest::CloseView(req))
    }
}

impl PluginUiHost for RpcHost {
    fn notify(&mut self, req: requests::NotifyRequest) -> ContractResult<()> {
        self.unit(PluginRequest::Notify(req))
    }

    fn prompt(
        &mut self,
        plugin: PluginId,
        request: requests::PromptRequest,
    ) -> ContractResult<UiCallbackToken> {
        ui_callback(self.call(PluginRequest::Prompt { plugin, request })?)
    }

    fn confirm(
        &mut self,
        plugin: PluginId,
        request: requests::ConfirmRequest,
    ) -> ContractResult<UiCallbackToken> {
        ui_callback(self.call(PluginRequest::Confirm { plugin, request })?)
    }

    fn picker(
        &mut self,
        plugin: PluginId,
        request: requests::PickerRequest,
    ) -> ContractResult<UiCallbackToken> {
        ui_callback(self.call(PluginRequest::Picker { plugin, request })?)
    }
}

fn ui_callback(response: HostResponse) -> ContractResult<UiCallbackToken> {
    match response {
        HostResponse::UiCallback(callback) => Ok(callback),
        other => Err(internal(format!("unexpected response: {other:?}"))),
    }
}

impl PluginPanelHost for RpcHost {
    fn register_panel(
        &mut self,
        plugin: PluginId,
        registration: requests::PanelRegistration,
    ) -> ContractResult<PanelHandle> {
        match self.call(PluginRequest::RegisterPanel {
            plugin,
            registration,
        })? {
            HostResponse::PanelHandle(handle) => Ok(handle),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn update_panel(
        &mut self,
        plugin: PluginId,
        request: requests::PanelUpdateRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::UpdatePanel { plugin, request })
    }

    fn close_panel(
        &mut self,
        plugin: PluginId,
        request: requests::PanelCloseRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::ClosePanel { plugin, request })
    }

    fn toggle_panel(
        &mut self,
        plugin: PluginId,
        request: requests::TogglePanelRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::TogglePanel { plugin, request })
    }

    fn focus_panel(
        &mut self,
        plugin: PluginId,
        request: requests::FocusPanelRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::FocusPanel { plugin, request })
    }

    fn resize_panel(
        &mut self,
        plugin: PluginId,
        request: requests::ResizePanelRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::ResizePanel { plugin, request })
    }

    fn list_panels(&self) -> Vec<snapshots::PanelSnapshot> {
        match self.call(PluginRequest::ListPanels) {
            Ok(HostResponse::PanelSnapshots(panels)) => panels,
            _ => Vec::new(),
        }
    }
}

impl PluginResourceHost for RpcHost {
    fn release_plugin_resources(&mut self, plugin: PluginId) -> ContractResult<()> {
        self.unit(PluginRequest::ReleaseResources { plugin })
    }
}

impl PluginCommandHost for RpcHost {
    fn command_catalog(&self) -> Vec<CommandDescriptor> {
        match self.call(PluginRequest::CommandCatalog) {
            Ok(HostResponse::CommandCatalog(commands)) => commands,
            _ => Vec::new(),
        }
    }

    fn register_command(
        &mut self,
        plugin: PluginId,
        definition: requests::CommandDefinition,
    ) -> ContractResult<CommandHandle> {
        match self.call(PluginRequest::RegisterCommand { plugin, definition })? {
            HostResponse::CommandHandle(handle) => Ok(handle),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn update_command(
        &mut self,
        plugin: PluginId,
        request: requests::CommandUpdateRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::UpdateCommand { plugin, request })
    }

    fn remove_command(
        &mut self,
        plugin: PluginId,
        request: requests::CommandRemoveRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::RemoveCommand { plugin, request })
    }
}

impl PluginKeymapHost for RpcHost {
    fn register_keymap(
        &mut self,
        plugin: PluginId,
        definition: KeymapDefinition,
    ) -> ContractResult<KeymapHandle> {
        match self.call(PluginRequest::RegisterKeymap { plugin, definition })? {
            HostResponse::KeymapHandle(handle) => Ok(handle),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn update_keymap(
        &mut self,
        plugin: PluginId,
        request: KeymapUpdateRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::UpdateKeymap { plugin, request })
    }

    fn remove_keymap(
        &mut self,
        plugin: PluginId,
        request: KeymapRemoveRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::RemoveKeymap { plugin, request })
    }
}

impl PluginEventHost for RpcHost {
    fn subscribe(
        &mut self,
        plugin: PluginId,
        kind: events::EventKind,
    ) -> ContractResult<SubscriptionHandle> {
        match self.call(PluginRequest::Subscribe { plugin, kind })? {
            HostResponse::SubscriptionHandle(handle) => Ok(handle),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn unsubscribe(&mut self, plugin: PluginId, handle: SubscriptionHandle) -> ContractResult<()> {
        self.unit(PluginRequest::Unsubscribe { plugin, handle })
    }

    fn event_catalog(&self) -> Vec<metadata::EventKindInfo> {
        match self.call(PluginRequest::EventCatalog) {
            Ok(HostResponse::EventCatalog(catalog)) => catalog,
            _ => Vec::new(),
        }
    }
}

impl PluginSplitHost for RpcHost {
    fn split_view(&mut self, req: requests::SplitViewRequest) -> ContractResult<ViewHandle> {
        match self.call(PluginRequest::SplitView(req))? {
            HostResponse::ViewHandle(handle) => Ok(handle),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn focus_direction(
        &mut self,
        req: requests::FocusDirectionRequest,
    ) -> ContractResult<Option<ViewHandle>> {
        match self.call(PluginRequest::FocusDirection(req))? {
            HostResponse::OptionViewHandleResult(handle) => Ok(handle),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn swap_split(&mut self, req: requests::SwapSplitRequest) -> ContractResult<()> {
        self.unit(PluginRequest::SwapSplit(req))
    }

    fn resize_split(&mut self, req: requests::ResizeSplitRequest) -> ContractResult<()> {
        self.unit(PluginRequest::ResizeSplit(req))
    }

    fn transpose(&mut self, req: requests::TransposeSplitRequest) -> ContractResult<()> {
        self.unit(PluginRequest::Transpose(req))
    }

    fn split_tree(&self) -> snapshots::SplitTreeSnapshot {
        match self.call(PluginRequest::SplitTree) {
            Ok(HostResponse::SplitTree(tree)) => tree,
            _ => snapshots::SplitTreeSnapshot {
                root: snapshots::SplitNodeSnapshot::Leaf {
                    view: ViewHandle::from_raw(nonzero_one()),
                },
            },
        }
    }
}

impl PluginTabHost for RpcHost {
    fn open_tab(&mut self, req: requests::OpenTabRequest) -> ContractResult<()> {
        self.unit(PluginRequest::OpenTab(req))
    }

    fn close_tab(&mut self, req: requests::CloseTabRequest) -> ContractResult<()> {
        self.unit(PluginRequest::CloseTab(req))
    }

    fn focus_tab(&mut self, req: requests::FocusTabRequest) -> ContractResult<()> {
        self.unit(PluginRequest::FocusTab(req))
    }

    fn cycle_tab(&mut self, req: requests::CycleTabRequest) -> ContractResult<()> {
        self.unit(PluginRequest::CycleTab(req))
    }

    fn list_tabs(&self, view: Option<ViewHandle>) -> ContractResult<snapshots::TabGroupSnapshot> {
        match self.call(PluginRequest::ListTabs(view))? {
            HostResponse::TabGroup(tabs) => Ok(tabs),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }
}

impl PluginFloatHost for RpcHost {
    fn create_float(
        &mut self,
        plugin: PluginId,
        request: requests::CreateFloatRequest,
    ) -> ContractResult<FloatHandle> {
        match self.call(PluginRequest::CreateFloat { plugin, request })? {
            HostResponse::FloatHandle(handle) => Ok(handle),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn update_float(
        &mut self,
        plugin: PluginId,
        request: requests::UpdateFloatRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::UpdateFloat { plugin, request })
    }

    fn close_float(
        &mut self,
        plugin: PluginId,
        request: requests::CloseFloatRequest,
    ) -> ContractResult<()> {
        self.unit(PluginRequest::CloseFloat { plugin, request })
    }

    fn list_floats(&self, plugin: PluginId) -> Vec<snapshots::FloatSnapshot> {
        match self.call(PluginRequest::ListFloats(plugin)) {
            Ok(HostResponse::FloatSnapshots(floats)) => floats,
            _ => Vec::new(),
        }
    }
}

impl PluginAssistantQueryHost for RpcHost {
    fn assistant_snapshot(&self) -> snapshots::AssistantSnapshot {
        match self.call(PluginRequest::AssistantSnapshot) {
            Ok(HostResponse::AssistantSnapshot(snapshot)) => snapshot,
            _ => snapshots::AssistantSnapshot {
                active_thread: None,
                threads: Vec::new(),
                is_ready: false,
            },
        }
    }

    fn thread_snapshot(
        &self,
        thread: ThreadHandle,
    ) -> ContractResult<snapshots::AssistantThreadSnapshot> {
        match self.call(PluginRequest::ThreadSnapshot(thread))? {
            HostResponse::AssistantThreadSnapshot(snapshot) => Ok(snapshot),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn thread_entries(
        &self,
        thread: ThreadHandle,
    ) -> ContractResult<Vec<snapshots::AssistantEntrySnapshot>> {
        match self.call(PluginRequest::ThreadEntries(thread))? {
            HostResponse::AssistantEntries(entries) => Ok(entries),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn thread_context(
        &self,
        thread: ThreadHandle,
    ) -> ContractResult<Vec<snapshots::AssistantContextSnapshot>> {
        match self.call(PluginRequest::ThreadContext(thread))? {
            HostResponse::AssistantContext(items) => Ok(items),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }
}

impl PluginAssistantMutationHost for RpcHost {
    fn submit_prompt(&mut self, thread: Option<ThreadHandle>, text: String) -> ContractResult<()> {
        self.unit(PluginRequest::SubmitPrompt { thread, text })
    }

    fn cancel_thread(&mut self, thread: Option<ThreadHandle>) -> ContractResult<()> {
        self.unit(PluginRequest::CancelThread(thread))
    }
}

impl PluginWorkspaceQueryHost for RpcHost {
    fn workspace_detail(&self) -> snapshots::WorkspaceDetailSnapshot {
        match self.call(PluginRequest::WorkspaceDetail) {
            Ok(HostResponse::WorkspaceDetail(snapshot)) => snapshot,
            _ => snapshots::WorkspaceDetailSnapshot {
                focused_document: self.focused_document(),
                focused_view: self.focused_view(),
                documents: self.list_documents(),
                views: self.list_views(),
                mode: snapshots::EditMode::Normal,
                splits: PluginSplitHost::split_tree(self),
                panels: Vec::new(),
                floats: Vec::new(),
                focus: snapshots::FocusTargetSnapshot::Editor,
            },
        }
    }
}

impl PluginFacadeQueryHost for RpcHost {
    fn split_tree(&self) -> snapshots::SplitTreeSnapshot {
        PluginSplitHost::split_tree(self)
    }

    fn list_tabs(&self, view: Option<ViewHandle>) -> ContractResult<snapshots::TabGroupSnapshot> {
        PluginTabHost::list_tabs(self, view)
    }

    fn editor_config(&self) -> ContractResult<snapshots::EditorConfigSnapshot> {
        match self.call(PluginRequest::EditorConfig)? {
            HostResponse::EditorConfig(config) => Ok(config),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn terminal_size(&self) -> ContractResult<snapshots::TerminalSizeSnapshot> {
        match self.call(PluginRequest::TerminalSize)? {
            HostResponse::TerminalSize(size) => Ok(size),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    fn read_register(&self, name: char) -> ContractResult<Vec<String>> {
        match self.call(PluginRequest::ReadRegister(name))? {
            HostResponse::Strings(values) => Ok(values),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }
}

impl PluginFacadeMutationHost for RpcHost {
    fn write_register(&mut self, name: char, values: Vec<String>) -> ContractResult<()> {
        self.unit(PluginRequest::WriteRegister { name, values })
    }

    fn request_redraw(&mut self) {
        let _ = self.unit(PluginRequest::RequestRedraw);
    }
}

fn parse_args(config: &mut PluginConfig) {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--plugin-host" => {}
            "--plugin-dir" => {
                if let Some(dir) = args.next() {
                    config.plugin_dirs.push(PathBuf::from(dir));
                }
            }
            "--help" | "-h" => {
                eprintln!("usage: helix-plugin-host [--plugin-dir PATH]...");
                std::process::exit(0);
            }
            other => eprintln!("helix-plugin-host: ignoring unknown argument: {other}"),
        }
    }
}

fn load_plugins(
    engine: &mut LuaEngine,
    host: &mut RpcHost,
    config: &PluginConfig,
) -> crate::Result<()> {
    let plugin_dirs = if config.plugin_dirs.is_empty() {
        PluginLoader::default_plugin_dirs()
    } else {
        config.plugin_dirs.clone()
    };
    let plugin_roots = PluginLoader::active_plugin_roots()?;
    let loader = PluginLoader::new(plugin_dirs).with_plugin_roots(plugin_roots);
    let plugins = loader.discover_plugins()?;
    for plugin in plugins {
        if let Some(plugin_config) = config
            .plugins
            .iter()
            .find(|p| p.name == plugin.metadata.name)
        {
            if !plugin_config.enabled {
                continue;
            }
        }
        if let Err(err) = engine.load_plugin(host, plugin) {
            eprintln!("helix-plugin-host: failed to load plugin: {err}");
        }
    }
    Ok(())
}

fn configure_engine_hosts(engine: &mut LuaEngine, host: &RpcHost) {
    engine.set_ui_host(Box::new(host.clone()));
    engine.set_task_host(Box::new(host.clone()));
    engine.set_panel_host(Box::new(host.clone()));
    engine.set_resource_host(Box::new(host.clone()));
    engine.set_command_host(Box::new(host.clone()));
    engine.set_keymap_host(Box::new(host.clone()));
    engine.set_event_host(Box::new(host.clone()));
}

fn reload_plugins(
    engine: &mut LuaEngine,
    host: &mut RpcHost,
    config: &PluginConfig,
    api_metadata: &metadata::ApiMetadata,
) -> crate::Result<()> {
    engine.reset()?;
    configure_engine_hosts(engine, host);
    engine.set_api_metadata(api_metadata.clone());
    engine.register_api(config.clone())?;
    load_plugins(engine, host, config)
}

fn callback_id(callback: UiCallbackToken) -> UiCallbackId {
    UiCallbackId::new(callback.raw().get()).expect("UI callback tokens are non-zero")
}

fn dispatch_host_notification(
    engine: &mut LuaEngine,
    host: &mut RpcHost,
    config: &PluginConfig,
    api_metadata: &metadata::ApiMetadata,
    request: HostRequest,
) -> crate::Result<bool> {
    match request {
        HostRequest::Event(event) => engine.call_event_handlers(host, &event)?,
        HostRequest::CommandInvoke { command, args } => {
            engine.execute_command_handle(host, command, args)?
        }
        HostRequest::UiCallback { callback, value } => {
            engine.handle_ui_callback(host, callback_id(callback), value)?
        }
        HostRequest::PanelKey { panel, key } => {
            engine.handle_panel_key(panel, &key)?;
        }
        HostRequest::Reload => reload_plugins(engine, host, config, api_metadata)?,
        HostRequest::TaskCompleted { operation, result } => {
            engine.handle_task_completion(host, operation, result)?;
        }
        HostRequest::Shutdown => return Ok(true),
        HostRequest::Init { .. } => {
            return Err(crate::PluginError::ApiAccessError("duplicate Init".into()))
        }
    }
    Ok(false)
}

fn drain_deferred_requests(
    engine: &mut LuaEngine,
    host: &mut RpcHost,
    config: &PluginConfig,
    api_metadata: &metadata::ApiMetadata,
) -> crate::Result<bool> {
    while let Some(request) = host.take_deferred_request() {
        if dispatch_host_notification(engine, host, config, api_metadata, request)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn run_plugin_host() {
    let peer = Arc::new(Mutex::new(Peer::new()));
    let mut host = RpcHost::new(Arc::clone(&peer));

    let (init, metadata) = match peer.lock().read() {
        Ok(Frame::Request {
            body:
                HostRequest::Init {
                    mut config,
                    metadata,
                },
            ..
        })
        | Ok(Frame::Notify {
            body:
                HostRequest::Init {
                    mut config,
                    metadata,
                },
        }) => {
            parse_args(&mut config);
            (config, metadata)
        }
        Ok(_) => {
            eprintln!("helix-plugin-host: expected Init as first frame");
            return;
        }
        Err(err) => {
            eprintln!("helix-plugin-host: failed to read Init: {err}");
            return;
        }
    };

    let mut engine = match LuaEngine::new() {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("helix-plugin-host: Lua initialization failed: {err}");
            return;
        }
    };
    configure_engine_hosts(&mut engine, &host);
    engine.set_api_metadata(metadata.clone());
    if let Err(err) = engine.register_api(init.clone()) {
        eprintln!("helix-plugin-host: API registration failed: {err}");
        return;
    }
    if let Err(err) = load_plugins(&mut engine, &mut host, &init) {
        eprintln!("helix-plugin-host: plugin discovery failed: {err}");
        return;
    }
    match drain_deferred_requests(&mut engine, &mut host, &init, &metadata) {
        Ok(false) => {}
        Ok(true) => return,
        Err(err) => {
            eprintln!("helix-plugin-host: initialization dispatch failed: {err}");
            return;
        }
    }
    loop {
        let frame = match peer.lock().read() {
            Ok(frame) => frame,
            Err(err) if err.to_string().contains("early eof") => break,
            Err(err) => {
                eprintln!("helix-plugin-host: read failed: {err}");
                break;
            }
        };

        match frame {
            Frame::Notify { body } => {
                match dispatch_host_notification(&mut engine, &mut host, &init, &metadata, body) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(err) => eprintln!("helix-plugin-host: notification failed: {err}"),
                }
            }
            Frame::Request { id, body } => {
                let result = match body {
                    HostRequest::Event(event) => engine
                        .call_event_handlers(&mut host, &event)
                        .map(|()| PluginResponse::Unit)
                        .map_err(|err| internal(err.to_string())),
                    HostRequest::CommandInvoke { command, args } => engine
                        .execute_command_handle(&mut host, command, args)
                        .map(|()| PluginResponse::Unit)
                        .map_err(|err| internal(err.to_string())),
                    HostRequest::TaskCompleted { operation, result } => engine
                        .handle_task_completion(&mut host, operation, result)
                        .map(|_| PluginResponse::Unit)
                        .map_err(|err| internal(err.to_string())),
                    HostRequest::UiCallback { callback, value } => engine
                        .handle_ui_callback(&mut host, callback_id(callback), value)
                        .map(|()| PluginResponse::Unit)
                        .map_err(|err| internal(err.to_string())),
                    HostRequest::PanelKey { panel, key } => engine
                        .handle_panel_key(panel, &key)
                        .map(PluginResponse::Bool)
                        .map_err(|err| internal(err.to_string())),
                    HostRequest::Reload => reload_plugins(&mut engine, &mut host, &init, &metadata)
                        .map(|()| PluginResponse::Unit)
                        .map_err(|err| internal(err.to_string())),
                    HostRequest::Shutdown => break,
                    HostRequest::Init { .. } => Err(internal("duplicate Init")),
                };
                if let Err(err) = peer.lock().respond(id, result) {
                    eprintln!("helix-plugin-host: response write failed: {err}");
                    break;
                }
            }
            Frame::Response { .. } => {
                eprintln!("helix-plugin-host: unexpected response frame");
            }
        }

        match drain_deferred_requests(&mut engine, &mut host, &init, &metadata) {
            Ok(false) => {}
            Ok(true) => break,
            Err(err) => eprintln!("helix-plugin-host: deferred notification failed: {err}"),
        }
    }
}
