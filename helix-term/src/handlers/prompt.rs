use helix_runtime::{send_blocking, Sender as IngressSender};
use helix_view::handlers::Handlers;

use crate::runtime::{LayerCommand, RuntimeEvent, UiCommand};

pub(super) fn attach(
    editor: &helix_view::Editor,
    _handlers: &Handlers,
    ingress: IngressSender<RuntimeEvent>,
) {
    editor.lifecycle().on_document_focus_lost(move |_event| {
        send_blocking(
            &ingress,
            RuntimeEvent::Ui(UiCommand::Layer(LayerCommand::DismissPromptIfPresent)),
        );
        Ok(())
    });
}
