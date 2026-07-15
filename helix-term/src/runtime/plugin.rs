use std::path::PathBuf;

/// Lightweight editor signal resolved into a typed plugin event on the UI thread.
#[derive(Debug, Clone)]
pub enum PluginNotification {
    BufferOpen {
        document_id: helix_view::DocumentId,
        path: Option<PathBuf>,
    },
    BufferChanged {
        document_id: helix_view::DocumentId,
    },
    BufferClosed {
        document_id: helix_view::DocumentId,
    },
    SelectionChange {
        document_id: helix_view::DocumentId,
        path: Option<PathBuf>,
    },
    ModeChange {
        old_mode: String,
        new_mode: String,
    },
    KeyPress {
        key: String,
    },
    LspDiagnostic {
        document_id: helix_view::DocumentId,
        diagnostic_count: usize,
    },
}
