use crate::{
    annotations::state::AnnotationState, document::DocumentInlayHints, document::PluginAnnotation,
    ViewId,
};
use helix_core::{
    editor_config::EditorConfig, indent::IndentStyle, text_annotations::Overlay,
    text_folding::FoldContainer,
};

#[derive(Debug)]
pub struct DocumentPresentationState {
    annotations: AnnotationState,
    inlay_hints_outdated: bool,
    restore_cursor: bool,
    indent_style: IndentStyle,
    editor_config: EditorConfig,
    is_welcome: bool,
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

    pub fn annotation_gen(&self) -> u64 {
        self.annotations.gen()
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

    pub fn inlay_hints(&self, view_id: ViewId) -> Option<&DocumentInlayHints> {
        self.annotations.inlay_hints(view_id)
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

    pub fn fold_container_mut(&mut self, view_id: ViewId) -> &mut FoldContainer {
        self.annotations.fold_container_mut(view_id)
    }

    pub fn fold_container_get_mut(&mut self, view_id: &ViewId) -> Option<&mut FoldContainer> {
        self.annotations.fold_container_get_mut(view_id)
    }

    pub fn fold_containers_mut(&mut self) -> impl Iterator<Item = &mut FoldContainer> {
        self.annotations.fold_containers_mut()
    }

    pub fn plugin_annotations(&self, view_id: ViewId) -> Option<&Vec<PluginAnnotation>> {
        self.annotations.plugin_annotations(view_id)
    }

    pub fn set_plugin_annotations(&mut self, view_id: ViewId, annotations: Vec<PluginAnnotation>) {
        self.annotations
            .set_plugin_annotations(view_id, annotations);
    }
}
