use crate::ViewId;

use super::Editor;

impl Editor {
    pub fn jump_forward(&mut self, view_id: ViewId, count: usize) {
        let jump = self.with_view_mut(view_id, |view| view.jumps_mut().forward(count).cloned());
        if let Some((doc_id, selection)) = jump {
            self.jump_to(view_id, doc_id, selection);
        }
    }

    pub fn jump_backward(&mut self, view_id: ViewId, count: usize) {
        let current_doc_id = self.with_view_mut(view_id, |view| view.doc_id());
        let jump = self.with_view_doc_mut(view_id, current_doc_id, |view, doc| {
            view.jumps_mut().backward(view_id, doc, count).cloned()
        });
        if let Some((doc_id, selection)) = jump {
            self.jump_to(view_id, doc_id, selection);
        }
    }
}
