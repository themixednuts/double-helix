use std::path::PathBuf;

use helix_view::{editor::FileExplorerConfig, DocumentId};

use crate::ui::{ExplorerPath, ExplorerSource, ExplorerSourceId};

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
    RefreshCollaboration {
        project: helix_collab::ProjectId,
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
        root: ExplorerPath,
        generation: u64,
    },
    PreviewSelection {
        source: ExplorerSourceId,
        root: ExplorerPath,
        path: ExplorerPath,
        cursor: u32,
        generation: u64,
    },
    ApplyPreview {
        source: ExplorerSourceId,
        root: ExplorerPath,
        path: ExplorerPath,
        cursor: u32,
        generation: u64,
    },
    ApplyVcsSnapshot {
        root: PathBuf,
        snapshot: crate::ui::VcsSnapshot,
    },
    StartSearch {
        source: ExplorerSource,
        root: ExplorerPath,
        query: String,
        generation: u64,
        config: FileExplorerConfig,
    },
    ApplySearchResults {
        source: ExplorerSourceId,
        root: ExplorerPath,
        query: String,
        generation: u64,
        matches: Vec<ExplorerPath>,
    },
    ApplyWorkspaceTransaction {
        root: ExplorerPath,
        cursor: u32,
        select_path: Option<ExplorerPath>,
        transaction: helix_workspace::FileTransaction,
        success: String,
        modified_buffer_check: ModifiedBufferCheck,
    },
    ApplyWorkspacePaste {
        root: ExplorerPath,
        cursor: u32,
        source: helix_workspace::WorkspacePath,
        destination: helix_workspace::WorkspacePath,
        move_source: bool,
        modified_buffer_check: ModifiedBufferCheck,
    },
    PromptWorkspaceDelete {
        root: ExplorerPath,
        cursor: u32,
        target: helix_workspace::WorkspacePath,
    },
    ReplayWorkspaceTransaction {
        root: ExplorerPath,
        cursor: u32,
        redo: bool,
    },
    WorkspaceTransactionCompleted {
        root: ExplorerPath,
        cursor: u32,
        select_path: Option<ExplorerPath>,
        result: Result<String, String>,
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
