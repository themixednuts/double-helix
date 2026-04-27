use crate::{view::ComponentViewState, DocumentId, ViewId};

use super::Editor;

impl Editor {
    pub fn allocate_view_id(&mut self) -> ViewId {
        let idx = self.next_virtual_view_idx;
        self.next_virtual_view_idx += 1;
        let raw = ((u32::MAX as u64) << 32) | (idx as u64);
        ViewId::from(slotmap::KeyData::from_ffi(raw))
    }

    pub fn component_view(&self, id: ViewId) -> Option<&ComponentViewState> {
        self.component_views.get(&id)
    }

    pub fn component_view_mut(&mut self, id: ViewId) -> Option<&mut ComponentViewState> {
        self.component_views.get_mut(&id)
    }

    pub fn ensure_component_view(
        &mut self,
        id: ViewId,
        doc: DocumentId,
    ) -> &mut ComponentViewState {
        self.track_component_surface(id, doc);
        self.component_views
            .entry(id)
            .and_modify(|view| view.doc = doc)
            .or_insert_with(|| ComponentViewState::new(id, doc))
    }
}
