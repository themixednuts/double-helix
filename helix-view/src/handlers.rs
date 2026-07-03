use completion::{CompletionEvent, CompletionHandler};
use helix_runtime::{channel, send_blocking, Sender};

use crate::handlers::lsp::SignatureHelpInvoked;
use crate::{DocumentId, Editor, ViewId};

pub mod completion;
pub mod dap;
pub mod diagnostics;
pub mod lsp;
pub mod word_index;
mod workspace_edit;

#[derive(Debug)]
pub enum AutoSaveEvent {
    DocumentChanged { save_after: u64 },
    LeftInsertMode,
}

#[derive(Debug)]
pub struct BlameEvent {
    /// The path for which we request blame
    pub path: std::path::PathBuf,
    /// Document for which the blame is requested
    pub doc_id: DocumentId,
    /// If this field is set, when we obtain the blame for the file we will
    /// show blame for this line in the status line
    pub line: Option<u32>,
}

#[derive(Debug)]
pub enum AutoReloadEvent {
    /// A watched file changed on disk (from notify watcher).
    FileChanged {
        path: std::path::PathBuf,
        doc_ids: Vec<crate::DocumentId>,
    },
    LeftInsertMode,
}

pub struct Handlers {
    // only public because most of the actual implementation is in helix-term right now :/
    pub completions: CompletionHandler,
    pub signature_hints: Sender<lsp::SignatureHelpEvent>,
    pub auto_save: Sender<AutoSaveEvent>,
    pub auto_reload: Sender<AutoReloadEvent>,
    pub document_colors: Sender<lsp::DocumentColorsEvent>,
    pub lsp_feature_refresh: Sender<lsp::LspFeatureRefreshEvent>,
    pub blame: Sender<BlameEvent>,
    pub word_index: word_index::Handler,
    pub pull_diagnostics: Sender<lsp::PullDiagnosticsEvent>,
    pub pull_all_documents_diagnostics: Sender<lsp::PullAllDocumentsDiagnosticsEvent>,
}

impl Handlers {
    /// Create a dummy `Handlers` for headless testing.
    ///
    /// All senders point to immediately-dropped receivers, so any send will
    /// fail silently.  This is fine for tests that don't exercise async
    /// handler behaviour.
    pub fn dummy() -> Self {
        let (comp_tx, _) = channel(1);
        let (sig_tx, _) = channel(1);
        let (auto_save_tx, _) = channel(1);
        let (auto_reload_tx, _) = channel(1);
        let (doc_colors_tx, _) = channel(1);
        let (lsp_feature_refresh_tx, _) = channel(1);
        let (blame_tx, _) = channel(1);
        let (pull_diag_tx, _) = channel(1);
        let (pull_all_diag_tx, _) = channel(1);
        Self {
            completions: CompletionHandler::new(comp_tx),
            signature_hints: sig_tx,
            auto_save: auto_save_tx,
            auto_reload: auto_reload_tx,
            document_colors: doc_colors_tx,
            lsp_feature_refresh: lsp_feature_refresh_tx,
            blame: blame_tx,
            word_index: word_index::Handler::dummy(),
            pull_diagnostics: pull_diag_tx,
            pull_all_documents_diagnostics: pull_all_diag_tx,
        }
    }

    /// Manually trigger completion (c-x)
    pub fn trigger_completions(&self, trigger_pos: usize, doc: DocumentId, view: ViewId) {
        self.completions.event(CompletionEvent::ManualTrigger {
            cursor: trigger_pos,
            doc,
            view,
        });
    }

    pub fn trigger_signature_help(&self, invocation: SignatureHelpInvoked, editor: &Editor) {
        let event = match invocation {
            SignatureHelpInvoked::Automatic => {
                if !editor.config().lsp.auto_signature_help {
                    return;
                }
                lsp::SignatureHelpEvent::Trigger
            }
            SignatureHelpInvoked::Manual => lsp::SignatureHelpEvent::Invoked,
        };
        send_blocking(&self.signature_hints, event)
    }

    pub fn word_index(&self) -> &word_index::WordIndex {
        &self.word_index.index
    }
}

pub fn attach(editor: &Editor, handlers: &Handlers) {
    lsp::attach(editor, handlers);
    word_index::attach(editor, handlers);
}
