//! Apply [`super::command::UiCommand`] on the main thread (`Editor` + `Compositor`).

use super::command::UiCommand;
use crate::compositor::Compositor;

pub fn apply_ui_command(
    compositor: &mut Compositor,
    context: &mut crate::compositor::Context<'_>,
    cmd: UiCommand,
) {
    let ingress = context.ingress.clone();
    let foreground = context.foreground.clone();
    match cmd {
        UiCommand::AfterWrites { .. } => {
            unreachable!("application-owned write continuation reached generic UI apply")
        }
        UiCommand::Layer(layer) => {
            super::layer::apply_layer_command(context.editor, compositor, ingress.clone(), layer)
        }
        UiCommand::Completion(cmd) => super::completion::apply_completion_command(
            context.editor,
            compositor,
            ingress.clone(),
            *cmd,
        ),
        UiCommand::Picker(cmd) => {
            super::picker::apply_picker_command(context.editor, compositor, ingress.clone(), cmd)
        }
        UiCommand::Prompt(cmd) => {
            super::prompt::apply_prompt_command(context.editor, compositor, ingress.clone(), cmd)
        }
        UiCommand::Document(cmd) => {
            super::document::apply_document_command(
                context.editor,
                ingress.clone(),
                foreground,
                cmd,
            );
        }
        UiCommand::Nop => {}
        UiCommand::NeedFullRedraw => compositor.need_full_redraw(),
        UiCommand::Dap(cmd) => {
            super::dap::apply_dap_command(context.editor, compositor, ingress.clone(), cmd)
        }
        UiCommand::Assistant(cmd) => super::assistant::apply_assistant_command(
            context.editor,
            compositor,
            ingress.clone(),
            foreground,
            cmd,
        ),
        UiCommand::FileExplorer(cmd) => super::file_explorer::apply_file_explorer_command(
            context.editor,
            compositor,
            ingress.clone(),
            cmd,
        ),
        UiCommand::Pkg(cmd) => super::pkg::apply_pkg_command(context.editor, compositor, cmd),
        UiCommand::Plugin(cmd) => super::plugin::apply_plugin_command(compositor, context, cmd),
        UiCommand::Lsp(cmd) => super::lsp::apply_lsp_command(
            context.editor,
            compositor,
            ingress.clone(),
            foreground,
            cmd,
        ),
    }
}
