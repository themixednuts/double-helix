use std::{path::PathBuf, sync::Arc};

use crate::ui::picker::{CachedPreview, PickerInstanceId};
use helix_core::Syntax;

pub enum PickerCommand {
    RequestPreviewHighlight {
        picker: PickerInstanceId,
        path: PathBuf,
    },
    ApplyPreview {
        picker: PickerInstanceId,
        generation: u64,
        path: PathBuf,
        preview: CachedPreview,
    },
    ApplyPreviewSyntax {
        picker: PickerInstanceId,
        path: PathBuf,
        syntax: Syntax,
    },
    RunDynamicQuery {
        picker: PickerInstanceId,
        query: Arc<str>,
    },
    RefreshDynamicQuery {
        picker: PickerInstanceId,
    },
}

impl std::fmt::Debug for PickerCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestPreviewHighlight { picker, path } => f
                .debug_struct("RequestPreviewHighlight")
                .field("picker", picker)
                .field("path", path)
                .finish(),
            Self::ApplyPreview {
                picker,
                generation,
                path,
                ..
            } => f
                .debug_struct("ApplyPreview")
                .field("picker", picker)
                .field("generation", generation)
                .field("path", path)
                .finish_non_exhaustive(),
            Self::ApplyPreviewSyntax { picker, path, .. } => f
                .debug_struct("ApplyPreviewSyntax")
                .field("picker", picker)
                .field("path", path)
                .finish_non_exhaustive(),
            Self::RunDynamicQuery { picker, query } => f
                .debug_struct("RunDynamicQuery")
                .field("picker", picker)
                .field("query", query)
                .finish(),
            Self::RefreshDynamicQuery { picker } => f
                .debug_struct("RefreshDynamicQuery")
                .field("picker", picker)
                .finish(),
        }
    }
}
