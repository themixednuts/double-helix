use crate::{
    compositor::Compositor,
    runtime::{ui::command::PickerCommand, RuntimeEvent},
};

pub(crate) fn apply_picker_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    _ingress: helix_runtime::Sender<RuntimeEvent>,
    cmd: PickerCommand,
) {
    match cmd {
        PickerCommand::RequestPreviewHighlight { path } => {
            let Some(picker) = compositor.find_picker() else {
                return;
            };
            picker.request_preview_highlight(editor, path);
        }
        PickerCommand::ApplyPreviewSyntax { path, syntax } => {
            let Some(picker) = compositor.find_picker() else {
                log::info!("picker closed before syntax highlighting finished");
                return;
            };
            picker.apply_preview_syntax(editor, path, syntax);
        }
        PickerCommand::RunDynamicQuery { query } => {
            let Some(picker) = compositor.find_picker() else {
                return;
            };
            picker.run_dynamic_query(editor, query);
        }
    }
}
