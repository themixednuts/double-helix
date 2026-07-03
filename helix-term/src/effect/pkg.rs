use helix_core::diagnostic::Severity;
use helix_pkg::OpEvent;
use helix_view::Editor;

pub(crate) fn apply_event(editor: &mut Editor, event: &OpEvent) {
    match event {
        OpEvent::Started { name } => {
            editor.notify_with_severity(format!("pkg: started {name}"), Severity::Info);
        }
        OpEvent::Progress { name, message } => {
            editor.set_status(format!("pkg {name}: {message}"));
        }
        OpEvent::Done { name } => {
            editor.notify_with_severity(format!("pkg: finished {name}"), Severity::Info);
        }
        OpEvent::Failed { name, message } => {
            editor.notify_with_severity(format!("pkg: {name} failed: {message}"), Severity::Error);
        }
    }
}
