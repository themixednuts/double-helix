use crate::{
    compositor::Compositor,
    runtime::ui::command::{LspCodeActionPresentation, LspCommand},
};
use helix_view::Editor;

pub(crate) fn apply_lsp_command(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    cmd: LspCommand,
) {
    match cmd {
        LspCommand::Goto {
            locations,
            empty_message,
        } => {
            if locations.is_empty() {
                editor.set_error(empty_message);
            } else {
                crate::ui::lsp::navigation::goto_locations(editor, compositor, ingress, locations);
            }
        }
        LspCommand::Hover { hovers, display } => {
            crate::ui::lsp::hover::show_hover(editor, compositor, hovers, display);
        }
        LspCommand::CodeActions {
            items,
            presentation,
        } => {
            if items.is_empty() {
                editor.set_error("No code actions available");
            } else {
                match presentation {
                    LspCodeActionPresentation::Menu => {
                        crate::ui::lsp::code_actions::show_code_action_menu(
                            editor, compositor, ingress, items,
                        );
                    }
                    LspCodeActionPresentation::Picker => {
                        crate::ui::lsp::code_actions::show_code_action_picker(
                            editor, compositor, ingress, items,
                        );
                    }
                }
            }
        }
        LspCommand::DocumentSymbols { symbols } => {
            crate::ui::lsp::document_symbols::show_document_symbol_picker(
                editor, compositor, ingress, symbols,
            );
        }
        LspCommand::SignatureHelp {
            invoked,
            request,
            response,
        } => {
            crate::ui::lsp::signature_help::show_signature(
                editor, compositor, invoked, request, response,
            );
        }
        LspCommand::PrepareRename {
            prefill,
            history_register,
            language_server_id,
        } => {
            let prompt = crate::commands::lsp::create_rename_prompt(
                editor,
                prefill,
                history_register,
                language_server_id,
            );
            compositor.push(prompt);
        }
    }
}
