use std::sync::Arc;

use arc_swap::ArcSwap;
use diagnostics::PullAllDocumentsDiagnosticHandler;

use crate::config::Config;
use crate::events;
use helix_view::events::DocumentDidClose;
use crate::handlers::auto_reload::AutoReloadHandler;
use crate::handlers::auto_save::AutoSaveHandler;
use crate::handlers::diagnostics::PullDiagnosticsHandler;
use crate::runtime::RuntimeEvent;

pub use helix_view::handlers::{word_index, Handlers};

use self::blame::BlameHandler;
use self::document_colors::DocumentColorsHandler;

pub(super) mod auto_reload;
mod auto_save;
pub mod blame;
pub mod completion;
pub mod diagnostics;
mod document_colors;
mod prompt;
mod signature_help;
mod snippet;

fn register_assistant_hooks() {
    helix_event::register_hook!(move |event: &mut DocumentDidClose<'_>| {
        let effects = event.editor.untrack_assistant_doc(event.doc.id());
        event.editor.apply_assistant_effects(effects);
        Ok(())
    });
}

pub fn setup(
    config: Arc<ArcSwap<Config>>,
    ingress: helix_runtime::Sender<RuntimeEvent>,
    runtime: helix_runtime::Runtime,
) -> Handlers {
    #[cfg(feature = "integration")]
    {
        helix_event::reset();
    }

    events::register();

    let event_tx = completion::CompletionHandler::spawn(config, runtime.clone(), ingress.clone());
    let signature_hints = signature_help::SignatureHelpHandler::spawn(runtime.clone(), ingress.clone());
    let auto_save = AutoSaveHandler::spawn(runtime.clone(), ingress.clone());
    let auto_reload = AutoReloadHandler::spawn(runtime.clone(), ingress.clone());
    let document_colors = DocumentColorsHandler::spawn(runtime.clone(), ingress.clone());
    let blame = BlameHandler::spawn(runtime.clone(), ingress.clone());
    let word_index = word_index::Handler::spawn(runtime.clone());
    let pull_diagnostics = PullDiagnosticsHandler::spawn(runtime.clone(), ingress.clone());
    let pull_all_documents_diagnostics =
        PullAllDocumentsDiagnosticHandler::spawn(runtime, ingress.clone());

    let handlers = Handlers {
        completions: helix_view::handlers::completion::CompletionHandler::new(event_tx),
        signature_hints,
        auto_save,
        auto_reload,
        document_colors,
        blame,
        word_index,
        pull_diagnostics,
        pull_all_documents_diagnostics,
    };

    helix_view::handlers::register_hooks(&handlers);
    completion::register_hooks(&handlers);
    signature_help::register_hooks(&handlers);
    auto_save::register_hooks(&handlers);
    auto_reload::register_hooks(&handlers);
    diagnostics::register_hooks(&handlers, ingress.clone());
    snippet::register_hooks(&handlers);
    document_colors::register_hooks(&handlers, ingress.clone());
    prompt::register_hooks(&handlers, ingress.clone());
    blame::register_hooks(&handlers);
    register_assistant_hooks();

    handlers
}
