use std::path::PathBuf;

use helix_view::{editor::FileExplorerConfig, DocumentId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifiedBufferCheck {
    Prompt,
    Skip,
}

#[derive(Debug, Clone)]
pub enum FileExplorerCommand {
    ToggleSourceOption {
        option: crate::ui::file_options::FileSourceOption,
    },
    /// A queued file operation reached its terminal state. Refreshing and
    /// notifying happen only here, never at submission time.
    FileOperationCompleted {
        root: PathBuf,
        cursor: u32,
        select_path: Option<PathBuf>,
        result: Result<String, String>,
    },
    ApplyTree {
        root: PathBuf,
        generation: u64,
    },
    PreviewSelection {
        root: PathBuf,
        path: PathBuf,
        cursor: u32,
        generation: u64,
    },
    ApplyPreview {
        root: PathBuf,
        path: PathBuf,
        cursor: u32,
        generation: u64,
    },
    ApplyVcsSnapshot {
        root: PathBuf,
        snapshot: crate::ui::VcsSnapshot,
    },
    StartSearch {
        root: PathBuf,
        query: String,
        generation: u64,
        config: FileExplorerConfig,
    },
    ApplySearchResults {
        root: PathBuf,
        query: String,
        generation: u64,
        matches: Vec<PathBuf>,
    },
    ApplyCreate {
        root: PathBuf,
        cursor: u32,
        is_dir: bool,
        target: PathBuf,
        modified_buffer_check: ModifiedBufferCheck,
    },
    ApplyMove {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        destination: helix_view::editor::FileOperationDestination,
        modified_buffer_check: ModifiedBufferCheck,
    },
    PromptDelete {
        target: PathBuf,
        root: PathBuf,
        cursor: u32,
    },
    ApplyConfirmedDelete {
        target: PathBuf,
        root: PathBuf,
        cursor: u32,
        modified_buffer_check: ModifiedBufferCheck,
    },
    PromptCopy {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        prefill: String,
    },
    ApplyCopy {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        destination: helix_view::editor::FileOperationDestination,
        modified_buffer_check: ModifiedBufferCheck,
    },
    PromptSaveBefore {
        operation: String,
        documents: Vec<DocumentId>,
        continuation: Box<FileExplorerCommand>,
    },
}
