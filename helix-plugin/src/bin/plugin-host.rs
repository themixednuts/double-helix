use helix_plugin::contract::host::*;
use helix_plugin::contract::*;
use helix_plugin::contract::{events, metadata, requests, snapshots};
use helix_plugin::lua::loader::PluginLoader;
use helix_plugin::rpc::*;
use helix_plugin::{LuaEngine, PluginConfig};
use parking_lot::Mutex;
use std::num::NonZeroU64;
use std::path::PathBuf;
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
}

impl Peer {
    fn new() -> Self {
        Self {
            codec: FrameCodec::new(),
            stdin: std::io::stdin(),
            stdout: std::io::stdout(),
            next_id: 1,
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
        match self.codec.read_sync::<In, _>(&mut self.stdin) {
            Ok(Frame::Response { id: res_id, result }) if res_id == id => result,
            Ok(Frame::Notify {
                body: HostRequest::Shutdown,
            }) => Err(internal("plugin host shut down while waiting for response")),
            Ok(_) => Err(internal("unexpected rpc frame while waiting for response")),
            Err(err) => Err(internal(err.to_string())),
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
}

impl RpcHost {
    fn new(peer: Arc<Mutex<Peer>>) -> Self {
        Self { peer }
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
    fn open_document(
        &mut self,
        req: requests::OpenDocumentRequest,
    ) -> ContractResult<DocumentHandle> {
        match self.call(PluginRequest::OpenDocument(req))? {
            HostResponse::DocumentHandle(handle) => Ok(handle),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

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
        HostResponse::UiCallback(id) => NonZeroU64::new(id)
            .map(UiCallbackToken::from_raw)
            .ok_or_else(|| internal("zero UI callback token")),
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

impl PluginCommandHost for RpcHost {
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

    fn run_command(&mut self, request: requests::RunCommandRequest) -> ContractResult<()> {
        self.unit(PluginRequest::RunCommand(request))
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

    fn update_float(&mut self, req: requests::UpdateFloatRequest) -> ContractResult<()> {
        self.unit(PluginRequest::UpdateFloat(req))
    }

    fn close_float(&mut self, req: requests::CloseFloatRequest) -> ContractResult<()> {
        self.unit(PluginRequest::CloseFloat(req))
    }

    fn list_floats(&self) -> Vec<snapshots::FloatSnapshot> {
        match self.call(PluginRequest::ListFloats) {
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
}

impl PluginFacadeMutationHost for RpcHost {}

fn parse_args(config: &mut PluginConfig) {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
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

fn load_plugins(engine: &mut LuaEngine, host: &mut RpcHost, config: &PluginConfig) {
    let plugin_dirs = if config.plugin_dirs.is_empty() {
        PluginLoader::default_plugin_dirs()
    } else {
        config.plugin_dirs.clone()
    };
    let loader = PluginLoader::new(plugin_dirs);
    let plugins = match loader.discover_plugins() {
        Ok(plugins) => plugins,
        Err(err) => {
            eprintln!("helix-plugin-host: plugin discovery failed: {err}");
            return;
        }
    };
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
        if let Err(err) = engine.load_plugin_remote(host, plugin) {
            eprintln!("helix-plugin-host: failed to load plugin: {err}");
        }
    }
}

fn main() {
    let peer = Arc::new(Mutex::new(Peer::new()));
    let mut host = RpcHost::new(Arc::clone(&peer));

    let init = match peer.lock().read() {
        Ok(Frame::Request {
            body: HostRequest::Init { mut config, .. },
            ..
        })
        | Ok(Frame::Notify {
            body: HostRequest::Init { mut config, .. },
        }) => {
            parse_args(&mut config);
            config
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
    engine.set_ui_host(Box::new(host.clone()));
    engine.set_panel_host(Box::new(host.clone()));
    engine.set_command_host(Box::new(host.clone()));
    engine.set_event_host(Box::new(host.clone()));
    if let Err(err) = engine.register_api(init.clone()) {
        eprintln!("helix-plugin-host: API registration failed: {err}");
        return;
    }
    load_plugins(&mut engine, &mut host, &init);

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
            Frame::Notify {
                body: HostRequest::Event(event),
            } => {
                if let Err(err) = engine.call_event_handlers_remote(&mut host, &event) {
                    eprintln!("helix-plugin-host: event dispatch failed: {err}");
                }
            }
            Frame::Notify {
                body: HostRequest::Shutdown,
            } => break,
            Frame::Request { id, body } => {
                let result = match body {
                    HostRequest::Event(event) => engine
                        .call_event_handlers_remote(&mut host, &event)
                        .map(|()| PluginResponse::Unit)
                        .map_err(|err| internal(err.to_string())),
                    HostRequest::CommandInvoke { name, args } => engine
                        .execute_command_remote(&mut host, &name, args)
                        .map(|()| PluginResponse::Unit)
                        .map_err(|err| internal(err.to_string())),
                    HostRequest::UiCallback {
                        callback_id: _,
                        value: _,
                    } => Ok(PluginResponse::Unit),
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
            Frame::Notify {
                body: HostRequest::CommandInvoke { name, args },
            } => {
                if let Err(err) = engine.execute_command_remote(&mut host, &name, args) {
                    eprintln!("helix-plugin-host: command failed: {err}");
                }
            }
            Frame::Notify {
                body: HostRequest::UiCallback { .. },
            }
            | Frame::Notify {
                body: HostRequest::Init { .. },
            } => {}
        }
    }
}
