use crate::{DocumentId, View, ViewId};

use super::{Editor, EditorEvent};

impl Editor {
    pub(crate) fn bind_view_redraw(&self, view: &mut View) {
        view.diagnostics_handler.bind_runtime(self.runtime.clone());
        view.diagnostics_handler
            .bind_redraw(self.frame_gate.handle());
    }

    pub(crate) fn track_tree_surface(
        &mut self,
        view_id: ViewId,
    ) -> Option<crate::collab::SurfaceId> {
        if !self.tree.contains(view_id) {
            self.surface_registry.remove_view(view_id);
            return None;
        }

        let view = self.tree.get(view_id);
        Some(self.surface_registry.track(
            crate::collab::surface::kind::EDITOR,
            crate::collab::surface::Role::Editor,
            view.id,
            view.doc,
        ))
    }

    pub(super) fn track_component_surface(
        &mut self,
        view_id: ViewId,
        doc_id: DocumentId,
    ) -> crate::collab::SurfaceId {
        self.surface_registry.track(
            crate::collab::surface::kind::ASSISTANT_THREAD,
            crate::collab::surface::Role::Auxiliary,
            view_id,
            doc_id,
        )
    }

    pub fn capture_current_surface(
        &self,
        capture: crate::collab::surface::Capture,
    ) -> Option<crate::assistant::context::Kind> {
        let view = self.tree.get(self.tree.focus);
        let doc = self.document(view.doc)?;
        crate::collab::surface::Context::capture(
            &crate::collab::surface::Ref::Tree { view, doc },
            self,
            capture,
        )
    }

    pub fn pause_current_surface(&self, event: &EditorEvent) -> Option<crate::collab::FollowPause> {
        let view = self.tree.get(self.tree.focus);
        let doc = self.document(view.doc)?;
        crate::collab::surface::PauseFollow::pause(
            &crate::collab::surface::Ref::Tree { view, doc },
            event,
        )
    }
}
