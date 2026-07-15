use crate::{
    compositor::Compositor,
    runtime::ui::command::PkgCommand,
    ui::{overlay::Overlay, pkg::PkgManager},
};
use helix_view::Editor;

pub(crate) fn apply_pkg_command(editor: &mut Editor, compositor: &mut Compositor, cmd: PkgCommand) {
    match cmd {
        PkgCommand::Refresh {
            request_id,
            finished_revision,
            stage,
            result,
        } => {
            if let Some(manager) = compositor.find_id::<Overlay<PkgManager>>(crate::ui::pkg::ID) {
                manager.content.apply_refresh_result(
                    editor,
                    request_id,
                    finished_revision,
                    stage,
                    result,
                );
            }
        }
    }
}
