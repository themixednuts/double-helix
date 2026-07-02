use std::path::PathBuf;

use helix_vcs::FileChange;
use helix_view::DocumentId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifiedBufferCheck {
    Prompt,
    Skip,
}

#[derive(Debug, Clone)]
pub enum FileExplorerCommand {
    RefreshPanel {
        root: PathBuf,
        cursor: u32,
    },
    PreviewSelection {
        root: PathBuf,
        path: PathBuf,
        cursor: u32,
    },
    ApplyVcsSnapshot {
        root: PathBuf,
        changes: Vec<FileChange>,
    },
    ConfirmCreate {
        root: PathBuf,
        cursor: u32,
        input: String,
        target: PathBuf,
    },
    ApplyCreate {
        root: PathBuf,
        cursor: u32,
        input: String,
        target: PathBuf,
        modified_buffer_check: ModifiedBufferCheck,
    },
    ConfirmMove {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        input: String,
        destination: PathBuf,
    },
    ApplyMove {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        input: String,
        destination: PathBuf,
        modified_buffer_check: ModifiedBufferCheck,
    },
    PromptDelete {
        target: PathBuf,
        root: PathBuf,
        cursor: u32,
    },
    ApplyDelete {
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
    ConfirmCopy {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        input: String,
        destination: PathBuf,
    },
    ApplyCopy {
        source: PathBuf,
        root: PathBuf,
        cursor: u32,
        destination: PathBuf,
        modified_buffer_check: ModifiedBufferCheck,
    },
    PromptSaveBefore {
        operation: String,
        documents: Vec<DocumentId>,
        continuation: Box<FileExplorerCommand>,
    },
}
