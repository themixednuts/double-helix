use crate::{
    view::{AnyViewMut, AnyViewRef},
    Document, DocumentId, ViewId,
};

use super::Editor;

impl Editor {
    pub fn for_each_view_document(&self, mut f: impl FnMut(&crate::View, &Document)) {
        for (view, _) in self.tree.views() {
            let Some(doc) = self.document(view.doc) else {
                continue;
            };
            f(view, doc);
        }
    }

    pub fn with_view_doc_mut<R>(
        &mut self,
        view_id: ViewId,
        doc_id: DocumentId,
        f: impl FnOnce(&mut AnyViewMut<'_>, &mut Document) -> R,
    ) -> R {
        let Self {
            tree,
            documents,
            component_docs,
            component_views,
            ..
        } = self;

        let is_tree = tree.contains(view_id);
        let doc = documents
            .get_mut(&doc_id)
            .or_else(|| component_docs.get_mut(&doc_id))
            .expect("document not found in documents or component_docs");
        let mut view = if is_tree {
            AnyViewMut::Tree(tree.get_mut(view_id))
        } else {
            AnyViewMut::Component(
                component_views
                    .get_mut(&view_id)
                    .expect("component view not found"),
            )
        };
        f(&mut view, doc)
    }

    pub fn with_view<R>(&self, view_id: ViewId, f: impl FnOnce(AnyViewRef<'_>) -> R) -> R {
        f(AnyViewRef::from_editor(self, view_id))
    }

    pub fn with_view_doc<R>(
        &self,
        view_id: ViewId,
        doc_id: DocumentId,
        f: impl FnOnce(AnyViewRef<'_>, &Document) -> R,
    ) -> R {
        let doc = self
            .document(doc_id)
            .expect("document not found in documents or component_docs");
        self.with_view(view_id, |view| f(view, doc))
    }

    pub fn with_view_mut<R>(
        &mut self,
        view_id: ViewId,
        f: impl FnOnce(&mut AnyViewMut<'_>) -> R,
    ) -> R {
        let mut view = AnyViewMut::from_editor(self, view_id);
        f(&mut view)
    }

    pub fn with_surface<R>(
        &self,
        id: crate::collab::SurfaceId,
        f: impl FnOnce(crate::collab::surface::Ref<'_>) -> R,
    ) -> Result<R, crate::collab::surface::Missing> {
        let surface = self.surface_registry.require(id)?;
        let value = self.with_view_doc(surface.view, surface.doc, |view, doc| {
            f(view.as_surface_ref(doc))
        });
        Ok(value)
    }

    pub fn with_surface_mut<R>(
        &mut self,
        id: crate::collab::SurfaceId,
        f: impl FnOnce(crate::collab::surface::Mut<'_>) -> R,
    ) -> Result<R, crate::collab::surface::Missing> {
        let surface = self.surface_registry.require(id)?;
        let doc_id = surface.doc;
        let view_id = surface.view;
        let value =
            self.with_view_doc_mut(view_id, doc_id, |view, doc| f(view.as_surface_mut(doc)));
        Ok(value)
    }
}
