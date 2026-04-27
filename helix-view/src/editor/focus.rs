use helix_core::{Tendril, Transaction};

use crate::{
    graphics::Rect,
    tree::{self, Dimension, Resize},
    view::ensure_cursor_in_view_center_in,
    DocumentId, ViewId,
};

use super::Editor;

impl Editor {
    pub fn resize(&mut self, area: Rect) {
        if self.tree.resize(area) {
            self._refresh();
        }
    }

    pub fn focus(&mut self, view_id: ViewId) {
        if self.tree.focus == view_id {
            return;
        }

        self.enter_normal_mode();
        let (cur_view_id, doc) = focused!(self);
        let view = self.tree.get_mut(cur_view_id);
        doc.append_changes_to_history(view);
        self.ensure_cursor_in_view(view_id);
        for (view, _focused) in self.tree.views_mut() {
            let doc = doc_mut!(self, &view.doc);
            view.sync_changes(doc);
        }

        let prev_id = std::mem::replace(&mut self.tree.focus, view_id);
        focused!(self).1.mark_as_focused();

        let focus_lost = self.tree.get(prev_id).doc;
        self.dispatch_document_focus_lost(focus_lost);
    }

    pub fn with_temporary_focus<T>(
        &mut self,
        view_id: ViewId,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous_view = self.tree.focus;
        self.tree.focus = view_id;
        let output = f(self);
        self.tree.focus = previous_view;
        if previous_view != view_id {
            self.ensure_cursor_in_view(previous_view);
        }
        output
    }

    pub fn focus_next(&mut self) {
        self.focus(self.tree.next());
    }

    pub fn focus_prev(&mut self) {
        self.focus(self.tree.prev());
    }

    pub fn focus_direction(&mut self, direction: tree::Direction) {
        let current_view = self.tree.focus;
        if let Some(id) = self.tree.find_split_in_direction(current_view, direction) {
            self.focus(id)
        }
    }

    pub fn swap_split_in_direction(&mut self, direction: tree::Direction) {
        self.tree.swap_split_in_direction(direction);
    }

    pub fn transpose_view(&mut self) {
        self.tree.transpose();
    }

    pub fn resize_buffer(&mut self, resize_type: Resize, dimension: Dimension) {
        self.tree
            .resize_buffer(resize_type, dimension, &self.config());
    }

    pub fn toggle_focus_window(&mut self) {
        self.tree.toggle_focus_window();
    }

    pub fn should_close(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn ensure_cursor_in_view(&mut self, id: ViewId) {
        let config = self.config();
        let view = self.tree.get(id);
        let doc = doc_mut!(self, &view.doc);
        view.ensure_cursor_in_view(doc, config.scrolloff)
    }

    pub fn ensure_all_cursors_in_view(&mut self) {
        let view_ids: Vec<_> = self.tree.views().map(|(view, _)| view.id).collect();
        for view_id in view_ids {
            self.ensure_cursor_in_view(view_id);
        }
    }

    pub fn focused_buffer_stats(&self) -> (usize, usize) {
        let view = self.tree.get(self.tree.focus);
        self.documents
            .get(&view.doc)
            .map(|doc| (doc.text().len_lines(), doc.text().len_bytes()))
            .unwrap_or((0, 0))
    }

    pub fn insert_into_focused_document(
        &mut self,
        text: &str,
    ) -> Option<(usize, usize, usize, usize, usize)> {
        let view_id = self.tree.focus;
        let view = self.tree.get(view_id);
        let doc_id = view.doc;
        let doc = self.documents.get_mut(&doc_id)?;
        let rope = doc.text();
        let selection = doc.selection(view_id).clone();
        let before_lines = rope.len_lines();
        let before_bytes = rope.len_bytes();
        let selection_count = selection.len();
        let transaction = Transaction::insert(rope, &selection, Tendril::from(text));
        doc.apply(&transaction, view_id);
        Some((
            selection_count,
            before_lines,
            before_bytes,
            doc.text().len_lines(),
            doc.text().len_bytes(),
        ))
    }

    pub fn undo_focused_document_to_line_limit(&mut self, limit: usize, max_steps: usize) -> usize {
        let view_id = self.tree.focus;
        let doc_id = self.tree.get(view_id).doc;
        let mut steps = 0;
        while steps < max_steps {
            let should_continue = {
                let Some(doc) = self.documents.get(&doc_id) else {
                    return steps;
                };
                doc.text().len_lines() > limit
            };
            if !should_continue {
                break;
            }
            let view = self.tree.get_mut(view_id);
            let Some(doc) = self.documents.get_mut(&doc_id) else {
                break;
            };
            if !doc.undo(view) {
                break;
            }
            steps += 1;
        }
        steps
    }

    pub fn get_synced_view_id(&mut self, id: DocumentId) -> ViewId {
        let current_view = view_mut!(self);
        let doc = self.documents.get_mut(&id).unwrap();
        if doc.selections().contains_key(&current_view.id) {
            if current_view.doc != id {
                current_view.sync_changes(doc);
            }
            current_view.id
        } else if let Some(view_id) = doc.selections().keys().next() {
            let view_id = *view_id;
            let view = self.tree.get_mut(view_id);
            view.sync_changes(doc);
            view_id
        } else {
            doc.ensure_view_init(current_view.id);
            current_view.id
        }
    }

    pub(super) fn jump_to(
        &mut self,
        view_id: ViewId,
        dest_doc_id: DocumentId,
        mut selection: helix_core::Selection,
    ) {
        if self.with_view(view_id, |view| view.is_tree()) {
            let view = view_mut!(self, view_id);
            let old_doc_id = view.doc;
            if old_doc_id != dest_doc_id {
                if let Some(transaction) =
                    self.with_view_doc_mut(view_id, dest_doc_id, |view, doc| {
                        view.changes_to_sync(doc)
                    })
                {
                    let new_doc = doc_mut!(self, &dest_doc_id);
                    let text = new_doc.text().slice(..);
                    selection = selection.map(transaction.changes()).ensure_invariants(text);
                }
                self.replace_document_in_view(view_id, dest_doc_id);
                self.dispatch_document_focus_lost(old_doc_id);
            }
            let (cur_view_id, doc) = focused!(self);
            doc.set_selection(cur_view_id, selection);
            let view = self.tree.get(cur_view_id);
            view.ensure_cursor_in_view_center(doc, self.config.load().scrolloff);
            return;
        }

        let scrolloff = self.config.load().scrolloff;
        self.with_view_doc_mut(view_id, dest_doc_id, |view, doc| {
            if view.doc_id() != dest_doc_id {
                return;
            }
            doc.set_selection(view_id, selection);
            ensure_cursor_in_view_center_in(view, doc, scrolloff);
        });
    }
}
