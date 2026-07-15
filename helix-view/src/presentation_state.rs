use crate::{
    annotations::state::AnnotationState, document::DocumentInlayHints, document::PluginAnnotation,
    ViewId,
};
use helix_core::{
    editor_config::EditorConfig, indent::IndentStyle, text_annotations::Overlay,
    text_folding::FoldContainer,
};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnotationSnapshot {
    pub(crate) revision: crate::Revision,
    pub(crate) inlay_hints_outdated: bool,
    pub(crate) inlay_hint_views: usize,
    pub(crate) jump_label_views: usize,
    pub(crate) fold_views: usize,
    pub(crate) plugin_annotation_views: usize,
}

impl AnnotationSnapshot {
    pub const fn new(revision: crate::Revision) -> Self {
        Self {
            revision,
            inlay_hints_outdated: false,
            inlay_hint_views: 0,
            jump_label_views: 0,
            fold_views: 0,
            plugin_annotation_views: 0,
        }
    }

    pub const fn with_state(
        revision: crate::Revision,
        inlay_hints_outdated: bool,
        inlay_hint_views: usize,
        jump_label_views: usize,
        fold_views: usize,
        plugin_annotation_views: usize,
    ) -> Self {
        Self {
            revision,
            inlay_hints_outdated,
            inlay_hint_views,
            jump_label_views,
            fold_views,
            plugin_annotation_views,
        }
    }

    pub const fn revision(self) -> crate::Revision {
        self.revision
    }

    pub const fn inlay_hints_outdated(self) -> bool {
        self.inlay_hints_outdated
    }
}

#[derive(Debug)]
pub struct DocumentPresentationState {
    annotations: AnnotationState,
    inlay_hints_outdated: bool,
    restore_cursor: bool,
    indent_style: IndentStyle,
    editor_config: EditorConfig,
    is_welcome: bool,
    persistent_scratch: bool,
}

impl Default for DocumentPresentationState {
    fn default() -> Self {
        Self {
            annotations: AnnotationState::default(),
            inlay_hints_outdated: false,
            restore_cursor: false,
            indent_style: IndentStyle::Tabs,
            editor_config: EditorConfig::default(),
            is_welcome: false,
            persistent_scratch: false,
        }
    }
}

impl DocumentPresentationState {
    pub fn restore_cursor(&self) -> bool {
        self.restore_cursor
    }

    pub fn mark_restore_cursor(&mut self) {
        self.restore_cursor = true;
    }

    pub fn clear_restore_cursor(&mut self) {
        self.restore_cursor = false;
    }

    pub fn is_welcome(&self) -> bool {
        self.is_welcome
    }

    pub fn set_welcome(&mut self, is_welcome: bool) {
        self.is_welcome = is_welcome;
    }

    pub fn is_persistent_scratch(&self) -> bool {
        self.persistent_scratch
    }

    pub fn set_persistent_scratch(&mut self, persistent_scratch: bool) {
        self.persistent_scratch = persistent_scratch;
    }

    pub fn indent_style(&self) -> IndentStyle {
        self.indent_style
    }

    pub fn set_indent_style(&mut self, indent_style: IndentStyle) {
        self.indent_style = indent_style;
    }

    pub fn editor_config(&self) -> &EditorConfig {
        &self.editor_config
    }

    pub fn set_editor_config(&mut self, editor_config: EditorConfig) {
        self.editor_config = editor_config;
    }

    pub fn inlay_hints_outdated(&self) -> bool {
        self.inlay_hints_outdated
    }

    pub fn mark_inlay_hints_outdated(&mut self) {
        self.inlay_hints_outdated = true;
    }

    pub fn clear_inlay_hints_outdated(&mut self) {
        self.inlay_hints_outdated = false;
    }

    pub fn remove_view(&mut self, view_id: ViewId) {
        self.annotations.remove_view(view_id);
    }

    pub fn set_inlay_hints(&mut self, view_id: ViewId, inlay_hints: DocumentInlayHints) {
        self.annotations.set_inlay_hints(view_id, inlay_hints);
    }

    fn annotation_gen(&self) -> u64 {
        self.annotations.gen()
    }

    pub fn annotation_snapshot(&self) -> AnnotationSnapshot {
        AnnotationSnapshot::with_state(
            crate::Revision::from(self.annotation_gen()),
            self.inlay_hints_outdated,
            self.annotations.inlay_hints.len(),
            self.annotations.jump_labels.len(),
            self.annotations.fold_containers.len(),
            self.annotations.plugin_annotations.len() + self.annotations.presence_annotations.len(),
        )
    }

    pub fn set_jump_labels(&mut self, view_id: ViewId, labels: Vec<Overlay>) {
        self.annotations.set_jump_labels(view_id, labels);
    }

    pub fn remove_jump_labels(&mut self, view_id: ViewId) {
        self.annotations.remove_jump_labels(view_id);
    }

    pub fn jump_labels(&self, view_id: ViewId) -> Option<&[Overlay]> {
        self.annotations.jump_labels(view_id)
    }

    pub fn jump_labels_snapshot(&self, view_id: ViewId) -> Option<Arc<[Overlay]>> {
        self.annotations.jump_labels_snapshot(view_id)
    }

    pub fn inlay_hints(&self, view_id: ViewId) -> Option<&DocumentInlayHints> {
        self.annotations.inlay_hints(view_id)
    }

    pub fn inlay_hints_snapshot(&self, view_id: ViewId) -> Option<Arc<DocumentInlayHints>> {
        self.annotations.inlay_hints_snapshot(view_id)
    }

    pub fn reset_all_inlay_hints(&mut self) {
        self.annotations.reset_all_inlay_hints();
    }

    pub fn inlay_hints_mut(&mut self) -> impl Iterator<Item = &mut DocumentInlayHints> {
        self.annotations.inlay_hints_mut()
    }

    pub fn insert_fold_container(&mut self, view_id: ViewId, container: FoldContainer) {
        self.annotations.insert_fold_container(view_id, container);
    }

    pub fn fold_container(&self, view_id: ViewId) -> Option<&FoldContainer> {
        self.annotations.fold_container(view_id)
    }

    pub fn fold_container_snapshot(&self, view_id: ViewId) -> Option<Arc<FoldContainer>> {
        self.annotations.fold_container_snapshot(view_id)
    }

    pub fn fold_container_mut(&mut self, view_id: ViewId) -> &mut FoldContainer {
        self.annotations.fold_container_mut(view_id)
    }

    pub fn fold_container_get_mut(&mut self, view_id: &ViewId) -> Option<&mut FoldContainer> {
        self.annotations.fold_container_get_mut(view_id)
    }

    pub fn fold_containers_mut(&mut self) -> impl Iterator<Item = &mut FoldContainer> {
        self.annotations.fold_containers_mut()
    }

    pub fn visual_annotations(
        &self,
        view_id: ViewId,
    ) -> Option<std::sync::Arc<[PluginAnnotation]>> {
        self.annotations.visual_annotations(view_id)
    }

    pub fn set_plugin_annotations(
        &mut self,
        view_id: ViewId,
        plugin: String,
        annotations: Vec<PluginAnnotation>,
    ) {
        self.annotations
            .set_plugin_annotations(view_id, plugin, annotations);
    }

    pub fn clear_plugin_annotations(&mut self, plugin: &str) {
        self.annotations.clear_plugin_scope(plugin);
    }

    pub fn presence_annotations(&self, view_id: ViewId) -> Option<&Vec<PluginAnnotation>> {
        self.annotations.presence_annotations(view_id)
    }

    pub fn set_presence_annotations(
        &mut self,
        view_id: ViewId,
        annotations: Vec<PluginAnnotation>,
    ) {
        self.annotations
            .set_presence_annotations(view_id, annotations);
    }
}
