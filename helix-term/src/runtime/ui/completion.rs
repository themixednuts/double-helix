use crate::{
    compositor::Compositor,
    runtime::{ui::command::CompletionCommand, RuntimeEvent},
};

pub(crate) fn apply_completion_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: helix_runtime::Sender<RuntimeEvent>,
    cmd: CompletionCommand,
) {
    match cmd {
        CompletionCommand::ApplyProviderResponse {
            request,
            response,
            is_incomplete,
        } => {
            crate::ui::completion_ingress::apply_provider_completion_response(
                editor,
                compositor,
                request,
                response,
                is_incomplete,
            );
        }
        CompletionCommand::ReplaceResolvedItem { previous, resolved } => {
            crate::ui::completion_ingress::apply_resolved_completion_item(
                compositor,
                previous.as_ref(),
                resolved,
            );
        }
        CompletionCommand::Show {
            request,
            items,
            context,
            trigger,
        } => {
            crate::ui::completion_ingress::show_completion_popup(
                editor, compositor, ingress, request, items, context, trigger,
            );
        }
        CompletionCommand::RequestDebounced { trigger } => {
            crate::ui::completion_ingress::request_completions(
                trigger, editor, compositor, ingress,
            );
        }
    }
}
