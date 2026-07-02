use crate::{compositor::Compositor, runtime::ui::command::PickerCommand};

pub(crate) fn apply_picker_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    _ingress: crate::runtime::RuntimeIngress,
    cmd: PickerCommand,
) {
    log::info!(
        target: crate::ui::picker::PICKER_TRACE_TARGET,
        "phase=picker_command_apply command={cmd:?}",
    );
    match cmd {
        PickerCommand::RequestPreviewHighlight {
            picker: picker_id,
            path,
        } => {
            let Some(picker) = compositor.find_picker() else {
                return;
            };
            if picker.instance_id() != picker_id {
                return;
            }
            picker.request_preview_highlight(editor, path);
        }
        PickerCommand::ApplyPreview {
            picker: picker_id,
            path,
            preview,
        } => {
            let Some(picker) = compositor.find_picker() else {
                log::info!("picker closed before preview snapshot finished");
                return;
            };
            if picker.instance_id() != picker_id {
                return;
            }
            picker.apply_preview(editor, path, preview);
        }
        PickerCommand::ApplyPreviewSyntax {
            picker: picker_id,
            path,
            syntax,
        } => {
            let Some(picker) = compositor.find_picker() else {
                log::info!("picker closed before syntax highlighting finished");
                return;
            };
            if picker.instance_id() != picker_id {
                return;
            }
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
