//! Apply [`super::command::UiCommand`] on the main thread (`Editor` + `Compositor`).

use crate::compositor::Compositor;
use helix_plugin::PluginManager;
use helix_view::Editor;

use super::command::UiCommand;

pub fn apply_ui_command(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    plugin_manager: std::sync::Arc<PluginManager>,
    cmd: UiCommand,
) {
    match cmd {
        UiCommand::Layer(layer) => {
            super::layer::apply_layer_command(editor, compositor, ingress.clone(), layer)
        }
        UiCommand::Completion(cmd) => {
            super::completion::apply_completion_command(editor, compositor, ingress.clone(), *cmd)
        }
        UiCommand::Picker(cmd) => {
            super::picker::apply_picker_command(editor, compositor, ingress.clone(), cmd)
        }
        UiCommand::Document(cmd) => {
            super::document::apply_document_command(editor, cmd);
        }
        UiCommand::Nop => {}
        UiCommand::NeedFullRedraw => compositor.need_full_redraw(),
        UiCommand::Dap(cmd) => {
            super::dap::apply_dap_command(editor, compositor, ingress.clone(), cmd)
        }
        UiCommand::Assistant(cmd) => {
            super::assistant::apply_assistant_command(editor, compositor, ingress.clone(), cmd)
        }
        UiCommand::FileExplorer(cmd) => super::file_explorer::apply_file_explorer_command(
            editor,
            compositor,
            ingress.clone(),
            cmd,
        ),
        UiCommand::Plugin(cmd) => super::plugin::apply_plugin_command(
            editor,
            compositor,
            ingress.clone(),
            plugin_manager,
            cmd,
        ),
        UiCommand::Lsp(cmd) => {
            super::lsp::apply_lsp_command(editor, compositor, ingress.clone(), cmd)
        }
    }
}

/// Like [`apply_ui_command`] when the compositor may be unavailable during shutdown.
pub fn apply_ui_command_opt(
    editor: &mut Editor,
    compositor: &mut Option<&mut Compositor>,
    ingress: crate::runtime::RuntimeIngress,
    plugin_manager: std::sync::Arc<PluginManager>,
    cmd: UiCommand,
) {
    let Some(c) = compositor.as_mut() else {
        return;
    };
    apply_ui_command(editor, c, ingress, plugin_manager, cmd);
}
