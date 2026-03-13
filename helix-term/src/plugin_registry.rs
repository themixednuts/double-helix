use crate::compositor::Context;
use crate::job::Jobs;
use crate::ui::PromptEvent;
use helix_core::command_line::Args;
use helix_plugin::types::{EditorCommandRegistry, UiHandler};
use helix_view::Editor;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub enum UiRequest {
    Prompt {
        message: String,
        default: Option<String>,
        plugin_name: String,
        callback_id: u64,
    },
    Confirm {
        message: String,
        plugin_name: String,
        callback_id: u64,
    },
    Picker {
        items: Vec<String>,
        prompt: String,
        plugin_name: String,
        callback_id: u64,
    },
    RegisterPanel {
        plugin_name: String,
        panel_id: String,
        title: String,
        side: String,
        width: u16,
        render_callback_id: u64,
        event_callback_id: Option<u64>,
    },
    RemovePanel {
        plugin_name: String,
        panel_id: String,
    },
}

pub struct TermUiHandler {
    sender: UnboundedSender<UiRequest>,
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
        let _ = self.sender.send(UiRequest::Prompt {
            message,
            default,
            plugin_name,
            callback_id,
        });
    }

    fn confirm(
        &self,
        _editor: &mut Editor,
        message: String,
        plugin_name: String,
        callback_id: u64,
    ) {
        let _ = self.sender.send(UiRequest::Confirm {
            message,
            plugin_name,
            callback_id,
        });
    }

    fn picker(
        &self,
        _editor: &mut Editor,
        items: Vec<String>,
        prompt: String,
        plugin_name: String,
        callback_id: u64,
    ) {
        let _ = self.sender.send(UiRequest::Picker {
            items,
            prompt,
            plugin_name,
            callback_id,
        });
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
        let _ = self.sender.send(UiRequest::RegisterPanel {
            plugin_name,
            panel_id,
            title,
            side,
            width,
            render_callback_id,
            event_callback_id,
        });
    }

    fn remove_panel(&self, _editor: &mut Editor, plugin_name: String, panel_id: String) {
        let _ = self.sender.send(UiRequest::RemovePanel {
            plugin_name,
            panel_id,
        });
    }
}

pub struct TermCommandRegistry {}

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

        // We need a Context. Let's try to create a minimal one.
        let mut jobs = Jobs::new();
        let mut cx = Context {
            editor,
            scroll: None,
            jobs: &mut jobs,
            plugin_manager: None,
        };

        let line = args.join(" ");
        let args_struct = Args::parse(&line, cmd.signature, true, |token| Ok(token.content))
            .map_err(|e| anyhow::anyhow!("Failed to parse arguments: {}", e))?;

        (cmd.fun)(&mut cx, args_struct, PromptEvent::Validate)?;

        Ok(())
    }
}

pub fn get_registry() -> Arc<dyn EditorCommandRegistry> {
    Arc::new(TermCommandRegistry {})
}

pub fn get_ui_handler() -> (Arc<dyn UiHandler>, UnboundedReceiver<UiRequest>) {
    let (tx, rx) = unbounded_channel();
    (Arc::new(TermUiHandler { sender: tx }), rx)
}
