use helix_view::handlers::Handlers;

use crate::runtime::{LayerCommand, UiCommand};

pub(super) fn attach(
    editor: &helix_view::Editor,
    _handlers: &Handlers,
    foreground: crate::runtime::ForegroundEvents,
) {
    editor.lifecycle().on_document_focus_lost(move |_event| {
        foreground
            .ui(UiCommand::Layer(LayerCommand::DismissPromptIfPresent))
            .map_err(anyhow::Error::from)?;
        Ok(())
    });
}
