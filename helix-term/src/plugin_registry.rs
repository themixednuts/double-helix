use crate::compositor::Context;
use crate::runtime::ui::command::PluginCommand;
use crate::runtime::ExitTaskSet;
use crate::runtime::{RuntimeEvent, RuntimeTaskEvent, UiCommand};
use crate::ui::PromptEvent;
use helix_core::command_line::Args;
use helix_plugin::types::{EditorCommandRegistry, UiHandler};
use helix_runtime::{send_blocking, Sender};
use helix_view::Editor;
use std::sync::Arc;

pub struct TermUiHandler {
    sender: Sender<RuntimeEvent>,
}

impl UiHandler for TermUiHandler {
    fn prompt(
        &self,
        _editor: &mut Editor,
        message: String,
        default: Option<String>,
        plugin_name: String,
        callback_id: u64,
    ) {
        send_blocking(
            &self.sender,
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::Prompt {
                message,
                default,
                plugin_name,
                callback_id,
            })),
        );
    }

    fn confirm(
        &self,
        _editor: &mut Editor,
        message: String,
        plugin_name: String,
        callback_id: u64,
    ) {
        send_blocking(
            &self.sender,
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::Confirm {
                message,
                plugin_name,
                callback_id,
            })),
        );
    }

    fn picker(
        &self,
        _editor: &mut Editor,
        items: Vec<String>,
        prompt: String,
        plugin_name: String,
        callback_id: u64,
    ) {
        send_blocking(
            &self.sender,
            RuntimeEvent::Ui(UiCommand::Plugin(PluginCommand::Picker {
                items,
                prompt,
                plugin_name,
                callback_id,
            })),
        );
    }

    fn register_panel(
        &self,
        _editor: &mut Editor,
        plugin_name: String,
        panel_id: String,
        title: String,
        side: String,
        width: u16,
        render_callback_id: u64,
        event_callback_id: Option<u64>,
    ) {
        send_blocking(
            &self.sender,
            RuntimeEvent::Task(RuntimeTaskEvent::RegisterPluginPanel {
                plugin_name,
                panel_id,
                title,
                side,
                width,
                render_callback_id,
                event_callback_id,
            }),
        );
    }

    fn remove_panel(&self, _editor: &mut Editor, _plugin_name: String, panel_id: String) {
        send_blocking(
            &self.sender,
            RuntimeEvent::Task(RuntimeTaskEvent::RemovePluginPanel { panel_id }),
        );
    }
}

pub struct TermCommandRegistry {
    ingress: crate::runtime::RuntimeEventSender,
}

impl EditorCommandRegistry for TermCommandRegistry {
    fn execute(
        &self,
        editor: &mut Editor,
        name: &str,
        args: &[String],
    ) -> std::result::Result<(), anyhow::Error> {
        // Find the command in TYPABLE_COMMAND_LIST
        let cmd = crate::commands::typed::TYPABLE_COMMAND_LIST
            .iter()
            .find(|c| c.name == name || c.aliases.contains(&name))
            .ok_or_else(|| anyhow::anyhow!("Command not found: {}", name))?;

        let work = editor.runtime().work().clone();
        let exit_task_work = work;
        let mut exit_tasks = ExitTaskSet::new();
        let mut cx = Context {
            editor,
            scroll: None,
            exit_tasks: &mut exit_tasks,
            exit_task_work,
            ingress: self.ingress.clone(),
            idle_reset_tx: helix_runtime::channel(1).0,
            plugin_manager: None,
        };

        let line = args.join(" ");
        let args_struct = Args::parse(&line, cmd.signature, true, |token| Ok(token.content))
            .map_err(|e| anyhow::anyhow!("Failed to parse arguments: {}", e))?;

        (cmd.fun)(&mut cx, args_struct, PromptEvent::Validate)?;

        Ok(())
    }
}

pub fn get_registry(ingress: crate::runtime::RuntimeEventSender) -> Arc<dyn EditorCommandRegistry> {
    Arc::new(TermCommandRegistry { ingress })
}

pub fn get_ui_handler(ingress: crate::runtime::RuntimeEventSender) -> Arc<dyn UiHandler> {
    Arc::new(TermUiHandler { sender: ingress })
}
