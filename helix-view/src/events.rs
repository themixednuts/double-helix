use helix_core::{ChangeSet, Rope};
use helix_lsp::LanguageServerId;

use crate::{editor::Config, Document, DocumentId, Editor, ViewId};

pub struct DocumentDidOpen<'a> {
    pub editor: &'a mut Editor,
    pub doc: DocumentId,
    pub path: &'a std::path::PathBuf,
}

pub struct DocumentDidChange<'a> {
    pub doc: &'a mut Document,
    pub view: ViewId,
    pub old_text: &'a Rope,
    pub changes: &'a ChangeSet,
    pub ghost_transaction: bool,
}

pub struct EditorConfigDidChange<'a> {
    pub old_config: &'a Config,
    pub editor: &'a mut Editor,
}

pub struct DocumentDidClose<'a> {
    pub editor: &'a mut Editor,
    pub doc: Document,
}

pub struct SelectionDidChange<'a> {
    pub doc: &'a mut Document,
    pub view: ViewId,
}

pub struct DiagnosticsDidChange<'a> {
    pub editor: &'a mut Editor,
    pub doc: DocumentId,
    pub diagnostic_count: usize,
}

pub struct DocumentFocusLost<'a> {
    pub editor: &'a mut Editor,
    pub doc: DocumentId,
}

pub struct LanguageServerInitialized<'a> {
    pub editor: &'a mut Editor,
    pub server_id: LanguageServerId,
}

pub struct LanguageServerExited<'a> {
    pub editor: &'a mut Editor,
    pub server_id: LanguageServerId,
}

pub struct ConfigDidChange<'a> {
    pub editor: &'a mut Editor,
    pub old: &'a Config,
    pub new: &'a Config,
}
