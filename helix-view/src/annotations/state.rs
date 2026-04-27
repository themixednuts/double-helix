//! Consolidated annotation state with automatic generation tracking.
//!
//! All per-view annotation data (inlay hints, jump labels, folds, plugin
//! annotations) lives in [`AnnotationState`]. Every mutation automatically
//! increments an internal generation counter, so the render cache can detect
//! changes without callers having to remember to bump anything.

use std::collections::HashMap;

use helix_core::text_folding::FoldContainer;

use crate::document::{DocumentInlayHints, PluginAnnotation};
use crate::ViewId;
use helix_core::text_annotations::Overlay;

/// All per-view annotation data for a document.
///
/// The generation counter is bumped automatically on every mutation.
/// Read access (`&self`) is free and doesn't bump.
#[derive(Default)]
pub struct AnnotationState {
    pub(crate) inlay_hints: HashMap<ViewId, DocumentInlayHints>,
    pub(crate) jump_labels: HashMap<ViewId, Box<[Overlay]>>,
    pub(crate) fold_containers: HashMap<ViewId, FoldContainer>,
    pub(crate) plugin_annotations: HashMap<ViewId, HashMap<String, Vec<PluginAnnotation>>>,
    pub(crate) presence_annotations: HashMap<ViewId, Vec<PluginAnnotation>>,
    gen: u64,
}

impl AnnotationState {
    /// Generation counter. Changes whenever any annotation state is mutated.
    pub fn gen(&self) -> u64 {
        self.gen
    }

    fn bump(&mut self) {
        self.gen = self.gen.wrapping_add(1);
    }

    // -- Inlay hints --------------------------------------------------------

    pub fn inlay_hints(&self, view_id: ViewId) -> Option<&DocumentInlayHints> {
        self.inlay_hints.get(&view_id)
    }

    pub fn set_inlay_hints(&mut self, view_id: ViewId, hints: DocumentInlayHints) {
        self.inlay_hints.insert(view_id, hints);
        self.bump();
    }

    pub fn reset_all_inlay_hints(&mut self) {
        if !self.inlay_hints.is_empty() {
            self.inlay_hints.clear();
            self.bump();
        }
    }

    /// Mutable access to all inlay hints (e.g. for updating positions during
    /// a transaction). Bumps generation.
    pub fn inlay_hints_mut(&mut self) -> impl Iterator<Item = &mut DocumentInlayHints> {
        self.bump();
        self.inlay_hints.values_mut()
    }

    // -- Jump labels --------------------------------------------------------

    pub fn jump_labels(&self, view_id: ViewId) -> Option<&[Overlay]> {
        self.jump_labels.get(&view_id).map(|b| &**b)
    }

    pub fn set_jump_labels(&mut self, view_id: ViewId, labels: Vec<Overlay>) {
        self.jump_labels.insert(view_id, labels.into_boxed_slice());
        self.bump();
    }

    pub fn remove_jump_labels(&mut self, view_id: ViewId) {
        if self.jump_labels.remove(&view_id).is_some() {
            self.bump();
        }
    }

    // -- Folds --------------------------------------------------------------

    pub fn fold_container(&self, view_id: ViewId) -> Option<&FoldContainer> {
        self.fold_containers.get(&view_id)
    }

    pub fn fold_container_mut(&mut self, view_id: ViewId) -> &mut FoldContainer {
        self.bump();
        self.fold_containers.entry(view_id).or_default()
    }

    pub fn insert_fold_container(&mut self, view_id: ViewId, container: FoldContainer) {
        self.fold_containers.insert(view_id, container);
        self.bump();
    }

    /// Mutable access to all fold containers (e.g. for updating positions
    /// during a transaction). Bumps generation.
    pub fn fold_containers_mut(&mut self) -> impl Iterator<Item = &mut FoldContainer> {
        self.bump();
        self.fold_containers.values_mut()
    }

    /// Mutable access to a specific fold container by view ID.
    /// Returns `None` if no container exists for this view. Bumps generation.
    pub fn fold_container_get_mut(&mut self, view_id: &ViewId) -> Option<&mut FoldContainer> {
        if self.fold_containers.contains_key(view_id) {
            self.bump();
            self.fold_containers.get_mut(view_id)
        } else {
            None
        }
    }

    // -- Plugin annotations -------------------------------------------------

    /// All plugin annotations for a view, flattened across plugin scopes.
    /// Returns `None` when no plugin has registered annotations for this view.
    pub fn plugin_annotations(&self, view_id: ViewId) -> Option<Vec<PluginAnnotation>> {
        let buckets = self.plugin_annotations.get(&view_id)?;
        if buckets.is_empty() {
            return None;
        }
        let mut merged = Vec::with_capacity(buckets.values().map(Vec::len).sum());
        for bucket in buckets.values() {
            merged.extend_from_slice(bucket);
        }
        Some(merged)
    }

    /// Replace annotations for a specific `plugin` scope. Other plugins' entries
    /// for the same view are left untouched. Empty `annotations` clears the scope.
    pub fn set_plugin_annotations(
        &mut self,
        view_id: ViewId,
        plugin: String,
        annotations: Vec<PluginAnnotation>,
    ) {
        let buckets = self.plugin_annotations.entry(view_id).or_default();
        if annotations.is_empty() {
            buckets.remove(&plugin);
            if buckets.is_empty() {
                self.plugin_annotations.remove(&view_id);
            }
        } else {
            buckets.insert(plugin, annotations);
        }
        self.bump();
    }

    /// Remove all plugin annotations registered by `plugin` across every view.
    pub fn clear_plugin_scope(&mut self, plugin: &str) {
        let mut changed = false;
        self.plugin_annotations.retain(|_, buckets| {
            if buckets.remove(plugin).is_some() {
                changed = true;
            }
            !buckets.is_empty()
        });
        if changed {
            self.bump();
        }
    }

    pub fn presence_annotations(&self, view_id: ViewId) -> Option<&Vec<PluginAnnotation>> {
        self.presence_annotations.get(&view_id)
    }

    pub fn set_presence_annotations(
        &mut self,
        view_id: ViewId,
        annotations: Vec<PluginAnnotation>,
    ) {
        self.presence_annotations.insert(view_id, annotations);
        self.bump();
    }

    // -- View cleanup -------------------------------------------------------

    /// Remove all annotation state for a view.
    pub fn remove_view(&mut self, view_id: ViewId) {
        let changed = self.inlay_hints.remove(&view_id).is_some()
            | self.jump_labels.remove(&view_id).is_some()
            | self.fold_containers.remove(&view_id).is_some()
            | self.plugin_annotations.remove(&view_id).is_some()
            | self.presence_annotations.remove(&view_id).is_some();
        if changed {
            self.bump();
        }
    }
}

impl std::fmt::Debug for AnnotationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnnotationState")
            .field("gen", &self.gen)
            .field("inlay_hints", &self.inlay_hints.len())
            .field("jump_labels", &self.jump_labels.len())
            .field("fold_containers", &self.fold_containers.len())
            .field("plugin_annotations", &self.plugin_annotations.len())
            .field("presence_annotations", &self.presence_annotations.len())
            .finish()
    }
}
