use helix_event::register_hook;
use helix_runtime::{send_blocking, Sender as IngressSender};
use helix_view::events::DocumentFocusLost;
use helix_view::handlers::Handlers;

use crate::runtime::{LayerCommand, RuntimeEvent, UiCommand};

pub(super) fn register_hooks(_handlers: &Handlers, ingress: IngressSender<RuntimeEvent>) {
    register_hook!(move |_event: &mut DocumentFocusLost<'_>| {
        send_blocking(
            &ingress,
            RuntimeEvent::Ui(UiCommand::Layer(LayerCommand::DismissPromptIfPresent)),
        );
        Ok(())
    });
}
