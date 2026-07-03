use std::sync::Arc;

use arc_swap::ArcSwap;
use diagnostics::PullAllDocumentsDiagnosticHandler;

use crate::config::Config;
use crate::handlers::auto_reload::AutoReloadHandler;
use crate::handlers::auto_save::AutoSaveHandler;
use crate::handlers::diagnostics::PullDiagnosticsHandler;
use crate::handlers::pkg::PkgHandler;

pub use helix_view::handlers::{word_index, Handlers};

use self::blame::BlameHandler;
use self::document_colors::DocumentColorsHandler;
use self::lsp_features::LspFeatureRefreshHandler;

pub(super) mod auto_reload;
mod auto_save;
pub mod blame;
pub mod completion;
pub mod diagnostics;
mod document_colors;
pub mod local;
mod lsp_features;
mod pkg;
mod prompt;
mod signature_help;
mod snippet;

fn attach_assistant_hooks(editor: &helix_view::Editor) {
    editor.lifecycle().on_document_close(move |event| {
        let effects = event.editor.untrack_assistant_doc(event.doc.id());
        event.editor.apply_assistant_effects(effects);
        Ok(())
    });
}

pub fn setup(
    config: Arc<ArcSwap<Config>>,
    ingress: crate::runtime::RuntimeIngress,
    runtime: helix_runtime::Runtime,
) -> Handlers {
    let event_tx = completion::CompletionHandler::spawn(config, runtime.clone(), ingress.clone());
    let signature_hints =
        signature_help::SignatureHelpHandler::spawn(runtime.clone(), ingress.clone());
    let auto_save = AutoSaveHandler::spawn(runtime.clone(), ingress.clone());
    let auto_reload = AutoReloadHandler::spawn(runtime.clone(), ingress.clone());
    let pkg = PkgHandler::spawn(runtime.clone(), ingress.clone());
    let document_colors = DocumentColorsHandler::spawn(runtime.clone(), ingress.clone());
    let lsp_feature_refresh = LspFeatureRefreshHandler::spawn(runtime.clone(), ingress.clone());
    let blame = BlameHandler::spawn(runtime.clone(), ingress.clone());
    let word_index = word_index::Handler::spawn(runtime.clone());
    let pull_diagnostics = PullDiagnosticsHandler::spawn(runtime.clone(), ingress.clone());
    let pull_all_documents_diagnostics =
        PullAllDocumentsDiagnosticHandler::spawn(runtime, ingress.clone());

    Handlers {
        completions: helix_view::handlers::completion::CompletionHandler::new(event_tx),
        signature_hints,
        auto_save,
        auto_reload,
        pkg,
        document_colors,
        lsp_feature_refresh,
        blame,
        word_index,
        pull_diagnostics,
        pull_all_documents_diagnostics,
    }
}

pub fn attach(
    editor: &helix_view::Editor,
    handlers: &Handlers,
    ingress: crate::runtime::RuntimeIngress,
) {
    helix_view::handlers::attach(editor, handlers);
    signature_help::attach(editor, handlers);
    auto_save::attach(editor, handlers);
    auto_reload::attach(editor, handlers);
    diagnostics::attach(editor, handlers, ingress.clone());
    snippet::attach(editor, handlers);
    document_colors::attach(editor, handlers, ingress.clone());
    lsp_features::attach(editor, handlers, ingress.clone());
    prompt::attach(editor, handlers, ingress.clone());
    blame::attach(editor, handlers);
    attach_assistant_hooks(editor);
}
