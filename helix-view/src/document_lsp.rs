use helix_core::syntax;
use helix_core::text_annotations::InlineAnnotation;
use helix_core::{Assoc, ChangeSet};
use helix_event::{TaskController, TaskHandle};

#[derive(Debug, Clone, Default)]
pub struct DocumentColorSwatches {
    pub color_swatches: Vec<InlineAnnotation>,
    pub colors: Vec<syntax::Highlight>,
    pub color_swatches_padding: Vec<InlineAnnotation>,
}

#[derive(Debug, Default)]
pub struct DocumentLspState {
    previous_diagnostic_id: Option<String>,
    color_swatches: Option<DocumentColorSwatches>,
    color_swatch_controller: TaskController,
    pull_diagnostic_controller: TaskController,
}

impl DocumentLspState {
    pub fn restart_pull_diagnostics(&mut self) -> TaskHandle {
        self.pull_diagnostic_controller.restart()
    }

    pub fn cancel_pull_diagnostics(&mut self) -> bool {
        self.pull_diagnostic_controller.cancel()
    }

    pub fn previous_diagnostic_id(&self) -> Option<&str> {
        self.previous_diagnostic_id.as_deref()
    }

    pub fn set_previous_diagnostic_id(&mut self, previous_diagnostic_id: Option<String>) {
        self.previous_diagnostic_id = previous_diagnostic_id;
    }

    pub fn restart_color_swatches(&mut self) -> TaskHandle {
        self.color_swatch_controller.restart()
    }

    pub fn cancel_color_swatches(&mut self) -> bool {
        self.color_swatch_controller.cancel()
    }

    pub fn color_swatches(&self) -> Option<&DocumentColorSwatches> {
        self.color_swatches.as_ref()
    }

    pub fn clear_color_swatches(&mut self) {
        self.color_swatches = None;
    }

    pub fn set_color_swatches(&mut self, color_swatches: DocumentColorSwatches) {
        self.color_swatches = Some(color_swatches);
    }

    pub fn update_color_swatches(&mut self, changes: &ChangeSet) {
        let apply_color_swatch_changes = |annotations: &mut Vec<InlineAnnotation>| {
            changes.update_positions(
                annotations
                    .iter_mut()
                    .map(|annotation| (&mut annotation.char_idx, Assoc::After)),
            );
        };

        if let Some(DocumentColorSwatches {
            color_swatches,
            colors: _,
            color_swatches_padding,
        }) = &mut self.color_swatches
        {
            apply_color_swatch_changes(color_swatches);
            apply_color_swatch_changes(color_swatches_padding);
        }
    }
}
