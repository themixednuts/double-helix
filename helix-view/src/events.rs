use helix_core::{ChangeSet, Rope};
use helix_lsp::LanguageServerId;

use crate::{editor::Config, Document, DocumentId, Editor, ViewId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentChangeOrigin {
    Local,
    Collaboration,
}

pub struct DocumentDidOpen<'a> {
    pub editor: &'a mut Editor,
    pub doc: DocumentId,
    pub location: &'a crate::file_bound::DocumentLocation,
}

pub struct DocumentDidChange<'a> {
    pub doc: &'a mut Document,
    pub view: Option<ViewId>,
    pub old_text: &'a Rope,
    pub changes: &'a ChangeSet,
    pub ghost_transaction: bool,
    pub origin: DocumentChangeOrigin,
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

pub struct DocumentLanguageServersDidChange<'a> {
    pub editor: &'a mut Editor,
    pub doc: DocumentId,
}

pub struct ConfigDidChange<'a> {
    pub editor: &'a mut Editor,
    pub old: &'a Config,
    pub new: &'a Config,
}
