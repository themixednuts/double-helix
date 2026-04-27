use crate::compositor::Context;
use crate::runtime::ui::command::PluginCommand;
use crate::runtime::ExitTaskSet;
use crate::runtime::{RuntimeEvent, UiCommand};
use crate::ui::PromptEvent;
use helix_core::command_line::Args;
use helix_plugin::contract::host::{
    PluginCommandHost, PluginEventHost, PluginPanelHost, PluginUiHost, UiCallbackToken,
};
use helix_plugin::contract::metadata::{ApiMetadata, EventKindInfo};
use helix_plugin::contract::requests::{
    self as contract_requests, CommandDefinition, CommandRemoveRequest, CommandUpdateRequest,
    NotifyRequest, PanelCloseRequest, PanelUpdateRequest, ResizePanelRequest, RunCommandRequest,
    TogglePanelRequest,
};
use helix_plugin::contract::{
    adapt, CommandHandle, ContractError, ContractResult, PanelHandle, PluginId, SubscriptionHandle,
};
use helix_runtime::{send_blocking, Sender};
use helix_view::model::FocusTarget;
use helix_view::Editor;
use std::collections::HashMap;
use std::num::NonZeroU64;

fn internal_error(message: impl Into<String>) -> ContractError {
    ContractError::internal(message)
}

fn with_editor<T>(f: impl FnOnce(&Editor) -> ContractResult<T>) -> ContractResult<T> {
    let editor = helix_plugin::lua::get_editor().map_err(|err| internal_error(err.to_string()))?;
    f(editor)
}

fn with_editor_mut<T>(f: impl FnOnce(&mut Editor) -> ContractResult<T>) -> ContractResult<T> {
    let editor =
        helix_plugin::lua::get_editor_mut().map_err(|err| internal_error(err.to_string()))?;
    f(editor)
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

pub struct TermUiHost {
    sender: Sender<RuntimeEvent>,
    next_callback_id: std::sync::atomic::AtomicU64,
}

impl PluginUiHost for TermUiHost {
    fn notify(&mut self, req: NotifyRequest) -> ContractResult<()> {
        with_editor_mut(|editor| {
            match req.level {
                contract_requests::NotifyLevel::Info => editor.set_status(req.message),
                contract_requests::NotifyLevel::Warn => {
                    editor.set_status(format!("Warning: {}", req.message));
                }
                contract_requests::NotifyLevel::Error => editor.set_error(req.message),
            }
            Ok(())
        })
    }

    fn prompt(
        &mut self,
        _plugin: PluginId,
        req: contract_requests::PromptRequest,
    ) -> ContractResult<UiCallbackToken> {
        let token = next_non_zero(&self.next_callback_id);
        let callback = UiCallbackToken::from_raw(token);
        send_blocking(
            &self.sender,
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::Prompt {
                request: req,
                callback,
            })),
        );
        Ok(callback)
    }

    fn confirm(
        &mut self,
        _plugin: PluginId,
        req: contract_requests::ConfirmRequest,
    ) -> ContractResult<UiCallbackToken> {
        let token = next_non_zero(&self.next_callback_id);
        let callback = UiCallbackToken::from_raw(token);
        send_blocking(
            &self.sender,
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::Confirm {
                request: req,
                callback,
            })),
        );
        Ok(callback)
    }

    fn picker(
        &mut self,
        _plugin: PluginId,
        req: contract_requests::PickerRequest,
    ) -> ContractResult<UiCallbackToken> {
        let token = next_non_zero(&self.next_callback_id);
        let callback = UiCallbackToken::from_raw(token);
        send_blocking(
            &self.sender,
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::Picker {
                request: req,
                callback,
            })),
        );
        Ok(callback)
    }
}

pub struct TermPanelHost {
    sender: Sender<RuntimeEvent>,
    panel_owners: HashMap<PanelHandle, PluginId>,
}

impl PluginPanelHost for TermPanelHost {
    fn register_panel(
        &mut self,
        plugin: PluginId,
        reg: contract_requests::PanelRegistration,
    ) -> ContractResult<PanelHandle> {
        use helix_plugin::contract::requests::PanelSizeSpec;
        use helix_view::model::{PanelSide, PanelSize, PluginPanelModel};

        let panel = with_editor_mut(|editor| {
            let contract_requests::PanelRegistration {
                title,
                side,
                size,
                hidden,
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

            let panel_id = editor.model.insert_panel(
                title,
                Box::new(PluginPanelModel),
                panel_side,
                panel_size,
            );

            if hidden {
                let _ = editor.model.toggle_panel(panel_id);
            }

            let panel = adapt::panel_handle(panel_id);
            Ok(panel)
        })?;

        self.panel_owners.insert(panel, plugin);
        send_blocking(
            &self.sender,
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::PushPanel { panel })),
        );
        Ok(panel)
    }

    fn update_panel(&mut self, plugin: PluginId, req: PanelUpdateRequest) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        with_editor_mut(|editor| {
            let panel_id = adapt::resolve_panel(&editor.model, req.panel)?;
            let panel = editor
                .model
                .panels
                .get_mut(panel_id)
                .ok_or_else(|| ContractError::stale_handle(req.panel.to_string()))?;
            if let Some(title) = req.title {
                panel.title = title;
            }
            Ok(())
        })
    }

    fn close_panel(&mut self, plugin: PluginId, req: PanelCloseRequest) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        let panel = req.panel;
        with_editor_mut(|editor| {
            let panel_id = adapt::resolve_panel(&editor.model, req.panel)?;
            if !editor.model.remove_panel(panel_id) {
                return Err(ContractError::stale_handle(req.panel.to_string()));
            }
            Ok(())
        })?;
        self.panel_owners.remove(&panel);
        send_blocking(
            &self.sender,
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::RemovePanel { panel })),
        );
        Ok(())
    }

    fn toggle_panel(&mut self, plugin: PluginId, req: TogglePanelRequest) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        with_editor_mut(|editor| {
            let panel_id = adapt::resolve_panel(&editor.model, req.panel)?;
            editor
                .model
                .toggle_panel(panel_id)
                .ok_or_else(|| ContractError::stale_handle(req.panel.to_string()))?;
            Ok(())
        })
    }

    fn focus_panel(
        &mut self,
        plugin: PluginId,
        req: contract_requests::FocusPanelRequest,
    ) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        with_editor_mut(|editor| {
            let panel_id = adapt::resolve_panel(&editor.model, req.panel)?;
            editor.model.focus_panel(panel_id);
            Ok(())
        })
    }

    fn resize_panel(&mut self, plugin: PluginId, req: ResizePanelRequest) -> ContractResult<()> {
        self.ensure_panel_owner(plugin, req.panel)?;
        with_editor_mut(|editor| {
            let panel_id = adapt::resolve_panel(&editor.model, req.panel)?;
            let panel = editor
                .model
                .panels
                .get_mut(panel_id)
                .ok_or_else(|| ContractError::stale_handle(req.panel.to_string()))?;
            panel.size = match req.size {
                contract_requests::PanelSizeSpec::Fixed(cells) => {
                    helix_view::model::PanelSize::fixed(cells)
                }
                contract_requests::PanelSizeSpec::Percent(percent) => {
                    helix_view::model::PanelSize::Percent(percent)
                }
            };
            Ok(())
        })
    }

    fn list_panels(&self) -> Vec<helix_plugin::contract::snapshots::PanelSnapshot> {
        with_editor(|editor| {
            Ok(editor
                .model
                .panels
                .iter()
                .map(
                    |(panel_id, panel)| helix_plugin::contract::snapshots::PanelSnapshot {
                        handle: adapt::panel_handle(panel_id),
                        title: panel.title.clone(),
                        side: adapt::panel_side_to_contract(panel.side),
                        visible: panel.visible,
                        is_focused: editor.model.focus == FocusTarget::Panel(panel_id),
                    },
                )
                .collect())
        })
        .unwrap_or_default()
    }
}

impl TermPanelHost {
    fn ensure_panel_owner(&self, plugin: PluginId, panel: PanelHandle) -> ContractResult<()> {
        match self.panel_owners.get(&panel) {
            Some(owner) if *owner == plugin => Ok(()),
            Some(_) => Err(permission_denied(plugin, panel)),
            None => Err(ContractError::stale_handle(panel.to_string())),
        }
    }
}

struct RegisteredCommandDefinition {
    plugin: PluginId,
    definition: CommandDefinition,
}

pub struct TermCommandHost {
    ingress: crate::runtime::RuntimeEventSender,
    next_command_handle: std::sync::atomic::AtomicU64,
    commands: HashMap<CommandHandle, RegisteredCommandDefinition>,
}

impl PluginCommandHost for TermCommandHost {
    fn register_command(
        &mut self,
        plugin: PluginId,
        def: CommandDefinition,
    ) -> ContractResult<helix_plugin::contract::CommandHandle> {
        if self.command_name_in_use(&def.name, None) {
            return Err(ContractError::invalid_request(format!(
                "command already registered: {}",
                def.name
            )));
        }

        let handle = helix_plugin::contract::CommandHandle::from_raw(next_non_zero(
            &self.next_command_handle,
        ));
        self.commands.insert(
            handle,
            RegisteredCommandDefinition {
                plugin,
                definition: def,
            },
        );
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
        Ok(())
    }

    fn run_command(&mut self, req: RunCommandRequest) -> ContractResult<()> {
        with_editor_mut(|editor| {
            let cmd = crate::commands::typed::TYPABLE_COMMAND_LIST
                .iter()
                .find(|c| c.name == req.name || c.aliases.contains(&req.name.as_str()))
                .ok_or_else(|| ContractError::not_found(format!("command {}", req.name)))?;

            let work = editor.work();
            let exit_task_work = work;
            let mut exit_tasks = ExitTaskSet::new();
            let mut cx = Context {
                editor,
                scroll: None,
                exit_tasks: &mut exit_tasks,
                exit_task_work,
                notifier: crate::handlers::local::Notifier {
                    ingress: self.ingress.clone(),
                    plugin_events: helix_runtime::channel(1).0,
                },
                ingress: self.ingress.clone(),
                idle_reset_tx: helix_runtime::channel(1).0,
                plugin_manager: None,
            };

            let line = req.args.join(" ");
            let args = Args::parse(&line, cmd.signature, true, |token| Ok(token.content))
                .map_err(|err| ContractError::invalid_request(err.to_string()))?;

            (cmd.fun)(&mut cx, args, PromptEvent::Validate)
                .map_err(|err| ContractError::internal(err.to_string()))
        })
    }
}

impl TermCommandHost {
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

        self.commands
            .iter()
            .any(|(handle, command)| Some(*handle) != except && command.definition.name == name)
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
        _kind: helix_plugin::contract::events::EventKind,
    ) -> ContractResult<helix_plugin::contract::SubscriptionHandle> {
        let handle = helix_plugin::contract::SubscriptionHandle::from_raw(next_non_zero(
            &self.next_subscription_handle,
        ));
        self.subscriptions.insert(handle, plugin);
        Ok(handle)
    }

    fn unsubscribe(
        &mut self,
        plugin: PluginId,
        handle: helix_plugin::contract::SubscriptionHandle,
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

pub fn get_ui_host(
    ingress: crate::runtime::RuntimeEventSender,
) -> Box<dyn PluginUiHost + Send + Sync> {
    Box::new(TermUiHost {
        sender: ingress,
        next_callback_id: std::sync::atomic::AtomicU64::new(1),
    })
}

pub fn get_panel_host(
    ingress: crate::runtime::RuntimeEventSender,
) -> Box<dyn PluginPanelHost + Send + Sync> {
    Box::new(TermPanelHost {
        sender: ingress,
        panel_owners: HashMap::new(),
    })
}

pub fn get_command_host(
    ingress: crate::runtime::RuntimeEventSender,
) -> Box<dyn PluginCommandHost + Send + Sync> {
    Box::new(TermCommandHost {
        ingress,
        next_command_handle: std::sync::atomic::AtomicU64::new(1),
        commands: HashMap::new(),
    })
}

pub fn get_event_host() -> Box<dyn PluginEventHost + Send + Sync> {
    Box::new(TermEventHost {
        next_subscription_handle: std::sync::atomic::AtomicU64::new(1),
        subscriptions: HashMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    fn test_editor() -> Editor {
        let theme_loader = helix_view::theme::Loader::new(helix_loader::runtime_dirs());
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
            ingress: helix_runtime::channel(1).0,
            next_command_handle: std::sync::atomic::AtomicU64::new(1),
            commands: HashMap::new(),
        }
    }

    fn plugin_id() -> PluginId {
        PluginId::from_raw(NonZeroU64::new(1).unwrap())
    }

    fn other_plugin_id() -> PluginId {
        PluginId::from_raw(NonZeroU64::new(2).unwrap())
    }

    fn command(name: &str) -> CommandDefinition {
        CommandDefinition {
            name: name.into(),
            doc: None,
            args: None,
        }
    }

    #[tokio::test]
    async fn panel_host_enqueues_typed_panel_handle() {
        let (sender, mut receiver) = helix_runtime::channel(1);
        let mut host = TermPanelHost {
            sender,
            panel_owners: HashMap::new(),
        };
        let mut editor = test_editor();

        let panel = helix_plugin::lua::with_editor_context(&mut editor, || {
            host.register_panel(
                plugin_id(),
                contract_requests::PanelRegistration {
                    title: "Plugin".into(),
                    side: contract_requests::PanelSide::Right,
                    size: Some(contract_requests::PanelSizeSpec::Fixed(30)),
                    hidden: false,
                },
            )
        })
        .expect("register panel");

        assert!(adapt::resolve_panel(&editor.model, panel).is_ok());
        match receiver.try_recv().expect("push panel event") {
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::PushPanel { panel: pushed })) => {
                assert_eq!(pushed, panel);
            }
            _ => panic!("expected typed push panel command"),
        }
    }

    #[tokio::test]
    async fn panel_host_rejects_foreign_panel_handles() {
        let (sender, _receiver) = helix_runtime::channel(4);
        let mut host = TermPanelHost {
            sender,
            panel_owners: HashMap::new(),
        };
        let mut editor = test_editor();

        let panel = helix_plugin::lua::with_editor_context(&mut editor, || {
            host.register_panel(
                plugin_id(),
                contract_requests::PanelRegistration {
                    title: "Plugin".into(),
                    side: contract_requests::PanelSide::Right,
                    size: Some(contract_requests::PanelSizeSpec::Fixed(30)),
                    hidden: false,
                },
            )
        })
        .expect("register panel");

        let err = helix_plugin::lua::with_editor_context(&mut editor, || {
            host.update_panel(
                other_plugin_id(),
                PanelUpdateRequest {
                    panel,
                    title: Some("Hijacked".into()),
                },
            )
        })
        .expect_err("foreign plugin must not update panel");
        assert!(matches!(err, ContractError::PermissionDenied { .. }));

        let err = helix_plugin::lua::with_editor_context(&mut editor, || {
            host.close_panel(other_plugin_id(), PanelCloseRequest { panel })
        })
        .expect_err("foreign plugin must not close panel");
        assert!(matches!(err, ContractError::PermissionDenied { .. }));
        assert!(adapt::resolve_panel(&editor.model, panel).is_ok());
    }

    #[test]
    fn ui_host_enqueues_contract_ui_requests() {
        let (sender, mut receiver) = helix_runtime::channel(3);
        let mut host = TermUiHost {
            sender,
            next_callback_id: std::sync::atomic::AtomicU64::new(1),
        };

        let prompt = host
            .prompt(
                plugin_id(),
                contract_requests::PromptRequest {
                    message: "Name?".into(),
                    default: Some("helix".into()),
                },
            )
            .expect("enqueue prompt");
        match receiver.try_recv().expect("prompt event") {
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::Prompt { request, callback })) => {
                assert_eq!(callback, prompt);
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
        match receiver.try_recv().expect("confirm event") {
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::Confirm { request, callback })) => {
                assert_eq!(callback, confirm);
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
        match receiver.try_recv().expect("picker event") {
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::Picker { request, callback })) => {
                assert_eq!(callback, picker);
                assert_eq!(request.items, ["one", "two"]);
                assert_eq!(request.prompt.as_deref(), Some("Pick:"));
            }
            _ => panic!("expected plugin picker command"),
        }
    }

    #[test]
    fn command_registration_rejects_builtin_name_and_alias() {
        let mut host = test_command_host();

        for name in ["write", "w"] {
            let err = host
                .register_command(plugin_id(), command(name))
                .expect_err("builtin command names and aliases must be reserved");
            assert!(matches!(err, ContractError::InvalidRequest { .. }));
        }
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
            .subscribe(
                plugin_id(),
                helix_plugin::contract::events::EventKind::HostReady,
            )
            .expect("subscribe");

        let err = host
            .unsubscribe(other_plugin_id(), handle)
            .expect_err("foreign plugin must not unsubscribe");
        assert!(matches!(err, ContractError::PermissionDenied { .. }));

        host.unsubscribe(plugin_id(), handle)
            .expect("owner can unsubscribe");
    }
}
