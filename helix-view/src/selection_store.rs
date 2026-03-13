use crate::{view::ViewPosition, ViewId};
use helix_core::Selection;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct SelectionStore {
    selections: HashMap<ViewId, Selection>,
    view_data: HashMap<ViewId, ViewData>,
}

impl SelectionStore {
    pub fn insert_selection(&mut self, view_id: ViewId, selection: Selection) {
        self.selections.insert(view_id, selection);
    }

    pub fn get_selection(&self, view_id: ViewId) -> Option<&Selection> {
        self.selections.get(&view_id)
    }

    pub fn selection(&self, view_id: ViewId) -> &Selection {
        &self.selections[&view_id]
    }

    pub fn selections(&self) -> &HashMap<ViewId, Selection> {
        &self.selections
    }

    pub fn selections_mut(&mut self) -> impl Iterator<Item = (&ViewId, &mut Selection)> {
        self.selections.iter_mut()
    }

    pub fn contains_selection(&self, view_id: ViewId) -> bool {
        self.selections.contains_key(&view_id)
    }

    pub fn remove_view(&mut self, view_id: ViewId) {
        self.selections.remove(&view_id);
        self.view_data.remove(&view_id);
    }

    pub fn view_data(&self, view_id: ViewId) -> &ViewData {
        self.view_data
            .get(&view_id)
            .expect("This should only be called after ensure_view_init")
    }

    pub fn ensure_view_data(&mut self, view_id: ViewId) {
        self.view_data.entry(view_id).or_default();
    }

    pub fn view_data_mut(&mut self, view_id: ViewId) -> &mut ViewData {
        self.view_data.entry(view_id).or_default()
    }

    pub fn view_data_values_mut(&mut self) -> impl Iterator<Item = &mut ViewData> {
        self.view_data.values_mut()
    }

    pub fn get_view_offset(&self, view_id: ViewId) -> Option<ViewPosition> {
        Some(self.view_data.get(&view_id)?.view_position)
    }

    pub fn view_offset(&self, view_id: ViewId) -> ViewPosition {
        self.view_data(view_id).view_position
    }

    pub fn set_view_offset(&mut self, view_id: ViewId, new_offset: ViewPosition) {
        self.view_data_mut(view_id).view_position = new_offset;
    }
}

#[derive(Debug, Default)]
pub struct ViewData {
    pub(crate) view_position: ViewPosition,
}
