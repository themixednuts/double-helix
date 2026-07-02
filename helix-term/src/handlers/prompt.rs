use helix_view::handlers::Handlers;

use crate::runtime::{LayerCommand, UiCommand};

pub(super) fn attach(
    editor: &helix_view::Editor,
    _handlers: &Handlers,
    ingress: crate::runtime::RuntimeIngress,
) {
    editor.lifecycle().on_document_focus_lost(move |_event| {
        ingress.ui(UiCommand::Layer(LayerCommand::DismissPromptIfPresent));
        Ok(())
    });
}
