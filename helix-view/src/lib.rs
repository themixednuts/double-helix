#[macro_use]
pub mod macros;

pub mod annotations;
pub mod bench;
pub mod clipboard;
pub mod commands;
pub mod content_region;
pub mod document;
pub mod document_lsp;
pub mod edit_region;
pub mod editor;
pub mod engine;
pub mod events;
pub mod expansion;
pub mod file_bound;
pub mod file_watcher;
pub mod graphics;
pub mod gutter;
pub mod handlers;
pub mod history_state;
pub mod icons;
pub mod id;
pub mod info;
pub mod input;
pub mod keyboard;
pub mod keymap;
pub mod layout;
pub mod model;
pub mod presentation_state;
pub mod register;
pub mod revision;
pub mod selection_store;
pub mod session_state;
pub mod snippet_state;
pub mod statusline;
pub mod syntax_aware;
pub mod text_buffer;
pub mod theme;
pub mod traits;
pub mod tree;
pub mod vcs_state;
pub mod view;
pub mod viewport;

use std::num::NonZeroUsize;

/// Marker type for document IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentKind {}

/// Uses `NonZeroUsize` so `Option<DocumentId>` gets niche optimization.
pub type DocumentId = id::Id<DocumentKind, NonZeroUsize>;

/// The default document ID (1).
const DEFAULT_DOCUMENT_ID: DocumentId = DocumentId::new(NonZeroUsize::new(1).unwrap());

impl Default for DocumentId {
    fn default() -> DocumentId {
        DEFAULT_DOCUMENT_ID
    }
}

slotmap::new_key_type! {
    pub struct ViewId;
}

pub enum Align {
    Top,
    Center,
    Bottom,
}

pub fn align_view_in<V, D>(doc: &mut D, view: &V, align: Align)
where
    V: traits::TextViewport<D>,
    D: traits::FormattableText + traits::Selectable,
{
    let doc_text = doc.text().slice(..);
    let cursor = doc.selection(view.id()).primary().cursor(doc_text);
    let viewport = view.text_area(doc);
    let last_line_height = viewport.height.saturating_sub(1);
    let mut view_offset = view.view_offset(doc);

    let relative = match align {
        Align::Center => last_line_height / 2,
        Align::Top => 0,
        Align::Bottom => last_line_height,
    };

    let text_fmt = doc.text_format(viewport.width);
    (view_offset.anchor, view_offset.vertical_offset) = char_idx_at_visual_offset(
        doc_text,
        cursor,
        -(relative as isize),
        0,
        &text_fmt,
        &view.text_annotations(doc),
    );
    view.set_view_offset(doc, view_offset);
}

pub fn align_view(doc: &mut Document, view: &View, align: Align) {
    align_view_in(doc, view, align);
}

pub use document::Document;
pub use editor::Editor;
use helix_core::char_idx_at_visual_offset;
pub use revision::Revision;
pub use theme::Theme;
pub use view::View;
