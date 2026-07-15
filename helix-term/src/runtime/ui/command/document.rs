use std::path::PathBuf;

use helix_core::{Position, Selection, Syntax};
use helix_view::{
    document::DocumentOpenError,
    editor::{
        Action, ClosePolicy, DocumentReloadError, FilePickerConfig, PreparedDocumentOpen,
        PreparedDocumentReload,
    },
    DocumentId, ViewId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentOpenLane {
    Navigation,
    Debug,
    Command,
    Plugin(u64),
}

#[derive(Debug, Clone)]
pub enum DocumentOpenCompletionTarget {
    Editor,
    Plugin(crate::plugin_registry::HostTaskResponder),
}

impl DocumentOpenCompletionTarget {
    pub fn is_editor(&self) -> bool {
        matches!(self, Self::Editor)
    }
}

#[derive(Debug, Clone)]
pub enum DocumentOpenSelection {
    None,
    Position(Position),
    Line(usize),
    CharRange {
        start: usize,
        end: usize,
    },
    LspRange {
        range: helix_lsp::lsp::Range,
        offset_encoding: helix_lsp::OffsetEncoding,
    },
    OneBasedRange {
        line: usize,
        column: usize,
        end_line: Option<usize>,
        end_column: Option<usize>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentOpenAlignment {
    None,
    Center,
    CenterIfAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentOpenTarget {
    View(ViewId),
    PreviousResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentOpenPostAction {
    None,
    DetachPath,
    RequestInlineValues,
    CollaborationReveal,
    AssistantFollow,
}

#[derive(Debug, Clone)]
pub struct FffOpenRecord {
    pub root: PathBuf,
    pub config: FilePickerConfig,
    pub query: String,
}

#[derive(Debug, Clone)]
pub struct DocumentOpenRequest {
    pub path: PathBuf,
    pub action: Action,
    pub lane: DocumentOpenLane,
    pub target: DocumentOpenTarget,
    pub selection: DocumentOpenSelection,
    pub alignment: DocumentOpenAlignment,
    pub default_folding_if_new: bool,
    pub fff_record: Option<FffOpenRecord>,
    pub external_if_binary: Option<url::Url>,
    pub post_action: DocumentOpenPostAction,
    pub completion: DocumentOpenCompletionTarget,
}

pub struct DocumentOpenCompletion {
    pub request: DocumentOpenRequest,
    pub result: Result<PreparedDocumentOpen, DocumentOpenError>,
}

impl std::fmt::Debug for DocumentOpenCompletion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DocumentOpenCompletion")
            .field("request", &self.request)
            .field("result", &self.result)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentReloadOrigin {
    Explicit,
    ReloadAll,
    Auto,
}

pub enum DocumentCommand {
    OpenRequested {
        request: helix_view::handlers::NavigationRequest,
    },
    CloseView {
        view: ViewId,
        check_buffers: bool,
    },
    CloseDocuments {
        documents: Vec<DocumentId>,
        policy: ClosePolicy,
    },
    CloseAllViews {
        check_buffers: bool,
    },
    ApplyRuntimeSyntax {
        document: DocumentId,
        path: PathBuf,
        version: i32,
        generation: u64,
        syntax: Syntax,
    },
    InsertFileFinished {
        document: DocumentId,
        view: ViewId,
        version: i32,
        selection: Selection,
        scrolloff: usize,
        path: PathBuf,
        result: Result<String, String>,
    },
    ReloadFinished {
        document: DocumentId,
        generation: u64,
        origin: DocumentReloadOrigin,
        result: Result<PreparedDocumentReload, DocumentReloadError>,
    },
    OpenFinished {
        generation: u64,
        lane: DocumentOpenLane,
        completions: Vec<DocumentOpenCompletion>,
        stop_on_error: bool,
    },
}

impl std::fmt::Debug for DocumentCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenRequested { request } => f
                .debug_struct("OpenRequested")
                .field("request", request)
                .finish(),
            Self::CloseView {
                view,
                check_buffers,
            } => f
                .debug_struct("CloseView")
                .field("view", view)
                .field("check_buffers", check_buffers)
                .finish(),
            Self::CloseDocuments { documents, policy } => f
                .debug_struct("CloseDocuments")
                .field("documents", documents)
                .field("policy", policy)
                .finish(),
            Self::CloseAllViews { check_buffers } => f
                .debug_struct("CloseAllViews")
                .field("check_buffers", check_buffers)
                .finish(),
            Self::ApplyRuntimeSyntax {
                document,
                path,
                version,
                generation,
                ..
            } => f
                .debug_struct("ApplyRuntimeSyntax")
                .field("document", document)
                .field("path", path)
                .field("version", version)
                .field("generation", generation)
                .finish_non_exhaustive(),
            Self::InsertFileFinished {
                document,
                view,
                version,
                path,
                result,
                ..
            } => f
                .debug_struct("InsertFileFinished")
                .field("document", document)
                .field("view", view)
                .field("version", version)
                .field("path", path)
                .field("result", result)
                .finish_non_exhaustive(),
            Self::ReloadFinished {
                document,
                generation,
                origin,
                result,
            } => f
                .debug_struct("ReloadFinished")
                .field("document", document)
                .field("generation", generation)
                .field("origin", origin)
                .field("result", result)
                .finish(),
            Self::OpenFinished {
                generation,
                lane,
                completions,
                stop_on_error,
            } => f
                .debug_struct("OpenFinished")
                .field("generation", generation)
                .field("lane", lane)
                .field("completions", completions)
                .field("stop_on_error", stop_on_error)
                .finish(),
        }
    }
}
