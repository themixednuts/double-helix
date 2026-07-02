use std::{path::Path, sync::Arc, time::Duration};

use helix_runtime::{Clock, Work};

use crate::runtime::{ui::command::PickerCommand, UiCommand};

use super::{PickerInstanceId, SharedIngress};

pub(super) struct PreviewHighlightHandler {
    picker: PickerInstanceId,
    trigger: Option<Arc<Path>>,
    debouncer: crate::runtime::RuntimeUiDebouncer,
}

impl PreviewHighlightHandler {
    pub(super) fn new(
        picker: PickerInstanceId,
        work: Work,
        clock: Clock,
        ingress: SharedIngress,
    ) -> Self {
        Self {
            picker,
            trigger: None,
            debouncer: crate::runtime::RuntimeUiDebouncer::new(
                Duration::from_millis(150),
                work,
                clock,
                (*ingress).clone(),
            ),
        }
    }

    pub(super) fn request(&mut self, path: Arc<Path>) {
        if self
            .trigger
            .as_ref()
            .is_some_and(|trigger| trigger == &path)
        {
            return;
        };

        self.trigger = Some(path.clone());
        self.debouncer
            .send(UiCommand::Picker(PickerCommand::RequestPreviewHighlight {
                picker: self.picker,
                path: path.to_path_buf(),
            }));
    }

    pub(super) fn cancel(&self) {
        self.debouncer.cancel();
    }
}

impl Drop for PreviewHighlightHandler {
    fn drop(&mut self) {
        self.cancel();
    }
}
