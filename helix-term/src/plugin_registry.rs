use crate::runtime::ui::command::PluginCommand;
use crate::runtime::UiCommand;
use helix_plugin::rpc::{Frame, FrameCodec, HostRequest, HostResponse, PluginRequest};
use helix_plugin::{PluginConfig, PluginHostConfig};
use helix_plugin_api::host::{
    PluginCommandHost, PluginEventHost, PluginKeymapHost, PluginPanelHost, PluginResourceHost,
    PluginUiHost,
};
use helix_plugin_api::metadata::{ApiMetadata, EventKindInfo};
use helix_plugin_api::requests::{
    self as contract_requests, CommandDefinition, CommandRemoveRequest, CommandUpdateRequest,
    NotifyRequest, PanelCloseRequest, PanelUpdateRequest, ResizePanelRequest, TogglePanelRequest,
};
use helix_plugin_api::{
    CommandHandle, ContractError, ContractResult, KeymapHandle, KeymapRemoveRequest,
    KeymapUpdateRequest, PanelHandle, PluginId, PluginOperationToken, SubscriptionHandle,
    UiCallbackToken,
};
use helix_plugin_editor::adapt;
use helix_view::model::FocusTarget;
use helix_view::Editor;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

fn internal_error(message: impl Into<String>) -> ContractError {
    ContractError::internal(message)
}

fn next_non_zero(counter: &std::sync::atomic::AtomicU64) -> NonZeroU64 {
    loop {
        let raw = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(id) = NonZeroU64::new(raw) {
            return id;
        }
    }
}

fn permission_denied(plugin: PluginId, handle: impl std::fmt::Display) -> ContractError {
    ContractError::permission_denied(format!("plugin {plugin} does not own {handle}"))
}

static NEXT_PLUGIN_HOST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static NEXT_HOST_GENERATION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginHostId(NonZeroU64);

impl PluginHostId {
    fn next() -> Self {
        Self(next_non_zero(&NEXT_PLUGIN_HOST_ID))
    }

    #[cfg(test)]
    fn from_raw(raw: NonZeroU64) -> Self {
        Self(raw)
    }
}

impl std::fmt::Display for PluginHostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plugin-host({})", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct HostGenerationId(NonZeroU64);

impl HostGenerationId {
    fn next() -> Self {
        Self(next_non_zero(&NEXT_HOST_GENERATION_ID))
    }
}

#[derive(Clone)]
struct PluginHostRoute {
    id: PluginHostId,
    name: Arc<str>,
    outbound: helix_runtime::Sender<HostOutbound>,
    work: helix_runtime::Work,
    active_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl std::fmt::Debug for PluginHostRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHostRoute")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl PluginHostRoute {
    fn try_notify(&self, request: HostRequest) -> ContractResult<()> {
        self.try_notify_outbound(HostOutbound::Notify {
            generation: None,
            request,
        })
    }

    fn try_notify_for_generation(
        &self,
        generation: HostGenerationId,
        request: HostRequest,
    ) -> ContractResult<()> {
        if !self.is_active(generation) {
            return Err(ContractError::stale_handle(format!(
                "{} generation {}",
                self.id, generation.0
            )));
        }
        self.try_notify_outbound(HostOutbound::Notify {
            generation: Some(generation),
            request,
        })
    }

    fn try_notify_outbound(&self, outbound: HostOutbound) -> ContractResult<()> {
        match self.outbound.try_send(outbound) {
            Ok(()) => Ok(()),
            Err(helix_runtime::TrySend::Full(_)) => Err(ContractError::Busy {
                reason: format!("plugin host '{}' outbound lane is full", self.name),
            }),
            Err(helix_runtime::TrySend::Closed(_)) => Err(internal_error(format!(
                "plugin host '{}' is closed",
                self.name
            ))),
        }
    }

    fn notify_on_work(
        &self,
        generation: HostGenerationId,
        request: HostRequest,
        kind: &'static str,
    ) {
        let outbound = self.outbound.clone();
        let active_generation = Arc::clone(&self.active_generation);
        let id = self.id;
        let name = Arc::clone(&self.name);
        self.work
            .spawn(async move {
                if active_generation.load(std::sync::atomic::Ordering::Acquire)
                    != generation.0.get()
                {
                    return;
                }
                if let Err(error) = outbound
                    .send(HostOutbound::Notify {
                        generation: Some(generation),
                        request,
                    })
                    .await
                {
                    log::debug!("plugin host '{name}' ({id}) dropped {kind}: {error}");
                }
            })
            .detach();
    }

    fn activate(&self, generation: HostGenerationId) {
        self.active_generation
            .store(generation.0.get(), std::sync::atomic::Ordering::Release);
    }

    fn deactivate(&self, generation: HostGenerationId) {
        let _ = self.active_generation.compare_exchange(
            generation.0.get(),
            0,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }

    fn active_generation(&self) -> Option<HostGenerationId> {
        NonZeroU64::new(
            self.active_generation
                .load(std::sync::atomic::Ordering::Acquire),
        )
        .map(HostGenerationId)
    }

    fn is_active(&self, generation: HostGenerationId) -> bool {
        self.active_generation() == Some(generation)
    }
}

#[derive(Debug, Clone)]
pub struct PluginUiCallback {
    route: PluginHostRoute,
    generation: Option<HostGenerationId>,
    callback: UiCallbackToken,
    completed: Arc<std::sync::atomic::AtomicBool>,
}

impl PluginUiCallback {
    fn new(route: PluginHostRoute, callback: UiCallbackToken) -> Self {
        Self {
            generation: route.active_generation(),
            route,
            callback,
            completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub(crate) fn send(&self, value: helix_plugin_api::DynamicValue) {
        if self
            .completed
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let Some(generation) = self.generation else {
            return;
        };
        self.route.notify_on_work(
            generation,
            HostRequest::UiCallback {
                callback: self.callback,
                value,
            },
            "UI callback",
        );
    }

    #[cfg(test)]
    fn identity(&self) -> (PluginHostId, UiCallbackToken) {
        (self.route.id, self.callback)
    }
}

#[derive(Debug, Clone)]
pub struct PluginPanelKeyRoute {
    route: PluginHostRoute,
    generation: Option<HostGenerationId>,
    panel: PanelHandle,
}

impl PluginPanelKeyRoute {
    fn new(route: PluginHostRoute, panel: PanelHandle) -> Self {
        Self {
            generation: route.active_generation(),
            route,
            panel,
        }
    }

    pub(crate) fn dispatch(&self, key: String) {
        let Some(generation) = self.generation else {
            return;
        };
        if let Err(error) = self.route.try_notify_for_generation(
            generation,
            HostRequest::PanelKey {
                panel: self.panel,
                key,
            },
        ) {
            log::warn!(
                "dropping plugin panel key host={} panel={}: {error}",
                self.route.id,
                self.panel
            );
        }
    }
}

#[derive(Clone, Debug)]
pub struct PluginTaskResponder(HostTaskResponder);

impl From<HostTaskResponder> for PluginTaskResponder {
    fn from(responder: HostTaskResponder) -> Self {
        Self(responder)
    }
}

impl PluginTaskResponder {
    pub(crate) fn complete_foreground(
        &self,
        result: ContractResult<helix_plugin_api::PluginTaskResult>,
    ) -> ContractResult<()> {
        self.0.send(result);
        Ok(())
    }

    pub(crate) async fn complete_async(
        &self,
        result: ContractResult<helix_plugin_api::PluginTaskResult>,
    ) {
        self.0.send(result);
    }
}

pub(crate) fn spawn_theme_load(
    runtime: helix_runtime::Runtime,
    loader: Arc<helix_view::theme::Loader>,
    name: String,
    cancellation: helix_runtime::Token,
    ingress: crate::runtime::RuntimeIngress,
    completion: PluginTaskResponder,
) {
    let blocking = runtime.block().spawn(move || {
        loader
            .load(&name)
            .map_err(|error| ContractError::not_found(format!("theme {name}: {error}")))
    });
    let work = runtime.work().clone();
    work.spawn(async move {
        let result = blocking
            .await
            .map_err(|error| ContractError::internal(error.to_string()))
            .and_then(|result| result);
        if cancellation.is_canceled() {
            return;
        }
        match result {
            Ok(theme) => {
                let _ = ingress
                    .send_ui(UiCommand::Plugin(PluginCommand::SetTheme {
                        theme,
                        completion,
                    }))
                    .await;
            }
            Err(error) => completion.complete_async(Err(error)).await,
        }
    })
    .detach();
}

pub(crate) fn prepare_lsp_call(
    editor: &Editor,
    request: &helix_plugin_api::LspCallRequest,
) -> ContractResult<(helix_runtime::Work, Arc<helix_lsp::Client>)> {
    let document_id = adapt::resolve_document(editor, request.document)?;
    let document = editor
        .document(document_id)
        .ok_or_else(|| ContractError::stale_handle(request.document.to_string()))?;
    let server = document
        .all_language_servers()
        .find(|server| {
            request
                .server
                .as_deref()
                .is_none_or(|name| server.name() == name)
        })
        .cloned()
        .ok_or_else(|| {
            ContractError::not_found(request.server.as_deref().unwrap_or("language server"))
        })?;
    Ok((editor.work(), server))
}

pub(crate) async fn execute_lsp_call(
    server: Arc<helix_lsp::Client>,
    request: helix_plugin_api::LspCallRequest,
    cancellation: helix_runtime::Token,
) -> Option<ContractResult<helix_plugin_api::PluginTaskResult>> {
    let params = match serde_json::Value::try_from(request.params) {
        Ok(params) => params,
        Err(error) => return Some(Err(ContractError::invalid_request(error))),
    };
    let call = server.call_custom(request.method, params);
    tokio::pin!(call);
    tokio::select! {
        _ = cancellation.canceled() => None,
        result = &mut call => Some(
            result
                .map_err(|error| ContractError::internal(error.to_string()))
                .and_then(|value| {
                    helix_plugin_api::DynamicValue::try_from(value)
                        .map(helix_plugin_api::PluginTaskResult::Value)
                        .map_err(ContractError::internal)
                })
        ),
    }
}

pub(crate) fn prepare_syntax_query(
    editor: &Editor,
    request: helix_plugin_api::SyntaxQueryRequest,
) -> ContractResult<(
    helix_runtime::Runtime,
    (
        helix_core::syntax::SyntaxQuerySnapshot,
        helix_plugin_api::SyntaxQueryRequest,
    ),
)> {
    let document = adapt::resolve_document(editor, request.document)?;
    let document = editor
        .document(document)
        .ok_or_else(|| ContractError::stale_handle(request.document.to_string()))?;
    let syntax = document
        .syntax()
        .ok_or_else(|| ContractError::unsupported("document has no syntax tree"))?;
    let loader = editor.syn_loader.load();
    let snapshot = syntax
        .query_snapshot(document.text(), &loader)
        .ok_or_else(|| ContractError::unsupported("document grammar is unavailable"))?;
    Ok((editor.runtime().clone(), (snapshot, request)))
}

pub(crate) fn execute_syntax_query(
    (snapshot, request): (
        helix_core::syntax::SyntaxQuerySnapshot,
        helix_plugin_api::SyntaxQueryRequest,
    ),
) -> ContractResult<helix_plugin_api::PluginTaskResult> {
    let text = snapshot.text();
    let position_to_byte = |position: helix_plugin_api::snapshots::Position| {
        if position.line >= text.len_lines() {
            return Err(ContractError::invalid_request(format!(
                "line {} is outside the document",
                position.line
            )));
        }
        let line_start = text.line_to_char(position.line);
        let line_len = text.line(position.line).len_chars();
        let char_index = line_start + position.column.min(line_len);
        Ok(text.char_to_byte(char_index) as u32)
    };
    let start = request
        .start
        .map(position_to_byte)
        .transpose()?
        .unwrap_or(0);
    let end = request
        .end
        .map(position_to_byte)
        .transpose()?
        .unwrap_or_else(|| text.len_bytes() as u32);
    if start > end {
        return Err(ContractError::invalid_request(
            "syntax query start must not follow end",
        ));
    }
    let max_captures = if request.max_captures == 0 {
        10_000
    } else {
        request.max_captures.min(100_000)
    };
    let captures = snapshot
        .execute(&request.query, start..end, max_captures)
        .map_err(|error| ContractError::invalid_request(error.to_string()))?
        .into_iter()
        .map(|capture| {
            let position = |byte: usize| {
                let char_index = text.byte_to_char(byte);
                let line = text.char_to_line(char_index);
                helix_plugin_api::snapshots::Position {
                    line,
                    column: char_index - text.line_to_char(line),
                }
            };
            helix_plugin_api::SyntaxCapture {
                name: capture.name,
                kind: capture.kind,
                start: position(capture.start_byte),
                end: position(capture.end_byte),
            }
        })
        .collect();
    Ok(helix_plugin_api::PluginTaskResult::SyntaxCaptures(captures))
}

pub struct TermUiHost {
    sender: crate::runtime::ForegroundEvents,
    next_callback_id: std::sync::atomic::AtomicU64,
    callback_route: PluginHostRoute,
}

impl TermUiHost {
    fn callback_route(&self, callback: UiCallbackToken) -> PluginUiCallback {
        PluginUiCallback::new(self.callback_route.clone(), callback)
    }
}

impl PluginUiHost for TermUiHost {
    fn notify(&mut self, req: NotifyRequest) -> ContractResult<()> {
        self.sender
            .ui(UiCommand::Plugin(PluginCommand::Notify {
                level: req.level,
                message: req.message,
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(())
    }

    fn prompt(
        &mut self,
        _plugin: PluginId,
        req: contract_requests::PromptRequest,
    ) -> ContractResult<UiCallbackToken> {
        let token = next_non_zero(&self.next_callback_id);
        let callback = UiCallbackToken::from_raw(token);
        self.sender
            .ui(UiCommand::Plugin(PluginCommand::Prompt {
                request: req,
                callback: self.callback_route(callback),
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(callback)
    }

    fn confirm(
        &mut self,
        _plugin: PluginId,
        req: contract_requests::ConfirmRequest,
    ) -> ContractResult<UiCallbackToken> {
        let token = next_non_zero(&self.next_callback_id);
        let callback = UiCallbackToken::from_raw(token);
        self.sender
            .ui(UiCommand::Plugin(PluginCommand::Confirm {
                request: req,
                callback: self.callback_route(callback),
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(callback)
    }

    fn picker(
        &mut self,
        _plugin: PluginId,
        req: contract_requests::PickerRequest,
    ) -> ContractResult<UiCallbackToken> {
        let token = next_non_zero(&self.next_callback_id);
        let callback = UiCallbackToken::from_raw(token);
        self.sender
            .ui(UiCommand::Plugin(PluginCommand::Picker {
                request: req,
                callback: self.callback_route(callback),
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(callback)
    }
}

pub struct TermPanelHost {
    sender: crate::runtime::ForegroundEvents,
    panel_owners: HashMap<PanelHandle, PluginId>,
    host_route: PluginHostRoute,
}

pub struct TermPanelService<'a> {
    host: &'a mut TermPanelHost,
    editor: &'a mut Editor,
}

impl PluginPanelHost for TermPanelService<'_> {
    fn register_panel(
        &mut self,
        plugin: PluginId,
        reg: contract_requests::PanelRegistration,
    ) -> ContractResult<PanelHandle> {
        use helix_plugin_api::requests::PanelSizeSpec;
        use helix_view::model::{PanelSide, PanelSize, PluginPanelModel};

        let contract_requests::PanelRegistration {
            title,
            side,
            size,
            hidden,
            content,
        } = reg;
        let panel_side = match side {
            contract_requests::PanelSide::Left => PanelSide::Left,
            contract_requests::PanelSide::Right => PanelSide::Right,
            contract_requests::PanelSide::Bottom => PanelSide::Bottom,
        };
        let panel_size = match size.unwrap_or(PanelSizeSpec::Fixed(30)) {
            PanelSizeSpec::Fixed(cells) => PanelSize::fixed(cells),
            PanelSizeSpec::Percent(percent) => PanelSize::Percent(percent),
        };
        let panel_id = self.editor.model.insert_panel(
            title,
            Box::new(PluginPanelModel),
            panel_side,
            panel_size,
        );
        if hidden {
            let _ = self.editor.model.toggle_panel(panel_id);
        }
        let panel = (adapt::panel_handle(panel_id), Arc::from(content));

        if let Err(error) = self
            .host
            .sender
            .ui(UiCommand::Plugin(PluginCommand::PushPanel {
                panel: panel.0,
                content: panel.1,
                key_events: Some(PluginPanelKeyRoute::new(
                    self.host.host_route.clone(),
                    panel.0,
                )),
            }))
        {
            self.editor.model.remove_panel(panel_id);
            return Err(internal_error(error.to_string()));
        }
        self.host.panel_owners.insert(panel.0, plugin);
        Ok(panel.0)
    }

    fn update_panel(&mut self, plugin: PluginId, req: PanelUpdateRequest) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        self.ensure_panel_exists(req.panel)?;
        self.host
            .sender
            .ui(UiCommand::Plugin(PluginCommand::UpdatePanel {
                panel: req.panel,
                title: req.title,
                content: req.content.map(Arc::from),
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(())
    }

    fn close_panel(&mut self, plugin: PluginId, req: PanelCloseRequest) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        let panel = req.panel;
        let panel_id = adapt::resolve_panel(&self.editor.model, req.panel)?;
        self.host
            .sender
            .ui(UiCommand::Plugin(PluginCommand::RemovePanel { panel }))
            .map_err(|error| internal_error(error.to_string()))?;
        if !self.editor.model.remove_panel(panel_id) {
            return Err(ContractError::stale_handle(req.panel.to_string()));
        }
        self.host.panel_owners.remove(&panel);
        Ok(())
    }

    fn toggle_panel(&mut self, plugin: PluginId, req: TogglePanelRequest) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        self.ensure_panel_exists(req.panel)?;
        self.host
            .sender
            .ui(UiCommand::Plugin(PluginCommand::TogglePanel {
                panel: req.panel,
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(())
    }

    fn focus_panel(
        &mut self,
        plugin: PluginId,
        req: contract_requests::FocusPanelRequest,
    ) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        self.ensure_panel_exists(req.panel)?;
        self.host
            .sender
            .ui(UiCommand::Plugin(PluginCommand::FocusPanel {
                panel: req.panel,
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(())
    }

    fn resize_panel(&mut self, plugin: PluginId, req: ResizePanelRequest) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        self.ensure_panel_exists(req.panel)?;
        self.host
            .sender
            .ui(UiCommand::Plugin(PluginCommand::ResizePanel {
                panel: req.panel,
                size: req.size,
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(())
    }

    fn list_panels(&self) -> Vec<helix_plugin_api::snapshots::PanelSnapshot> {
        self.editor
            .model
            .panels
            .iter()
            .map(
                |(panel_id, panel)| helix_plugin_api::snapshots::PanelSnapshot {
                    handle: adapt::panel_handle(panel_id),
                    title: panel.title.clone(),
                    side: adapt::panel_side_to_contract(panel.side),
                    visible: panel.visible,
                    is_focused: self.editor.model.focus == FocusTarget::Panel(panel_id),
                },
            )
            .collect()
    }
}

impl TermPanelService<'_> {
    fn ensure_panel_owner(&self, plugin: PluginId, panel: PanelHandle) -> ContractResult<()> {
        match self.host.panel_owners.get(&panel) {
            Some(owner) if *owner == plugin => Ok(()),
            Some(_) => Err(permission_denied(plugin, panel)),
            None => Err(ContractError::stale_handle(panel.to_string())),
        }
    }

    fn ensure_panel_exists(&self, panel: PanelHandle) -> ContractResult<()> {
        adapt::resolve_panel(&self.editor.model, panel)?;
        Ok(())
    }
}

impl TermPanelHost {
    pub(crate) fn service<'a>(&'a mut self, editor: &'a mut Editor) -> TermPanelService<'a> {
        TermPanelService { host: self, editor }
    }
}

#[derive(Clone)]
enum PluginUiSender {
    Foreground(crate::runtime::ForegroundEvents),
    Runtime(crate::runtime::RuntimeIngress),
}

impl PluginUiSender {
    fn ui(&self, command: PluginCommand) -> ContractResult<()> {
        match self {
            Self::Foreground(sender) => sender.ui(UiCommand::Plugin(command)).map_err(|error| {
                internal_error(format!("foreground plugin command rejected: {error}"))
            }),
            Self::Runtime(sender) => sender
                .ui(UiCommand::Plugin(command))
                .map_err(|error| internal_error(format!("plugin cleanup rejected: {error}"))),
        }
    }
}

pub struct TermResourceHost<'a> {
    sender: PluginUiSender,
    panel_owners: &'a mut HashMap<PanelHandle, PluginId>,
}

impl PluginResourceHost for TermResourceHost<'_> {
    fn release_plugin_resources(&mut self, plugin: PluginId) -> ContractResult<()> {
        let panels = self
            .panel_owners
            .iter()
            .filter_map(|(panel, owner)| (*owner == plugin).then_some(*panel))
            .collect::<Vec<_>>();
        self.sender.ui(PluginCommand::ReleaseResources {
            plugin,
            panels: panels.clone(),
        })?;
        for panel in &panels {
            self.panel_owners.remove(panel);
        }
        Ok(())
    }
}

struct RegisteredCommandDefinition {
    plugin: PluginId,
    definition: CommandDefinition,
}

impl RegisteredCommandDefinition {
    fn descriptor(&self) -> helix_plugin_api::CommandDescriptor {
        helix_plugin_api::CommandDescriptor {
            name: self.definition.name.clone(),
            aliases: Vec::new(),
            doc: self.definition.doc.clone().unwrap_or_default(),
            arguments: self.definition.args.clone().unwrap_or_default(),
            signature: None,
            kind: helix_plugin_api::CommandKind::Plugin,
            scope: helix_plugin_api::CommandScope::Frontend,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PluginCommandId {
    pub(crate) host: PluginHostId,
    generation: HostGenerationId,
    pub(crate) command: CommandHandle,
}

#[derive(Debug, Clone)]
pub(crate) struct PluginCommandSnapshot {
    pub(crate) id: PluginCommandId,
    pub(crate) descriptor: helix_plugin_api::CommandDescriptor,
}

type PublishedPluginCommands =
    arc_swap::ArcSwap<Vec<(CommandHandle, helix_plugin_api::CommandDescriptor)>>;

pub struct TermCommandHost {
    next_command_handle: std::sync::atomic::AtomicU64,
    commands: HashMap<CommandHandle, RegisteredCommandDefinition>,
    published: Arc<PublishedPluginCommands>,
}

impl PluginCommandHost for TermCommandHost {
    fn command_catalog(&self) -> Vec<helix_plugin_api::CommandDescriptor> {
        use helix_plugin_api::{
            CommandDescriptor, CommandFlagDescriptor, CommandKind,
            CommandScope as ContractCommandScope, CommandSignatureDescriptor,
        };

        let command_scope = |scope| match scope {
            helix_modal::registry::CommandScope::Viewport => ContractCommandScope::Viewport,
            helix_modal::registry::CommandScope::Tree => ContractCommandScope::Tree,
            helix_modal::registry::CommandScope::Frontend => ContractCommandScope::Frontend,
        };

        let mut catalog = crate::commands::typed::TYPABLE_COMMAND_LIST
            .iter()
            .map(|command| CommandDescriptor {
                name: command.name.into(),
                aliases: command
                    .aliases
                    .iter()
                    .map(|alias| (*alias).into())
                    .collect(),
                doc: command.doc.into(),
                arguments: Vec::new(),
                signature: Some(CommandSignatureDescriptor {
                    min_positionals: command.signature.positionals.0,
                    max_positionals: command.signature.positionals.1,
                    raw_after: command.signature.raw_after,
                    flags: command
                        .signature
                        .flags
                        .iter()
                        .map(|flag| CommandFlagDescriptor {
                            name: flag.name.into(),
                            alias: flag.alias,
                            doc: flag.doc.into(),
                            takes_value: flag.completions.is_some(),
                            values: flag
                                .completions
                                .unwrap_or_default()
                                .iter()
                                .map(|value| (*value).into())
                                .collect(),
                        })
                        .collect(),
                }),
                kind: CommandKind::Typable,
                scope: ContractCommandScope::Frontend,
            })
            .collect::<Vec<_>>();

        catalog.extend(
            crate::commands::MappableCommand::builtin_commands()
                .iter()
                .map(|command| CommandDescriptor {
                    name: command.name().into(),
                    aliases: Vec::new(),
                    doc: command.doc().into(),
                    arguments: Vec::new(),
                    signature: None,
                    kind: CommandKind::Static,
                    scope: command_scope(command.scope()),
                }),
        );

        catalog.extend(
            self.published
                .load()
                .iter()
                .map(|(_, descriptor)| descriptor.clone()),
        );
        catalog.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        catalog
    }

    fn register_command(
        &mut self,
        plugin: PluginId,
        def: CommandDefinition,
    ) -> ContractResult<helix_plugin_api::CommandHandle> {
        if self.command_name_in_use(&def.name, None) {
            return Err(ContractError::invalid_request(format!(
                "command already registered: {}",
                def.name
            )));
        }

        let handle =
            helix_plugin_api::CommandHandle::from_raw(next_non_zero(&self.next_command_handle));
        self.commands.insert(
            handle,
            RegisteredCommandDefinition {
                plugin,
                definition: def,
            },
        );
        self.publish();
        Ok(handle)
    }

    fn update_command(
        &mut self,
        plugin: PluginId,
        req: CommandUpdateRequest,
    ) -> ContractResult<()> {
        self.ensure_command_owner(plugin, req.command)?;
        if let Some(name) = req.name.as_deref() {
            if self.command_name_in_use(name, Some(req.command)) {
                return Err(ContractError::invalid_request(format!(
                    "command already registered: {name}"
                )));
            }
        }

        let command = self
            .commands
            .get_mut(&req.command)
            .ok_or_else(|| ContractError::stale_handle(req.command.to_string()))?;

        if let Some(name) = req.name {
            command.definition.name = name;
        }
        if let Some(doc) = req.doc {
            command.definition.doc = (!doc.is_empty()).then_some(doc);
        }
        if let Some(args) = req.args {
            command.definition.args = (!args.is_empty()).then_some(args);
        }
        self.publish();
        Ok(())
    }

    fn remove_command(
        &mut self,
        plugin: PluginId,
        req: CommandRemoveRequest,
    ) -> ContractResult<()> {
        self.ensure_command_owner(plugin, req.command)?;
        self.commands
            .remove(&req.command)
            .ok_or_else(|| ContractError::stale_handle(req.command.to_string()))?;
        self.publish();
        Ok(())
    }
}

impl TermCommandHost {
    fn publish(&self) {
        let commands = self
            .commands
            .iter()
            .map(|(handle, command)| (*handle, command.descriptor()))
            .collect();
        self.published.store(Arc::new(commands));
    }

    fn release_plugin(&mut self, plugin: PluginId) {
        self.commands.retain(|_, command| command.plugin != plugin);
        self.publish();
    }

    fn ensure_command_owner(&self, plugin: PluginId, command: CommandHandle) -> ContractResult<()> {
        match self.commands.get(&command) {
            Some(registered) if registered.plugin == plugin => Ok(()),
            Some(_) => Err(permission_denied(plugin, command)),
            None => Err(ContractError::stale_handle(command.to_string())),
        }
    }

    fn command_name_in_use(&self, name: &str, except: Option<CommandHandle>) -> bool {
        if crate::commands::typed::TYPABLE_COMMAND_LIST
            .iter()
            .any(|command| command.name == name || command.aliases.contains(&name))
        {
            return true;
        }

        if crate::commands::MappableCommand::named(name).is_some() {
            return true;
        }

        self.commands
            .iter()
            .any(|(handle, command)| Some(*handle) != except && command.definition.name == name)
    }
}

pub struct TermKeymapHost {
    foreground: crate::runtime::ForegroundEvents,
    next_keymap_handle: std::sync::atomic::AtomicU64,
    owners: HashMap<KeymapHandle, PluginId>,
}

impl PluginKeymapHost for TermKeymapHost {
    fn register_keymap(
        &mut self,
        plugin: PluginId,
        definition: helix_plugin_api::KeymapDefinition,
    ) -> ContractResult<KeymapHandle> {
        let contribution = crate::keymap::compile_plugin_keymap(&definition)
            .map_err(|error| ContractError::invalid_request(error.to_string()))?;
        let keymap = KeymapHandle::from_raw(next_non_zero(&self.next_keymap_handle));
        self.foreground
            .ui(UiCommand::Plugin(PluginCommand::SetKeymap {
                keymap,
                contribution,
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        self.owners.insert(keymap, plugin);
        Ok(keymap)
    }

    fn update_keymap(
        &mut self,
        plugin: PluginId,
        request: KeymapUpdateRequest,
    ) -> ContractResult<()> {
        self.ensure_owner(plugin, request.keymap)?;
        let contribution = crate::keymap::compile_plugin_keymap(&request.definition)
            .map_err(|error| ContractError::invalid_request(error.to_string()))?;
        self.foreground
            .ui(UiCommand::Plugin(PluginCommand::SetKeymap {
                keymap: request.keymap,
                contribution,
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(())
    }

    fn remove_keymap(
        &mut self,
        plugin: PluginId,
        request: KeymapRemoveRequest,
    ) -> ContractResult<()> {
        self.ensure_owner(plugin, request.keymap)?;
        self.foreground
            .ui(UiCommand::Plugin(PluginCommand::RemoveKeymap {
                keymap: request.keymap,
            }))
            .map_err(|error| internal_error(error.to_string()))?;
        self.owners.remove(&request.keymap);
        Ok(())
    }
}

impl TermKeymapHost {
    fn release_plugin(&mut self, plugin: PluginId, sender: &PluginUiSender) {
        let keymaps = self
            .owners
            .iter()
            .filter_map(|(keymap, owner)| (*owner == plugin).then_some(*keymap))
            .collect::<Vec<_>>();
        for keymap in keymaps {
            match sender.ui(PluginCommand::RemoveKeymap { keymap }) {
                Ok(()) => {
                    self.owners.remove(&keymap);
                }
                Err(error) => {
                    log::error!("failed to release plugin keymap {keymap}: {error}");
                }
            }
        }
    }
}

impl TermKeymapHost {
    fn ensure_owner(&self, plugin: PluginId, keymap: KeymapHandle) -> ContractResult<()> {
        match self.owners.get(&keymap) {
            Some(owner) if *owner == plugin => Ok(()),
            Some(_) => Err(permission_denied(plugin, keymap)),
            None => Err(ContractError::stale_handle(keymap.to_string())),
        }
    }
}

pub struct TermEventHost {
    next_subscription_handle: std::sync::atomic::AtomicU64,
    subscriptions: HashMap<SubscriptionHandle, PluginId>,
}

impl PluginEventHost for TermEventHost {
    fn subscribe(
        &mut self,
        plugin: PluginId,
        kind: helix_plugin_api::events::EventKind,
    ) -> ContractResult<helix_plugin_api::SubscriptionHandle> {
        if !kind.is_supported() {
            return Err(ContractError::unsupported(format!(
                "plugin event '{kind}' has no host emitter"
            )));
        }
        let handle = helix_plugin_api::SubscriptionHandle::from_raw(next_non_zero(
            &self.next_subscription_handle,
        ));
        self.subscriptions.insert(handle, plugin);
        Ok(handle)
    }

    fn unsubscribe(
        &mut self,
        plugin: PluginId,
        handle: helix_plugin_api::SubscriptionHandle,
    ) -> ContractResult<()> {
        match self.subscriptions.get(&handle) {
            Some(owner) if *owner == plugin => {
                self.subscriptions.remove(&handle);
                Ok(())
            }
            Some(_) => Err(permission_denied(plugin, handle)),
            None => Err(ContractError::stale_handle(handle.to_string())),
        }
    }

    fn event_catalog(&self) -> Vec<EventKindInfo> {
        ApiMetadata::default().event_catalog
    }
}

impl TermEventHost {
    fn release_plugin(&mut self, plugin: PluginId) {
        self.subscriptions.retain(|_, owner| *owner != plugin);
    }
}

pub(crate) struct PluginHostStateInner {
    pub(crate) ui: TermUiHost,
    pub(crate) panel: TermPanelHost,
    pub(crate) command: TermCommandHost,
    pub(crate) keymap: TermKeymapHost,
    pub(crate) event: TermEventHost,
    operations: HashMap<PluginOperationToken, HostOperation>,
    plugins: HashSet<PluginId>,
}

struct HostOperation {
    plugin: PluginId,
    cancellation: helix_runtime::Token,
    completed: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone)]
pub struct PluginHostState {
    route: PluginHostRoute,
    inner: Arc<Mutex<PluginHostStateInner>>,
    cleanup_ingress: crate::runtime::RuntimeIngress,
    published_commands: Arc<PublishedPluginCommands>,
    generation: Option<HostGenerationId>,
}

impl std::fmt::Debug for PluginHostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHostState")
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

impl PluginHostState {
    fn new(
        id: PluginHostId,
        name: String,
        cleanup_ingress: crate::runtime::RuntimeIngress,
        foreground: crate::runtime::ForegroundEvents,
        outbound: helix_runtime::Sender<HostOutbound>,
        work: helix_runtime::Work,
    ) -> Self {
        let route = PluginHostRoute {
            id,
            name: Arc::from(name),
            outbound,
            work,
            active_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let published_commands = Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new()));
        Self {
            inner: Arc::new(Mutex::new(PluginHostStateInner {
                ui: TermUiHost {
                    sender: foreground.clone(),
                    next_callback_id: std::sync::atomic::AtomicU64::new(1),
                    callback_route: route.clone(),
                },
                panel: TermPanelHost {
                    sender: foreground.clone(),
                    panel_owners: HashMap::new(),
                    host_route: route.clone(),
                },
                command: TermCommandHost {
                    next_command_handle: std::sync::atomic::AtomicU64::new(1),
                    commands: HashMap::new(),
                    published: Arc::clone(&published_commands),
                },
                keymap: TermKeymapHost {
                    foreground,
                    next_keymap_handle: std::sync::atomic::AtomicU64::new(1),
                    owners: HashMap::new(),
                },
                event: TermEventHost {
                    next_subscription_handle: std::sync::atomic::AtomicU64::new(1),
                    subscriptions: HashMap::new(),
                },
                operations: HashMap::new(),
                plugins: HashSet::new(),
            })),
            route,
            cleanup_ingress,
            published_commands,
            generation: None,
        }
    }

    pub(crate) fn id(&self) -> PluginHostId {
        self.route.id
    }

    fn lock_for_worker(&self) -> MutexGuard<'_, PluginHostStateInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn try_lock(&self) -> ContractResult<MutexGuard<'_, PluginHostStateInner>> {
        match self.inner.try_lock() {
            Ok(state) => Ok(state),
            Err(std::sync::TryLockError::WouldBlock) => Err(ContractError::Busy {
                reason: format!("plugin host '{}' state is busy", self.route.name),
            }),
            Err(std::sync::TryLockError::Poisoned(error)) => Ok(error.into_inner()),
        }
    }

    #[cfg(test)]
    fn lock(&self) -> MutexGuard<'_, PluginHostStateInner> {
        self.lock_for_worker()
    }

    fn begin_generation(&self) -> HostGenerationId {
        self.release_all_resources();
        let generation = HostGenerationId::next();
        self.route.activate(generation);
        generation
    }

    fn for_generation(&self, generation: HostGenerationId) -> Self {
        Self {
            route: self.route.clone(),
            inner: Arc::clone(&self.inner),
            cleanup_ingress: self.cleanup_ingress.clone(),
            published_commands: Arc::clone(&self.published_commands),
            generation: Some(generation),
        }
    }

    fn end_generation(&self, generation: HostGenerationId) {
        self.route.deactivate(generation);
        self.release_all_resources();
    }

    pub(crate) fn ensure_active(&self) -> ContractResult<()> {
        match self.generation {
            Some(generation) if !self.route.is_active(generation) => {
                Err(ContractError::stale_handle(format!(
                    "{} generation {}",
                    self.route.id, generation.0
                )))
            }
            _ => Ok(()),
        }
    }

    fn command_snapshot(&self, generation: HostGenerationId) -> Vec<PluginCommandSnapshot> {
        self.published_commands
            .load()
            .iter()
            .map(|(command, descriptor)| PluginCommandSnapshot {
                id: PluginCommandId {
                    host: self.id(),
                    generation,
                    command: *command,
                },
                descriptor: descriptor.clone(),
            })
            .collect()
    }

    fn has_command(&self, command: CommandHandle) -> bool {
        self.published_commands
            .load()
            .iter()
            .any(|(published, _)| *published == command)
    }

    pub(crate) fn track_plugin(&self, plugin: PluginId) -> ContractResult<()> {
        self.try_lock()?.plugins.insert(plugin);
        Ok(())
    }

    pub(crate) fn begin_task(
        &self,
        plugin: PluginId,
        operation: PluginOperationToken,
    ) -> ContractResult<(helix_runtime::Token, HostTaskResponder)> {
        self.ensure_active()?;
        let generation = self
            .generation
            .ok_or_else(|| internal_error("plugin host request has no active generation"))?;
        let cancellation = helix_runtime::Token::new();
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut state = self.try_lock()?;
        state.operations.retain(|_, operation| {
            !operation
                .completed
                .load(std::sync::atomic::Ordering::Acquire)
        });
        state.plugins.insert(plugin);
        if state.operations.len() >= 64 {
            return Err(ContractError::Busy {
                reason: "plugin operation capacity reached".to_owned(),
            });
        }
        if state.operations.contains_key(&operation) {
            return Err(ContractError::invalid_request(format!(
                "operation {} is already active",
                operation.raw()
            )));
        }
        state.operations.insert(
            operation,
            HostOperation {
                plugin,
                cancellation: cancellation.clone(),
                completed: Arc::clone(&completed),
            },
        );
        Ok((
            cancellation.clone(),
            HostTaskResponder {
                route: self.route.clone(),
                generation,
                operation,
                cancellation,
                completed,
            },
        ))
    }

    pub(crate) fn cancel_task(
        &self,
        plugin: PluginId,
        operation: PluginOperationToken,
    ) -> ContractResult<()> {
        let mut state = self.try_lock()?;
        let active = state
            .operations
            .get(&operation)
            .ok_or_else(|| ContractError::stale_handle(operation.to_string()))?;
        if active.completed.load(std::sync::atomic::Ordering::Acquire) {
            state.operations.remove(&operation);
            return Err(ContractError::stale_handle(operation.to_string()));
        }
        if active.plugin != plugin {
            return Err(permission_denied(plugin, operation));
        }
        let Some(active) = state.operations.remove(&operation) else {
            return Err(ContractError::stale_handle(operation.to_string()));
        };
        active
            .completed
            .store(true, std::sync::atomic::Ordering::Release);
        active.cancellation.cancel();
        Ok(())
    }

    pub(crate) fn release_plugin_resources(&self, plugin: PluginId) -> ContractResult<()> {
        let mut state = self.try_lock()?;
        let sender = PluginUiSender::Foreground(state.ui.sender.clone());
        let mut resources = TermResourceHost {
            sender: sender.clone(),
            panel_owners: &mut state.panel.panel_owners,
        };
        resources.release_plugin_resources(plugin)?;
        state.command.release_plugin(plugin);
        state.keymap.release_plugin(plugin, &sender);
        state.event.release_plugin(plugin);
        state.plugins.remove(&plugin);
        Ok(())
    }

    pub(crate) fn release_all_resources(&self) {
        let mut state = self.lock_for_worker();
        let sender = PluginUiSender::Runtime(self.cleanup_ingress.clone());
        let plugins = state.plugins.drain().collect::<Vec<_>>();
        for operation in state.operations.drain().map(|(_, operation)| operation) {
            operation
                .completed
                .store(true, std::sync::atomic::Ordering::Release);
            operation.cancellation.cancel();
        }
        for plugin in plugins {
            let mut resources = TermResourceHost {
                sender: sender.clone(),
                panel_owners: &mut state.panel.panel_owners,
            };
            if let Err(error) = resources.release_plugin_resources(plugin) {
                log::error!("failed to release resources for plugin {plugin}: {error}");
            }
            state.command.release_plugin(plugin);
            state.keymap.release_plugin(plugin, &sender);
            state.event.release_plugin(plugin);
        }
    }
}

#[derive(Clone)]
pub struct HostTaskResponder {
    route: PluginHostRoute,
    generation: HostGenerationId,
    operation: PluginOperationToken,
    cancellation: helix_runtime::Token,
    completed: Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for HostTaskResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostTaskResponder")
            .field("operation", &self.operation)
            .finish()
    }
}

impl HostTaskResponder {
    pub(crate) fn send(&self, result: ContractResult<helix_plugin_api::PluginTaskResult>) {
        if self
            .completed
            .swap(true, std::sync::atomic::Ordering::AcqRel)
            || self.cancellation.is_canceled()
        {
            return;
        }
        self.route.notify_on_work(
            self.generation,
            HostRequest::TaskCompleted {
                operation: self.operation,
                result,
            },
            "task completion",
        );
    }
}

#[derive(Clone)]
enum HostOutbound {
    Notify {
        generation: Option<HostGenerationId>,
        request: HostRequest,
    },
    Response {
        id: u64,
        result: ContractResult<HostResponse>,
    },
}

#[derive(Clone)]
pub struct PluginHostResponder(
    Arc<Mutex<Option<tokio::sync::oneshot::Sender<ContractResult<HostResponse>>>>>,
);

impl std::fmt::Debug for PluginHostResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PluginHostResponder").finish()
    }
}

impl PluginHostResponder {
    pub(crate) fn new(sender: tokio::sync::oneshot::Sender<ContractResult<HostResponse>>) -> Self {
        Self(Arc::new(Mutex::new(Some(sender))))
    }

    pub(crate) fn send(&self, result: ContractResult<HostResponse>) {
        let mut response = match self.0.try_lock() {
            Ok(response) => response,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        let Some(sender) = response.take() else {
            return;
        };
        let _ = sender.send(result);
    }
}

#[derive(Clone)]
pub struct PluginRuntime {
    hosts: Arc<RwLock<Vec<SupervisedPluginHost>>>,
    config: Arc<RwLock<PluginConfig>>,
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self {
            hosts: Arc::default(),
            config: Arc::new(RwLock::new(PluginConfig::default())),
        }
    }
}

#[derive(Clone)]
struct SupervisedPluginHost {
    name: String,
    state: PluginHostState,
    control: helix_runtime::Sender<HostOutbound>,
    events: helix_runtime::Sender<helix_plugin_api::events::PluginEvent>,
    dropped_events: Arc<std::sync::atomic::AtomicU64>,
    shutdown: helix_runtime::Token,
    stopped: tokio::sync::watch::Receiver<bool>,
}

impl PluginRuntime {
    fn host_snapshot(&self) -> Vec<SupervisedPluginHost> {
        self.hosts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn command_snapshot(&self) -> Vec<PluginCommandSnapshot> {
        let hosts = self.host_snapshot();
        let mut commands = hosts
            .iter()
            .flat_map(|host| {
                let Some(generation) = host.state.route.active_generation() else {
                    return Vec::new();
                };
                host.state.command_snapshot(generation)
            })
            .collect::<Vec<_>>();
        commands.sort_unstable_by(|left, right| {
            left.descriptor
                .name
                .cmp(&right.descriptor.name)
                .then_with(|| left.id.host.cmp(&right.id.host))
                .then_with(|| {
                    left.id
                        .command
                        .raw()
                        .get()
                        .cmp(&right.id.command.raw().get())
                })
        });
        commands
    }

    pub(crate) fn invoke_command(
        &self,
        id: PluginCommandId,
        args: Vec<String>,
    ) -> ContractResult<()> {
        let hosts = self.host_snapshot();
        let host = hosts
            .iter()
            .find(|host| host.state.id() == id.host)
            .ok_or_else(|| ContractError::stale_handle(id.host.to_string()))?;
        if !host.state.has_command(id.command) {
            return Err(ContractError::stale_handle(id.command.to_string()));
        }
        host.state.route.try_notify_for_generation(
            id.generation,
            HostRequest::CommandInvoke {
                command: id.command,
                args,
            },
        )
    }

    pub(crate) fn reload(&self) -> ContractResult<()> {
        for host in self.host_snapshot() {
            if host.shutdown.is_canceled() {
                return Err(internal_error(format!(
                    "plugin host '{}' is shutting down",
                    host.name
                )));
            }
            host.state.route.try_notify(HostRequest::Reload)?;
        }
        Ok(())
    }

    pub(crate) fn notify_event(&self, event: helix_plugin_api::events::PluginEvent) {
        for host in self.host_snapshot() {
            if let Err(error) = host.events.try_send(event.clone()) {
                let dropped = host
                    .dropped_events
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                if dropped.is_power_of_two() {
                    log::warn!(
                        "plugin host '{}' event lane saturated; dropped={} error={error}",
                        host.name,
                        dropped,
                    );
                }
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        let hosts = self.host_snapshot();
        request_host_shutdown(&hosts);
        wait_for_hosts(hosts).await;
    }

    pub(crate) fn reconfigure(
        &self,
        config: &PluginConfig,
        ingress: crate::runtime::RuntimeIngress,
        foreground: crate::runtime::ForegroundEvents,
        work: helix_runtime::Work,
    ) -> Result<bool, helix_plugin::PluginConfigError> {
        config.validate()?;
        let mut active_config = self
            .config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *active_config == *config {
            return Ok(false);
        }

        let next = spawn_plugin_hosts(config, ingress, foreground, work.clone());
        let previous = std::mem::replace(
            &mut *self
                .hosts
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            next,
        );
        *active_config = config.clone();
        drop(active_config);
        request_host_shutdown(&previous);
        work.spawn(wait_for_hosts(previous)).detach();
        Ok(true)
    }
}

fn request_host_shutdown(hosts: &[SupervisedPluginHost]) {
    for host in hosts {
        host.shutdown.cancel();
        let _ = host.control.try_send(HostOutbound::Notify {
            generation: None,
            request: HostRequest::Shutdown,
        });
    }
}

async fn wait_for_hosts(hosts: Vec<SupervisedPluginHost>) {
    for host in hosts {
        let mut stopped = host.stopped;
        while !*stopped.borrow() {
            if stopped.changed().await.is_err() {
                break;
            }
        }
    }
}

fn host_config(base: &PluginConfig, host: &PluginHostConfig) -> PluginConfig {
    let mut config = base.clone();
    if !host.plugin_dirs.is_empty() {
        config.plugin_dirs = host.plugin_dirs.clone();
    }
    config.hosts.clear();
    config
}

async fn write_plugin_host(
    name: String,
    mut stdin: tokio::process::ChildStdin,
    mut control: helix_runtime::Receiver<HostOutbound>,
    mut events: helix_runtime::Receiver<helix_plugin_api::events::PluginEvent>,
) {
    const MAX_CONTROL_BURST: usize = 8;

    let mut codec = FrameCodec::new();
    let mut control_streak = 0;
    loop {
        let outbound = if control_streak >= MAX_CONTROL_BURST {
            match events.try_recv() {
                Ok(event) => {
                    control_streak = 0;
                    Some(HostOutbound::Notify {
                        generation: None,
                        request: HostRequest::Event(event),
                    })
                }
                Err(_) => control.recv().await,
            }
        } else {
            match control.try_recv() {
                Ok(outbound) => {
                    control_streak += 1;
                    Some(outbound)
                }
                Err(helix_runtime::TryRecvError::Empty) => match events.try_recv() {
                    Ok(event) => {
                        control_streak = 0;
                        Some(HostOutbound::Notify {
                            generation: None,
                            request: HostRequest::Event(event),
                        })
                    }
                    Err(helix_runtime::TryRecvError::Empty) => {
                        tokio::select! {
                            outbound = control.recv() => {
                                if outbound.is_some() {
                                    control_streak += 1;
                                }
                                outbound
                            }
                            event = events.recv() => {
                                control_streak = 0;
                                event.map(|event| HostOutbound::Notify {
                                    generation: None,
                                    request: HostRequest::Event(event),
                                })
                            }
                        }
                    }
                    Err(helix_runtime::TryRecvError::Closed) => control.recv().await,
                },
                Err(helix_runtime::TryRecvError::Closed) => {
                    events.recv().await.map(|event| HostOutbound::Notify {
                        generation: None,
                        request: HostRequest::Event(event),
                    })
                }
            }
        };
        let Some(outbound) = outbound else {
            break;
        };
        let result = match outbound {
            HostOutbound::Notify { request: body, .. } => {
                codec
                    .write::<_, _>(
                        &mut stdin,
                        &Frame::<HostRequest, HostResponse>::Notify { body },
                    )
                    .await
            }
            HostOutbound::Response { id, result } => {
                codec
                    .write::<_, _>(
                        &mut stdin,
                        &Frame::<HostRequest, HostResponse>::Response { id, result },
                    )
                    .await
            }
        };

        if let Err(err) = result {
            log::error!("plugin host '{name}' write failed: {err}");
            break;
        }
    }
}

async fn read_plugin_host(
    name: String,
    mut stdout: tokio::process::ChildStdout,
    ingress: crate::runtime::RuntimeIngress,
    state: PluginHostState,
    tx: helix_runtime::Sender<HostOutbound>,
    shutdown: helix_runtime::Token,
) {
    let mut codec = FrameCodec::new();
    loop {
        let frame = match codec
            .read::<Frame<PluginRequest, HostResponse>, _>(&mut stdout)
            .await
        {
            Ok(frame) => frame,
            Err(err) => {
                if !shutdown.is_canceled() {
                    log::error!("plugin host '{name}' read failed: {err}");
                }
                break;
            }
        };

        match frame {
            Frame::Request { id, body } => {
                let (respond_to, response) = tokio::sync::oneshot::channel();
                if ingress
                    .send_task(crate::runtime::RuntimeTaskEvent::PluginHostRequest {
                        state: state.clone(),
                        request: body,
                        respond_to: PluginHostResponder::new(respond_to),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                let result = response
                    .await
                    .unwrap_or_else(|_| Err(internal_error("plugin host request canceled")));
                if tx
                    .send(HostOutbound::Response { id, result })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Frame::Notify { body } => {
                let (respond_to, response) = tokio::sync::oneshot::channel();
                if ingress
                    .send_task(crate::runtime::RuntimeTaskEvent::PluginHostRequest {
                        state: state.clone(),
                        request: body,
                        respond_to: PluginHostResponder::new(respond_to),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                let _ = response.await;
            }
            Frame::Response { .. } => {
                log::warn!("plugin host '{name}' sent an unexpected response frame");
            }
        }
    }
}

async fn log_plugin_stderr(name: String, stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncBufReadExt;

    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        log::warn!("plugin host '{name}' stderr: {line}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostGenerationExit {
    Restart { uptime: std::time::Duration },
    Shutdown,
}

struct RestartBackoff {
    next: std::time::Duration,
}

impl Default for RestartBackoff {
    fn default() -> Self {
        Self {
            next: std::time::Duration::from_millis(100),
        }
    }
}

impl RestartBackoff {
    const MAX: std::time::Duration = std::time::Duration::from_secs(5);
    const RESET_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

    fn after_generation(&mut self, uptime: std::time::Duration) -> std::time::Duration {
        if uptime >= Self::RESET_AFTER {
            self.next = Self::default().next;
        }
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(Self::MAX);
        delay
    }
}

async fn stop_plugin_child(child: &mut tokio::process::Child, already_exited: bool, name: &str) {
    if already_exited {
        return;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::debug!("plugin host '{name}' wait failed: {error}"),
        Err(_) => {
            if let Err(error) = child.kill().await {
                log::debug!("plugin host '{name}' kill failed: {error}");
            }
            let _ = child.wait().await;
        }
    }
}

async fn run_host_generation(
    config: &PluginConfig,
    host: &PluginHostConfig,
    ingress: &crate::runtime::RuntimeIngress,
    state: &PluginHostState,
    work: &helix_runtime::Work,
    control: &mut helix_runtime::Receiver<HostOutbound>,
    events: &mut helix_runtime::Receiver<helix_plugin_api::events::PluginEvent>,
    shutdown: &helix_runtime::Token,
) -> HostGenerationExit {
    const GENERATION_CONTROL_CAPACITY: usize = 64;
    const GENERATION_EVENT_CAPACITY: usize = 256;

    let started = std::time::Instant::now();
    let mut command = tokio::process::Command::new(&host.command);
    command
        .args(&host.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            log::error!("failed to spawn plugin host '{}': {error}", host.name);
            return HostGenerationExit::Restart {
                uptime: started.elapsed(),
            };
        }
    };
    let Some(stdin) = child.stdin.take() else {
        log::error!("plugin host '{}' has no stdin pipe", host.name);
        stop_plugin_child(&mut child, false, &host.name).await;
        return HostGenerationExit::Restart {
            uptime: started.elapsed(),
        };
    };
    let Some(stdout) = child.stdout.take() else {
        log::error!("plugin host '{}' has no stdout pipe", host.name);
        stop_plugin_child(&mut child, false, &host.name).await;
        return HostGenerationExit::Restart {
            uptime: started.elapsed(),
        };
    };
    if let Some(stderr) = child.stderr.take() {
        work.spawn(log_plugin_stderr(host.name.clone(), stderr))
            .detach();
    }

    let (generation_control, generation_control_rx) =
        helix_runtime::channel(GENERATION_CONTROL_CAPACITY);
    let (generation_events, generation_events_rx) =
        helix_runtime::channel(GENERATION_EVENT_CAPACITY);
    let generation = state.begin_generation();
    let mut writer = work.spawn(write_plugin_host(
        host.name.clone(),
        stdin,
        generation_control_rx,
        generation_events_rx,
    ));
    let mut reader = work.spawn(read_plugin_host(
        host.name.clone(),
        stdout,
        ingress.clone(),
        state.for_generation(generation),
        generation_control.clone(),
        shutdown.clone(),
    ));

    let init = HostRequest::Init {
        metadata: ApiMetadata::default(),
        config: config.clone(),
    };
    if generation_control
        .send(HostOutbound::Notify {
            generation: None,
            request: init,
        })
        .await
        .is_err()
    {
        stop_plugin_child(&mut child, false, &host.name).await;
        state.end_generation(generation);
        return HostGenerationExit::Restart {
            uptime: started.elapsed(),
        };
    }

    let mut child_exited = false;
    let mut events_open = true;
    let outcome = loop {
        tokio::select! {
            _ = shutdown.canceled() => {
                let _ = generation_control
                    .try_send(HostOutbound::Notify {
                        generation: None,
                        request: HostRequest::Shutdown,
                    });
                break HostGenerationExit::Shutdown;
            }
            result = &mut reader => {
                if let Err(error) = result {
                    log::debug!("plugin host '{}' reader task failed: {error}", host.name);
                }
                break HostGenerationExit::Restart { uptime: started.elapsed() };
            }
            result = &mut writer => {
                if let Err(error) = result {
                    log::debug!("plugin host '{}' writer task failed: {error}", host.name);
                }
                break HostGenerationExit::Restart { uptime: started.elapsed() };
            }
            status = child.wait() => {
                child_exited = true;
                match status {
                    Ok(status) if status.success() => {
                        log::warn!("plugin host '{}' exited; restarting", host.name);
                    }
                    Ok(status) => {
                        log::error!("plugin host '{}' exited with {status}; restarting", host.name);
                    }
                    Err(error) => {
                        log::error!("plugin host '{}' wait failed: {error}", host.name);
                    }
                }
                break HostGenerationExit::Restart { uptime: started.elapsed() };
            }
            outbound = control.recv() => {
                let Some(outbound) = outbound else {
                    shutdown.cancel();
                    break HostGenerationExit::Shutdown;
                };
                if matches!(
                    &outbound,
                    HostOutbound::Notify {
                        request: HostRequest::Shutdown,
                        ..
                    }
                ) {
                    shutdown.cancel();
                    let _ = generation_control.send(outbound).await;
                    break HostGenerationExit::Shutdown;
                }
                if matches!(
                    &outbound,
                    HostOutbound::Notify {
                        generation: Some(candidate),
                        ..
                    } if *candidate != generation
                ) {
                    continue;
                }
                if generation_control.send(outbound).await.is_err() {
                    break HostGenerationExit::Restart { uptime: started.elapsed() };
                }
            }
            event = events.recv(), if events_open => {
                match event {
                    Some(event) => {
                        if generation_events.send(event).await.is_err() {
                            break HostGenerationExit::Restart { uptime: started.elapsed() };
                        }
                    }
                    None => events_open = false,
                }
            }
        }
    };

    state.end_generation(generation);
    stop_plugin_child(&mut child, child_exited, &host.name).await;
    outcome
}

async fn supervise_plugin_host(
    config: PluginConfig,
    host: PluginHostConfig,
    ingress: crate::runtime::RuntimeIngress,
    state: PluginHostState,
    work: helix_runtime::Work,
    mut control: helix_runtime::Receiver<HostOutbound>,
    mut events: helix_runtime::Receiver<helix_plugin_api::events::PluginEvent>,
    shutdown: helix_runtime::Token,
    stopped: tokio::sync::watch::Sender<bool>,
) {
    let mut backoff = RestartBackoff::default();
    loop {
        if shutdown.is_canceled() {
            break;
        }
        match run_host_generation(
            &config,
            &host,
            &ingress,
            &state,
            &work,
            &mut control,
            &mut events,
            &shutdown,
        )
        .await
        {
            HostGenerationExit::Shutdown => break,
            HostGenerationExit::Restart { uptime } => {
                if shutdown.is_canceled() {
                    break;
                }
                let delay = backoff.after_generation(uptime);
                log::info!(
                    "plugin host '{}' restarting in {}ms",
                    host.name,
                    delay.as_millis()
                );
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown.canceled() => break,
                }
            }
        }
    }
    state.release_all_resources();
    let _ = stopped.send(true);
}

pub(crate) fn spawn_plugin_runtime(
    config: &PluginConfig,
    ingress: crate::runtime::RuntimeIngress,
    foreground: crate::runtime::ForegroundEvents,
    work: helix_runtime::Work,
) -> Result<PluginRuntime, helix_plugin::PluginConfigError> {
    config.validate()?;
    Ok(PluginRuntime {
        hosts: Arc::new(RwLock::new(spawn_plugin_hosts(
            config, ingress, foreground, work,
        ))),
        config: Arc::new(RwLock::new(config.clone())),
    })
}

fn spawn_plugin_hosts(
    config: &PluginConfig,
    ingress: crate::runtime::RuntimeIngress,
    foreground: crate::runtime::ForegroundEvents,
    work: helix_runtime::Work,
) -> Vec<SupervisedPluginHost> {
    const CONTROL_CAPACITY: usize = 64;
    const EVENT_CAPACITY: usize = 256;

    if !config.enabled {
        return Vec::new();
    }

    let mut hosts = Vec::new();
    for host in &config.hosts {
        let (control_tx, control_rx) = helix_runtime::channel(CONTROL_CAPACITY);
        let (event_tx, event_rx) = helix_runtime::channel(EVENT_CAPACITY);
        let shutdown = helix_runtime::Token::new();
        let (stopped_tx, stopped_rx) = tokio::sync::watch::channel(false);
        let state = PluginHostState::new(
            PluginHostId::next(),
            host.name.clone(),
            ingress.clone(),
            foreground.clone(),
            control_tx.clone(),
            work.clone(),
        );
        work.spawn(supervise_plugin_host(
            host_config(config, host),
            host.clone(),
            ingress.clone(),
            state.clone(),
            work.clone(),
            control_rx,
            event_rx,
            shutdown.clone(),
            stopped_tx,
        ))
        .detach();

        hosts.push(SupervisedPluginHost {
            name: host.name.clone(),
            state,
            control: control_tx,
            events: event_tx,
            dropped_events: Arc::default(),
            shutdown,
            stopped: stopped_rx,
        });
    }

    hosts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeDelivery;
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    fn test_editor() -> Editor {
        let theme_loader = helix_view::theme::Loader::new(&[]);
        let syn_loader = helix_core::config::default_lang_loader();
        let config = helix_view::editor::Config::default();
        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = helix_view::handlers::Handlers::dummy();
        Editor::new(
            helix_view::graphics::Rect::new(0, 0, 120, 40),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(
                config,
                |c: &helix_view::editor::Config| c,
            )),
            helix_runtime::test::runtime(),
            handlers,
        )
    }

    fn test_command_host() -> TermCommandHost {
        TermCommandHost {
            next_command_handle: std::sync::atomic::AtomicU64::new(1),
            commands: HashMap::new(),
            published: Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new())),
        }
    }

    fn test_foreground() -> crate::runtime::ForegroundEvents {
        crate::runtime::ForegroundEvents::new()
    }

    fn test_host_state(
        id: u64,
        name: &str,
        foreground: crate::runtime::ForegroundEvents,
    ) -> (PluginHostState, helix_runtime::Receiver<HostOutbound>) {
        let (outbound, receiver) = helix_runtime::channel(8);
        let runtime = helix_runtime::Runtime::current().expect("tokio test runtime");
        let (cleanup_ingress, _cleanup_events) =
            crate::runtime::RuntimeIngress::channel(runtime.clone());
        let state = PluginHostState::new(
            PluginHostId::from_raw(NonZeroU64::new(id).unwrap()),
            name.into(),
            cleanup_ingress,
            foreground,
            outbound,
            runtime.work().clone(),
        );
        let generation = state.begin_generation();
        (state.for_generation(generation), receiver)
    }

    fn plugin_id() -> PluginId {
        PluginId::from_raw(NonZeroU64::new(1).unwrap())
    }

    fn other_plugin_id() -> PluginId {
        PluginId::from_raw(NonZeroU64::new(2).unwrap())
    }

    #[tokio::test]
    async fn foreground_host_state_contention_returns_busy() {
        let (state, _outbound) = test_host_state(3, "contended", test_foreground());
        let _held = state.lock();

        assert!(matches!(state.try_lock(), Err(ContractError::Busy { .. })));
    }

    #[tokio::test]
    async fn task_completion_does_not_wait_for_host_state() {
        let (state, mut outbound) = test_host_state(4, "completion", test_foreground());
        let operation = PluginOperationToken::from_raw(NonZeroU64::new(9).unwrap());
        let (_, responder) = state
            .begin_task(plugin_id(), operation)
            .expect("begin operation");
        let held = state.lock();

        responder.send(Ok(helix_plugin_api::PluginTaskResult::Unit));
        let delivery = tokio::time::timeout(std::time::Duration::from_secs(1), outbound.recv())
            .await
            .expect("task completion timeout")
            .expect("task completion delivery");
        drop(held);

        assert!(matches!(
            delivery,
            HostOutbound::Notify {
                request: HostRequest::TaskCompleted {
                    operation: delivered,
                    result: Ok(helix_plugin_api::PluginTaskResult::Unit),
                },
                ..
            } if delivered == operation
        ));
    }

    #[tokio::test]
    async fn generation_cleanup_uses_background_ingress() {
        let runtime = helix_runtime::Runtime::current().expect("tokio test runtime");
        let (cleanup_ingress, mut cleanup_events) =
            crate::runtime::RuntimeIngress::channel(runtime.clone());
        let foreground = test_foreground();
        let (outbound, _outbound_events) = helix_runtime::channel(8);
        let state = PluginHostState::new(
            PluginHostId::from_raw(NonZeroU64::new(5).unwrap()),
            "cleanup".into(),
            cleanup_ingress,
            foreground.clone(),
            outbound,
            runtime.work().clone(),
        );
        let generation = state.begin_generation();
        let panel = PanelHandle::from_raw(NonZeroU64::new(21).unwrap());
        let keymap = KeymapHandle::from_raw(NonZeroU64::new(22).unwrap());
        {
            let mut inner = state.lock();
            inner.plugins.insert(plugin_id());
            inner.panel.panel_owners.insert(panel, plugin_id());
            inner.keymap.owners.insert(keymap, plugin_id());
            inner
                .command
                .register_command(plugin_id(), command("cleanup-command"))
                .expect("register cleanup command");
        }

        let worker_state = state.clone();
        std::thread::spawn(move || worker_state.end_generation(generation))
            .join()
            .expect("cleanup worker");

        let first = cleanup_events.recv().await.expect("panel cleanup");
        let second = cleanup_events.recv().await.expect("keymap cleanup");
        assert!(matches!(
            first,
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::ReleaseResources {
                plugin,
                panels,
            })) if plugin == plugin_id() && panels == [panel]
        ));
        assert!(matches!(
            second,
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::RemoveKeymap {
                keymap: removed,
            })) if removed == keymap
        ));
        assert!(state.command_snapshot(generation).is_empty());
        assert!(foreground.pop().is_none());
    }

    fn command(name: &str) -> CommandDefinition {
        CommandDefinition {
            name: name.into(),
            doc: None,
            args: None,
        }
    }

    fn keymap_definition(command: &str) -> helix_plugin_api::KeymapDefinition {
        helix_plugin_api::KeymapDefinition {
            mode: helix_plugin_api::KeymapMode::Normal,
            scope: helix_plugin_api::KeymapScope::default(),
            bindings: vec![helix_plugin_api::KeymapBinding {
                keys: vec!["F24".into()],
                commands: vec![command.into()],
            }],
        }
    }

    #[tokio::test]
    async fn plugin_host_callbacks_keep_host_identity_when_tokens_overlap() {
        let foreground_a = test_foreground();
        let foreground_b = test_foreground();
        let (state_a, mut outbound_a) = test_host_state(11, "host-a", foreground_a.clone());
        let (state_b, mut outbound_b) = test_host_state(12, "host-b", foreground_b.clone());

        let token_a = state_a
            .lock()
            .ui
            .prompt(
                plugin_id(),
                contract_requests::PromptRequest {
                    message: "A".into(),
                    default: None,
                },
            )
            .unwrap();
        let token_b = state_b
            .lock()
            .ui
            .prompt(
                plugin_id(),
                contract_requests::PromptRequest {
                    message: "B".into(),
                    default: None,
                },
            )
            .unwrap();
        assert_eq!(token_a, token_b);

        let callback_a = match foreground_a.pop().expect("host A prompt") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::Prompt { callback, .. })) => {
                callback
            }
            _ => panic!("expected plugin host A callback"),
        };
        let callback_b = match foreground_b.pop().expect("host B prompt") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::Prompt { callback, .. })) => {
                callback
            }
            _ => panic!("expected plugin host B callback"),
        };
        assert_eq!(callback_a.identity(), (state_a.id(), token_a));
        assert_eq!(callback_b.identity(), (state_b.id(), token_b));
        assert_ne!(callback_a.identity().0, callback_b.identity().0);

        callback_a.send(helix_plugin_api::DynamicValue::String("a".into()));
        callback_b.send(helix_plugin_api::DynamicValue::String("b".into()));
        callback_a.send(helix_plugin_api::DynamicValue::String("duplicate".into()));

        match outbound_a.recv().await.expect("host A callback delivery") {
            HostOutbound::Notify {
                request: HostRequest::UiCallback { callback, value },
                ..
            } => {
                assert_eq!(callback, token_a);
                assert_eq!(value, helix_plugin_api::DynamicValue::String("a".into()));
            }
            _ => panic!("expected typed host A callback"),
        }
        match outbound_b.recv().await.expect("host B callback delivery") {
            HostOutbound::Notify {
                request: HostRequest::UiCallback { callback, value },
                ..
            } => {
                assert_eq!(callback, token_b);
                assert_eq!(value, helix_plugin_api::DynamicValue::String("b".into()));
            }
            _ => panic!("expected typed host B callback"),
        }
        tokio::task::yield_now().await;
        assert!(matches!(
            outbound_a.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn plugin_command_and_panel_routes_target_the_exact_host() {
        let (state_a, mut outbound_a) = test_host_state(21, "host-a", test_foreground());
        let (state_b, mut outbound_b) = test_host_state(22, "host-b", test_foreground());
        let command_a = state_a
            .lock()
            .command
            .register_command(plugin_id(), command("shared-name"))
            .unwrap();
        let command_b = state_b
            .lock()
            .command
            .register_command(plugin_id(), command("shared-name"))
            .unwrap();
        assert_eq!(command_a, command_b);

        let (events_a, _events_rx_a) = helix_runtime::channel(1);
        let (events_b, _events_rx_b) = helix_runtime::channel(1);
        let (_stopped_a, stopped_a) = tokio::sync::watch::channel(false);
        let (_stopped_b, stopped_b) = tokio::sync::watch::channel(false);
        let hosts = PluginRuntime {
            hosts: Arc::new(RwLock::new(vec![
                SupervisedPluginHost {
                    name: "host-a".into(),
                    state: state_a.clone(),
                    control: state_a.route.outbound.clone(),
                    events: events_a,
                    dropped_events: Arc::default(),
                    shutdown: helix_runtime::Token::new(),
                    stopped: stopped_a,
                },
                SupervisedPluginHost {
                    name: "host-b".into(),
                    state: state_b.clone(),
                    control: state_b.route.outbound.clone(),
                    events: events_b,
                    dropped_events: Arc::default(),
                    shutdown: helix_runtime::Token::new(),
                    stopped: stopped_b,
                },
            ])),
            config: Arc::new(RwLock::new(PluginConfig::default())),
        };
        let snapshot = hosts.command_snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].id.command, snapshot[1].id.command);
        assert_ne!(snapshot[0].id.host, snapshot[1].id.host);

        let host_b_command = snapshot
            .iter()
            .find(|command| command.id.host == state_b.id())
            .unwrap();
        hosts
            .invoke_command(host_b_command.id, vec!["arg".into()])
            .unwrap();
        match outbound_b.recv().await.expect("host B command") {
            HostOutbound::Notify {
                request: HostRequest::CommandInvoke { command, args },
                ..
            } => {
                assert_eq!(command, command_b);
                assert_eq!(args, ["arg"]);
            }
            _ => panic!("expected exact host B command invocation"),
        }
        assert!(matches!(
            outbound_a.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));

        let panel = PanelHandle::from_raw(NonZeroU64::new(41).unwrap());
        PluginPanelKeyRoute::new(state_a.route.clone(), panel).dispatch("C-x".into());
        match outbound_a.recv().await.expect("host A panel key") {
            HostOutbound::Notify {
                request: HostRequest::PanelKey { panel: routed, key },
                ..
            } => {
                assert_eq!(routed, panel);
                assert_eq!(key, "C-x");
            }
            _ => panic!("expected exact host A panel key"),
        }

        hosts.reload().expect("enqueue reload for every host");
        assert!(matches!(
            outbound_a.recv().await,
            Some(HostOutbound::Notify {
                request: HostRequest::Reload,
                ..
            })
        ));
        assert!(matches!(
            outbound_b.recv().await,
            Some(HostOutbound::Notify {
                request: HostRequest::Reload,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn panel_host_enqueues_typed_panel_handle() {
        let sender = test_foreground();
        let (state, _outbound) = test_host_state(31, "panel", sender.clone());
        let mut host = TermPanelHost {
            sender: sender.clone(),
            panel_owners: HashMap::new(),
            host_route: state.route.clone(),
        };
        let mut editor = test_editor();

        let panel = host
            .service(&mut editor)
            .register_panel(
                plugin_id(),
                contract_requests::PanelRegistration {
                    title: "Plugin".into(),
                    side: contract_requests::PanelSide::Right,
                    size: Some(contract_requests::PanelSizeSpec::Fixed(30)),
                    hidden: false,
                    content: Vec::new(),
                },
            )
            .expect("register panel");

        assert!(adapt::resolve_panel(&editor.model, panel).is_ok());
        match sender.pop().expect("push panel event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::PushPanel {
                panel: pushed,
                ..
            })) => {
                assert_eq!(pushed, panel);
            }
            _ => panic!("expected typed push panel command"),
        }
    }

    #[tokio::test]
    async fn resource_host_releases_only_the_requested_plugin() {
        let sender = test_foreground();
        let mut owners = HashMap::from([
            (
                PanelHandle::from_raw(NonZeroU64::new(11).unwrap()),
                plugin_id(),
            ),
            (
                PanelHandle::from_raw(NonZeroU64::new(12).unwrap()),
                other_plugin_id(),
            ),
        ]);
        let mut host = TermResourceHost {
            sender: PluginUiSender::Foreground(sender.clone()),
            panel_owners: &mut owners,
        };

        host.release_plugin_resources(plugin_id()).unwrap();

        match sender.pop().expect("release resources event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::ReleaseResources {
                plugin,
                panels,
            })) => {
                assert_eq!(plugin, plugin_id());
                assert_eq!(panels.len(), 1);
                assert_eq!(panels[0].raw().get(), 11);
            }
            _ => panic!("expected resource release command"),
        }
        assert_eq!(owners.len(), 1);
        assert_eq!(owners.values().next(), Some(&other_plugin_id()));
    }

    #[tokio::test]
    async fn panel_host_rejects_foreign_panel_handles() {
        let sender = test_foreground();
        let (state, _outbound) = test_host_state(32, "panel", sender.clone());
        let mut host = TermPanelHost {
            sender,
            panel_owners: HashMap::new(),
            host_route: state.route.clone(),
        };
        let mut editor = test_editor();

        let panel = host
            .service(&mut editor)
            .register_panel(
                plugin_id(),
                contract_requests::PanelRegistration {
                    title: "Plugin".into(),
                    side: contract_requests::PanelSide::Right,
                    size: Some(contract_requests::PanelSizeSpec::Fixed(30)),
                    hidden: false,
                    content: Vec::new(),
                },
            )
            .expect("register panel");

        let err = host
            .service(&mut editor)
            .update_panel(
                other_plugin_id(),
                PanelUpdateRequest {
                    panel,
                    title: Some("Hijacked".into()),
                    content: None,
                },
            )
            .expect_err("foreign plugin must not update panel");
        assert!(matches!(err, ContractError::PermissionDenied { .. }));

        let err = host
            .service(&mut editor)
            .close_panel(other_plugin_id(), PanelCloseRequest { panel })
            .expect_err("foreign plugin must not close panel");
        assert!(matches!(err, ContractError::PermissionDenied { .. }));
        assert!(adapt::resolve_panel(&editor.model, panel).is_ok());
    }

    #[tokio::test]
    async fn panel_host_enqueues_typed_panel_mutations() {
        let sender = test_foreground();
        let (state, _outbound) = test_host_state(33, "panel", sender.clone());
        let mut host = TermPanelHost {
            sender: sender.clone(),
            panel_owners: HashMap::new(),
            host_route: state.route.clone(),
        };
        let mut editor = test_editor();

        let panel = host
            .service(&mut editor)
            .register_panel(
                plugin_id(),
                contract_requests::PanelRegistration {
                    title: "Plugin".into(),
                    side: contract_requests::PanelSide::Right,
                    size: Some(contract_requests::PanelSizeSpec::Fixed(30)),
                    hidden: false,
                    content: Vec::new(),
                },
            )
            .expect("register panel");
        let _ = sender.pop().expect("push panel event");

        host.service(&mut editor)
            .update_panel(
                plugin_id(),
                PanelUpdateRequest {
                    panel,
                    title: Some("Renamed".into()),
                    content: None,
                },
            )
            .expect("update panel");
        match sender.pop().expect("update panel event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::UpdatePanel {
                panel: updated,
                title,
                ..
            })) => {
                assert_eq!(updated, panel);
                assert_eq!(title.as_deref(), Some("Renamed"));
            }
            _ => panic!("expected typed update panel command"),
        }

        host.service(&mut editor)
            .toggle_panel(plugin_id(), TogglePanelRequest { panel })
            .expect("toggle panel");
        match sender.pop().expect("toggle panel event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::TogglePanel {
                panel: toggled,
            })) => {
                assert_eq!(toggled, panel);
            }
            _ => panic!("expected typed toggle panel command"),
        }

        host.service(&mut editor)
            .focus_panel(plugin_id(), contract_requests::FocusPanelRequest { panel })
            .expect("focus panel");
        match sender.pop().expect("focus panel event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::FocusPanel {
                panel: focused,
            })) => {
                assert_eq!(focused, panel);
            }
            _ => panic!("expected typed focus panel command"),
        }

        host.service(&mut editor)
            .resize_panel(
                plugin_id(),
                ResizePanelRequest {
                    panel,
                    size: contract_requests::PanelSizeSpec::Percent(40),
                },
            )
            .expect("resize panel");
        match sender.pop().expect("resize panel event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::ResizePanel {
                panel: resized,
                size,
            })) => {
                assert_eq!(resized, panel);
                assert_eq!(size, contract_requests::PanelSizeSpec::Percent(40));
            }
            _ => panic!("expected typed resize panel command"),
        }
    }

    #[tokio::test]
    async fn ui_host_enqueues_contract_ui_requests() {
        let sender = test_foreground();
        let (state, _outbound) = test_host_state(7, "ui", sender.clone());
        let mut state = state.lock();
        let host = &mut state.ui;

        host.notify(NotifyRequest {
            level: contract_requests::NotifyLevel::Warn,
            message: "Careful".into(),
        })
        .expect("enqueue notification");
        match sender.pop().expect("notify event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::Notify { level, message })) => {
                assert_eq!(level, contract_requests::NotifyLevel::Warn);
                assert_eq!(message, "Careful");
            }
            _ => panic!("expected plugin notify command"),
        }

        let prompt = host
            .prompt(
                plugin_id(),
                contract_requests::PromptRequest {
                    message: "Name?".into(),
                    default: Some("helix".into()),
                },
            )
            .expect("enqueue prompt");
        match sender.pop().expect("prompt event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::Prompt { request, callback })) => {
                assert_eq!(callback.identity().1, prompt);
                assert_eq!(request.message, "Name?");
                assert_eq!(request.default.as_deref(), Some("helix"));
            }
            _ => panic!("expected plugin prompt command"),
        }

        let confirm = host
            .confirm(
                plugin_id(),
                contract_requests::ConfirmRequest {
                    message: "Continue?".into(),
                },
            )
            .expect("enqueue confirm");
        match sender.pop().expect("confirm event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::Confirm {
                request,
                callback,
            })) => {
                assert_eq!(callback.identity().1, confirm);
                assert_eq!(request.message, "Continue?");
            }
            _ => panic!("expected plugin confirm command"),
        }

        let picker = host
            .picker(
                plugin_id(),
                contract_requests::PickerRequest {
                    items: vec!["one".into(), "two".into()],
                    prompt: Some("Pick:".into()),
                },
            )
            .expect("enqueue picker");
        match sender.pop().expect("picker event") {
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::Picker { request, callback })) => {
                assert_eq!(callback.identity().1, picker);
                assert_eq!(request.items, ["one", "two"]);
                assert_eq!(request.prompt.as_deref(), Some("Pick:"));
            }
            _ => panic!("expected plugin picker command"),
        }
    }

    #[test]
    fn command_registration_rejects_builtin_name_and_alias() {
        let mut host = test_command_host();

        for name in ["write", "w", "move_char_left"] {
            let err = host
                .register_command(plugin_id(), command(name))
                .expect_err("builtin command names and aliases must be reserved");
            assert!(matches!(err, ContractError::InvalidRequest { .. }));
        }
    }

    #[test]
    fn keymap_host_compiles_before_ack_and_enforces_ownership() {
        let foreground = test_foreground();
        let mut host = TermKeymapHost {
            foreground: foreground.clone(),
            next_keymap_handle: std::sync::atomic::AtomicU64::new(1),
            owners: HashMap::new(),
        };
        let keymap = host
            .register_keymap(plugin_id(), keymap_definition(":write"))
            .expect("register valid keymap");
        assert!(matches!(
            foreground.pop().expect("set keymap delivery"),
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::SetKeymap {
                keymap: delivered,
                ..
            })) if delivered == keymap
        ));

        let mut invalid = keymap_definition(":write");
        invalid.bindings[0].keys.clear();
        assert!(matches!(
            host.update_keymap(
                plugin_id(),
                KeymapUpdateRequest {
                    keymap,
                    definition: invalid,
                },
            ),
            Err(ContractError::InvalidRequest { .. })
        ));
        assert!(foreground.pop().is_none());

        assert!(matches!(
            host.remove_keymap(other_plugin_id(), KeymapRemoveRequest { keymap }),
            Err(ContractError::PermissionDenied { .. })
        ));
        host.remove_keymap(plugin_id(), KeymapRemoveRequest { keymap })
            .expect("owner removes keymap");
        assert!(matches!(
            foreground.pop().expect("remove keymap delivery"),
            RuntimeDelivery::Ui(UiCommand::Plugin(PluginCommand::RemoveKeymap {
                keymap: delivered,
            })) if delivered == keymap
        ));
    }

    #[test]
    fn command_catalog_preserves_builtin_signatures_and_tracks_plugins() {
        let mut host = test_command_host();
        let write = host
            .command_catalog()
            .into_iter()
            .find(|command| command.name == "write")
            .expect("write command is discoverable");
        assert_eq!(write.aliases, ["w"]);
        assert_eq!(write.kind, helix_plugin_api::CommandKind::Typable);
        assert_eq!(write.scope, helix_plugin_api::CommandScope::Frontend);
        let signature = write.signature.expect("builtin signature");
        assert_eq!(signature.min_positionals, 0);
        assert_eq!(signature.max_positionals, Some(1));
        assert!(signature.flags.iter().any(|flag| flag.name == "no-format"));

        let static_command = host
            .command_catalog()
            .into_iter()
            .find(|command| command.name == "move_char_left")
            .expect("static command is discoverable");
        assert_eq!(static_command.kind, helix_plugin_api::CommandKind::Static);
        assert_eq!(
            static_command.scope,
            helix_plugin_api::CommandScope::Viewport
        );
        assert!(static_command.signature.is_none());

        let handle = host
            .register_command(
                plugin_id(),
                CommandDefinition {
                    name: "plugin-command".into(),
                    doc: Some("Initial documentation".into()),
                    args: Some(vec!["path".into()]),
                },
            )
            .expect("register plugin command");
        let plugin_command = host
            .command_catalog()
            .into_iter()
            .find(|command| command.name == "plugin-command")
            .expect("registered command is discoverable");
        assert_eq!(plugin_command.kind, helix_plugin_api::CommandKind::Plugin);
        assert_eq!(plugin_command.doc, "Initial documentation");
        assert_eq!(plugin_command.arguments, ["path"]);
        assert!(plugin_command.signature.is_none());

        host.update_command(
            plugin_id(),
            CommandUpdateRequest {
                command: handle,
                name: Some("renamed-command".into()),
                doc: Some("Updated documentation".into()),
                args: Some(vec!["file".into(), "line".into()]),
            },
        )
        .expect("update plugin command");
        let catalog = host.command_catalog();
        assert!(catalog
            .iter()
            .all(|command| command.name != "plugin-command"));
        let renamed = catalog
            .iter()
            .find(|command| command.name == "renamed-command")
            .expect("renamed command is discoverable");
        assert_eq!(renamed.doc, "Updated documentation");
        assert_eq!(renamed.arguments, ["file", "line"]);

        host.remove_command(plugin_id(), CommandRemoveRequest { command: handle })
            .expect("remove plugin command");
        assert!(host
            .command_catalog()
            .iter()
            .all(|command| command.name != "renamed-command"));
    }

    #[test]
    fn command_update_rejects_builtin_name() {
        let mut host = test_command_host();
        let handle = host
            .register_command(plugin_id(), command("plugin-command"))
            .expect("register plugin command");

        let err = host
            .update_command(
                plugin_id(),
                CommandUpdateRequest {
                    command: handle,
                    name: Some("quit".into()),
                    doc: None,
                    args: None,
                },
            )
            .expect_err("builtin command names must be reserved");
        assert!(matches!(err, ContractError::InvalidRequest { .. }));
    }

    #[test]
    fn command_host_rejects_foreign_command_handles() {
        let mut host = test_command_host();
        let handle = host
            .register_command(plugin_id(), command("plugin-command"))
            .expect("register plugin command");

        let err = host
            .update_command(
                other_plugin_id(),
                CommandUpdateRequest {
                    command: handle,
                    name: Some("other-name".into()),
                    doc: None,
                    args: None,
                },
            )
            .expect_err("foreign plugin must not update command");
        assert!(matches!(err, ContractError::PermissionDenied { .. }));

        let err = host
            .remove_command(other_plugin_id(), CommandRemoveRequest { command: handle })
            .expect_err("foreign plugin must not remove command");
        assert!(matches!(err, ContractError::PermissionDenied { .. }));

        host.remove_command(plugin_id(), CommandRemoveRequest { command: handle })
            .expect("owner can remove command");
    }

    #[test]
    fn event_host_rejects_foreign_subscription_handles() {
        let mut host = TermEventHost {
            next_subscription_handle: std::sync::atomic::AtomicU64::new(1),
            subscriptions: HashMap::new(),
        };
        let handle = host
            .subscribe(plugin_id(), helix_plugin_api::events::EventKind::HostReady)
            .expect("subscribe");

        let err = host
            .unsubscribe(other_plugin_id(), handle)
            .expect_err("foreign plugin must not unsubscribe");
        assert!(matches!(err, ContractError::PermissionDenied { .. }));

        host.unsubscribe(plugin_id(), handle)
            .expect("owner can unsubscribe");

        let err = host
            .subscribe(
                plugin_id(),
                helix_plugin_api::events::EventKind::DocumentPreSave,
            )
            .expect_err("events without emitters must not be advertised as usable");
        assert!(matches!(err, ContractError::UnsupportedCapability { .. }));
    }

    #[test]
    fn restart_backoff_is_bounded_and_resets_after_a_healthy_generation() {
        let mut backoff = RestartBackoff::default();
        assert_eq!(
            backoff.after_generation(std::time::Duration::ZERO),
            std::time::Duration::from_millis(100)
        );
        assert_eq!(
            backoff.after_generation(std::time::Duration::ZERO),
            std::time::Duration::from_millis(200)
        );
        for _ in 0..16 {
            let delay = backoff.after_generation(std::time::Duration::ZERO);
            assert!(delay <= RestartBackoff::MAX);
        }
        assert_eq!(
            backoff.after_generation(RestartBackoff::RESET_AFTER),
            std::time::Duration::from_millis(100)
        );
    }

    #[tokio::test]
    async fn reconfigure_preserves_hosts_when_plugin_config_is_unchanged() {
        let runtime = helix_runtime::Runtime::current().unwrap();
        let (ingress, _receiver) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let config = PluginConfig {
            enabled: false,
            ..PluginConfig::default()
        };
        let plugins = spawn_plugin_runtime(
            &config,
            ingress.clone(),
            test_foreground(),
            runtime.work().clone(),
        )
        .unwrap();

        assert!(!plugins
            .reconfigure(
                &config,
                ingress.clone(),
                test_foreground(),
                runtime.work().clone(),
            )
            .unwrap());

        let enabled = PluginConfig::default();
        assert!(plugins
            .reconfigure(
                &enabled,
                ingress.clone(),
                test_foreground(),
                runtime.work().clone(),
            )
            .unwrap());
        assert!(!plugins
            .reconfigure(&enabled, ingress, test_foreground(), runtime.work().clone(),)
            .unwrap());
        plugins.shutdown().await;
    }

    #[tokio::test]
    async fn disabled_plugin_runtime_never_spawns_configured_hosts() {
        let runtime = helix_runtime::Runtime::current().unwrap();
        let (ingress, _receiver) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let config = PluginConfig {
            enabled: false,
            hosts: vec![PluginHostConfig {
                name: "must-not-spawn".into(),
                command: "missing-plugin-host".into(),
                args: Vec::new(),
                plugin_dirs: Vec::new(),
            }],
            ..PluginConfig::default()
        };

        let plugins =
            spawn_plugin_runtime(&config, ingress, test_foreground(), runtime.work().clone())
                .unwrap();

        assert!(plugins.host_snapshot().is_empty());
        plugins.shutdown().await;
    }
}
