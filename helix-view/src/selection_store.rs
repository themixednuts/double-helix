use crate::{view::ViewPosition, ViewId};
use helix_core::Selection;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct SelectionStore {
    views: HashMap<ViewId, ViewState>,
}

impl SelectionStore {
    pub fn insert_selection(&mut self, view_id: ViewId, selection: Selection) {
        self.views
            .entry(view_id)
            .or_insert_with(|| ViewState::new(Selection::point(0)))
            .selection = selection;
    }

    pub fn get_selection(&self, view_id: ViewId) -> Option<&Selection> {
        self.views.get(&view_id).map(|view| &view.selection)
    }

    pub fn selection(&self, view_id: ViewId) -> &Selection {
        &self.views[&view_id].selection
    }

    pub fn selections(&self) -> Selections<'_> {
        Selections { views: &self.views }
    }

    pub fn selections_mut(&mut self) -> impl Iterator<Item = (&ViewId, &mut Selection)> {
        self.views
            .iter_mut()
            .map(|(view_id, view)| (view_id, &mut view.selection))
    }

    pub fn contains_selection(&self, view_id: ViewId) -> bool {
        self.views.contains_key(&view_id)
    }

    pub fn remove_view(&mut self, view_id: ViewId) {
        self.views.remove(&view_id);
    }

    pub fn view_data(&self, view_id: ViewId) -> &ViewData {
        &self.views[&view_id].data
    }

    pub fn ensure_view_data(&mut self, view_id: ViewId) {
        self.views
            .entry(view_id)
            .or_insert_with(|| ViewState::new(Selection::point(0)));
    }

    pub fn view_data_mut(&mut self, view_id: ViewId) -> &mut ViewData {
        &mut self
            .views
            .entry(view_id)
            .or_insert_with(|| ViewState::new(Selection::point(0)))
            .data
    }

    pub fn view_data_values_mut(&mut self) -> impl Iterator<Item = &mut ViewData> {
        self.views.values_mut().map(|view| &mut view.data)
    }

    pub fn get_view_offset(&self, view_id: ViewId) -> Option<ViewPosition> {
        Some(self.views.get(&view_id)?.data.view_position)
    }

    pub fn view_offset(&self, view_id: ViewId) -> ViewPosition {
        self.view_data(view_id).view_position
    }

    pub fn set_view_offset(&mut self, view_id: ViewId, new_offset: ViewPosition) {
        self.view_data_mut(view_id).view_position = new_offset;
    }
}

#[derive(Debug)]
struct ViewState {
    selection: Selection,
    data: ViewData,
}

impl ViewState {
    fn new(selection: Selection) -> Self {
        Self {
            selection,
            data: ViewData::default(),
        }
    }
}

pub struct Selections<'a> {
    views: &'a HashMap<ViewId, ViewState>,
}

impl<'a> Selections<'a> {
    pub fn keys(&self) -> impl Iterator<Item = &'a ViewId> {
        self.views.keys()
    }

    pub fn values(&self) -> impl Iterator<Item = &'a Selection> {
        self.views.values().map(|view| &view.selection)
    }

    pub fn get(&self, view_id: &ViewId) -> Option<&'a Selection> {
        self.views.get(view_id).map(|view| &view.selection)
    }

    pub fn contains_key(&self, view_id: &ViewId) -> bool {
        self.views.contains_key(view_id)
    }

    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct ViewData {
    pub(crate) view_position: ViewPosition,
}
