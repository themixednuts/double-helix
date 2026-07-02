use std::{num::NonZeroUsize, path::Path};

use slotmap::Key;

use crate::{document::DocumentRedrawHandle, Document, DocumentId, ViewId};

use super::Editor;

impl Editor {
    pub(crate) fn new_document(&mut self, mut doc: Document) -> DocumentId {
        let id = self.next_document_id;
        self.next_document_id = DocumentId::new(unsafe {
            NonZeroUsize::new_unchecked(self.next_document_id.value().get() + 1)
        });
        doc.bind_lifecycle(self.lifecycle.clone());
        doc.id = id;
        self.documents.insert(id, doc);

        self.save_locks.insert(id, Default::default());

        id
    }

    pub(crate) fn restore_document(&mut self, mut doc: Document) -> DocumentId {
        let id = doc.id;
        debug_assert!(
            !self.documents.contains_key(&id) && !self.component_docs.contains_key(&id),
            "restored document id must not already be live"
        );
        doc.bind_lifecycle(self.lifecycle.clone());
        self.documents.insert(id, doc);

        self.save_locks.insert(id, Default::default());

        id
    }

    pub fn new_component_doc(&mut self, mut doc: Document) -> DocumentId {
        let id = self.next_document_id;
        self.next_document_id = DocumentId::new(unsafe {
            NonZeroUsize::new_unchecked(self.next_document_id.value().get() + 1)
        });
        doc.bind_lifecycle(self.lifecycle.clone());
        doc.id = id;
        self.component_docs.insert(id, doc);
        id
    }

    pub fn document_redraw_handle(&self) -> DocumentRedrawHandle {
        DocumentRedrawHandle::new(self.frame_gate.handle())
    }

    #[inline]
    pub fn document(&self, id: DocumentId) -> Option<&Document> {
        self.documents
            .get(&id)
            .or_else(|| self.component_docs.get(&id))
    }

    pub fn focused_document_id(&self) -> DocumentId {
        self.tree.get(self.tree.focus).doc
    }

    pub fn focused_view_id(&self) -> ViewId {
        self.tree.focus
    }

    pub fn focused_document(&self) -> Option<&Document> {
        self.document(self.focused_document_id())
    }

    pub fn focused_document_mut(&mut self) -> Option<&mut Document> {
        let doc_id = self.focused_document_id();
        self.document_mut(doc_id)
    }

    pub fn is_focused_document(&self, doc_id: DocumentId) -> bool {
        self.focused_document_id() == doc_id
    }

    #[inline]
    pub fn document_mut(&mut self, id: DocumentId) -> Option<&mut Document> {
        self.documents
            .get_mut(&id)
            .or_else(|| self.component_docs.get_mut(&id))
    }

    #[inline]
    pub fn documents(&self) -> impl Iterator<Item = &Document> {
        self.documents.values()
    }

    #[inline]
    pub fn document_count(&self) -> usize {
        self.documents.len()
    }

    #[inline]
    pub fn has_multiple_documents(&self) -> bool {
        self.document_count() > 1
    }

    #[inline]
    pub fn document_ids(&self) -> impl Iterator<Item = DocumentId> + '_ {
        self.documents.keys().copied()
    }

    #[inline]
    pub fn contains_document(&self, id: DocumentId) -> bool {
        self.documents.contains_key(&id) || self.component_docs.contains_key(&id)
    }

    #[inline]
    pub fn contains_view(&self, id: ViewId) -> bool {
        self.tree.contains(id)
    }

    pub fn view_document_id(&self, id: ViewId) -> Option<DocumentId> {
        self.tree.try_get(id).map(|view| view.doc)
    }

    pub fn view_ids(&self) -> impl Iterator<Item = ViewId> + '_ {
        self.tree.views().map(|(view, _)| view.id)
    }

    pub fn focused_document_handle(&self) -> usize {
        self.focused_document_id().value().get()
    }

    pub fn focused_view_handle(&self) -> u64 {
        self.focused_view_id().data().as_ffi()
    }

    pub fn document_handle(&self, id: DocumentId) -> usize {
        id.value().get()
    }

    pub fn view_handle(&self, id: ViewId) -> u64 {
        id.data().as_ffi()
    }

    pub fn language_server_client_ids(&self) -> impl Iterator<Item = String> + '_ {
        self.language_servers
            .iter_clients()
            .map(|client| client.id().to_string())
    }

    pub fn language_server_client_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.language_servers
            .iter_clients()
            .map(|client| client.name())
    }

    pub fn read_register(
        &self,
        name: char,
    ) -> Option<impl Iterator<Item = std::borrow::Cow<'_, str>> + '_> {
        self.registers.read(name, self)
    }

    pub fn write_register(&mut self, name: char, values: Vec<String>) -> anyhow::Result<()> {
        self.registers.write(name, values)
    }

    pub fn focused_cursor_position(&self) -> Option<(usize, usize)> {
        let view_id = self.focused_view_id();
        let doc = self.focused_document()?;
        let cursor = doc
            .selection(view_id)
            .primary()
            .cursor(doc.text().slice(..));
        let row = doc.text().char_to_line(cursor);
        let col = cursor - doc.text().line_to_char(row);
        Some((row, col))
    }

    pub fn set_focused_cursor_position(&mut self, row: usize, col: usize) -> bool {
        let view_id = self.focused_view_id();
        let Some(doc) = self.focused_document_mut() else {
            return false;
        };

        let text = doc.text();
        let row = row.min(text.len_lines().saturating_sub(1));
        let offset = text.line_to_char(row) + col.min(text.line(row).len_chars());
        doc.set_selection(view_id, helix_core::Selection::point(offset));
        true
    }

    pub fn focused_selection_ranges(&self) -> Vec<(usize, usize)> {
        let Some(doc) = self.focused_document() else {
            return Vec::new();
        };
        doc.selection(self.focused_view_id())
            .iter()
            .map(|range| (range.anchor, range.head))
            .collect()
    }

    pub fn set_focused_selection_ranges(&mut self, ranges: Vec<(usize, usize)>) -> bool {
        if ranges.is_empty() {
            return false;
        }

        let view_id = self.focused_view_id();
        let Some(doc) = self.focused_document_mut() else {
            return false;
        };

        let selection = helix_core::Selection::new(
            ranges
                .into_iter()
                .map(|(anchor, head)| helix_core::Range::new(anchor, head))
                .collect(),
            0,
        );
        doc.set_selection(view_id, selection);
        true
    }

    pub fn select_all_in_focused_document(&mut self) -> bool {
        let view_id = self.focused_view_id();
        let Some(doc) = self.focused_document_mut() else {
            return false;
        };
        let end = doc.text().len_chars();
        doc.set_selection(view_id, helix_core::Selection::single(0, end));
        true
    }

    pub fn undo_focused_document(&mut self) -> Option<bool> {
        let view_id = self.focused_view_id();
        let doc_id = self.focused_document_id();
        if !self.contains_document(doc_id) || !self.contains_view(view_id) {
            return None;
        }
        Some(self.with_view_doc_mut(view_id, doc_id, |view, doc| doc.undo(view)))
    }

    pub fn redo_focused_document(&mut self) -> Option<bool> {
        let view_id = self.focused_view_id();
        let doc_id = self.focused_document_id();
        if !self.contains_document(doc_id) || !self.contains_view(view_id) {
            return None;
        }
        Some(self.with_view_doc_mut(view_id, doc_id, |view, doc| doc.redo(view)))
    }

    pub fn close_focused_view(&mut self) {
        let view_id = self.focused_view_id();
        self.close(view_id);
    }

    pub fn edit_focused_document_if<R>(
        &mut self,
        doc_id: DocumentId,
        f: impl FnOnce(&mut Document, ViewId) -> R,
    ) -> Option<R> {
        if !self.is_focused_document(doc_id) {
            return None;
        }

        let view_id = self.focused_view_id();
        let doc = self.focused_document_mut()?;
        Some(f(doc, view_id))
    }

    #[inline]
    pub fn has_single_view(&self) -> bool {
        self.tree.views().count() == 1
    }

    #[inline]
    pub fn documents_mut(&mut self) -> impl Iterator<Item = &mut Document> {
        self.documents.values_mut()
    }

    pub fn document_by_path<P: AsRef<Path>>(&self, path: P) -> Option<&Document> {
        self.documents()
            .find(|doc| doc.path().map(|p| p == path.as_ref()).unwrap_or(false))
    }

    pub fn document_by_path_mut<P: AsRef<Path>>(&mut self, path: P) -> Option<&mut Document> {
        self.documents_mut()
            .find(|doc| doc.path().map(|p| p == path.as_ref()).unwrap_or(false))
    }
}
