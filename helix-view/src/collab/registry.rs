use std::collections::BTreeMap;
use std::sync::Arc;

use super::surface::{Factory, Kind, Missing, Open, OpenError, Surface};
use super::SurfaceId;

#[derive(Default)]
pub struct Registry {
    next: u64,
    factories: BTreeMap<Kind, Arc<dyn Factory>>,
    surfaces: BTreeMap<SurfaceId, Surface>,
    by_view: BTreeMap<crate::ViewId, SurfaceId>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("factories", &self.factories.len())
            .field("surfaces", &self.surfaces.len())
            .finish()
    }
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        Self {
            next: self.next,
            factories: self.factories.clone(),
            surfaces: self.surfaces.clone(),
            by_view: self.by_view.clone(),
        }
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            next: 1,
            ..Self::default()
        }
    }

    pub fn register(&mut self, factory: Arc<dyn Factory>) {
        self.factories.insert(factory.kind(), factory);
    }

    pub fn open(
        &mut self,
        editor: &mut crate::Editor,
        kind: &Kind,
        open: Open,
    ) -> Result<SurfaceId, OpenError> {
        let factory = self
            .factories
            .get(kind)
            .ok_or_else(|| OpenError::UnknownKind(kind.clone()))?
            .clone();
        factory.open(editor, open).map_err(OpenError::Factory)
    }

    pub fn track(
        &mut self,
        kind: Kind,
        role: super::surface::Role,
        view: crate::ViewId,
        doc: crate::DocumentId,
    ) -> SurfaceId {
        if let Some(id) = self.by_view.get(&view).copied() {
            self.surfaces.insert(
                id,
                Surface {
                    id,
                    kind,
                    role,
                    view,
                    doc,
                },
            );
            return id;
        }

        let id = SurfaceId::new(std::num::NonZeroU64::new(self.next).expect("surface id non-zero"));
        self.next += 1;
        self.surfaces.insert(
            id,
            Surface {
                id,
                kind,
                role,
                view,
                doc,
            },
        );
        self.by_view.insert(view, id);
        id
    }

    pub fn get(&self, id: SurfaceId) -> Option<&Surface> {
        self.surfaces.get(&id)
    }

    pub fn get_by_view(&self, view: crate::ViewId) -> Option<SurfaceId> {
        self.by_view.get(&view).copied()
    }

    pub fn surfaces(&self) -> impl Iterator<Item = &Surface> {
        self.surfaces.values()
    }

    pub fn require(&self, id: SurfaceId) -> Result<&Surface, Missing> {
        self.get(id).ok_or(Missing { id })
    }

    pub fn remove_view(&mut self, view: crate::ViewId) {
        if let Some(id) = self.by_view.remove(&view) {
            self.surfaces.remove(&id);
        }
    }
}
