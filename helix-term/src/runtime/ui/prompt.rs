use crate::{
    compositor::Compositor,
    runtime::{ui::PromptCommand, RuntimeIngress},
    ui::{CmdlinePopup, Prompt},
};
use helix_view::Editor;

pub(crate) fn apply_prompt_command(
    _editor: &mut Editor,
    compositor: &mut Compositor,
    _ingress: RuntimeIngress,
    command: PromptCommand,
) {
    match command {
        PromptCommand::CompletionReady(result) => {
            let target = result.0.prompt_id;
            let applied = compositor.find::<Prompt>().is_some_and(|prompt| {
                prompt.completion_id() == target && prompt.apply_completion_result(result.clone())
            }) || compositor
                .find::<CmdlinePopup>()
                .is_some_and(|prompt| prompt.apply_completion_result(result));
            if applied {
                compositor.need_full_redraw();
            }
        }
    }
}
