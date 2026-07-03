//! Typed compositor-facing ingress.
//!
//! Domain command Modules own their payloads. This file only keeps the root
//! sum type so the main-loop ingress has one typed envelope without turning
//! each feature flow into central schema churn.

mod assistant;
mod completion;
mod dap;
mod document;
mod file_explorer;
mod layer;
mod lsp;
mod picker;
mod plugin;

pub use assistant::{AssistantCommand, ModeConfigPickerItem};
pub use completion::CompletionCommand;
pub use dap::{DapCommand, DapThreadAction};
pub use document::DocumentCommand;
pub use file_explorer::{FileExplorerCommand, ModifiedBufferCheck};
pub use layer::LayerCommand;
pub use lsp::{
    DocumentSymbolPickerItem, LspCallHierarchyDirection, LspCodeActionItem,
    LspCodeActionPresentation, LspCommand, LspHierarchyPickerItem, LspHierarchyPrepareItem,
    LspHoverDisplay, LspLocation, LspTypeHierarchyDirection,
};
pub use picker::PickerCommand;
pub use plugin::PluginCommand;

/// Top-level UI command delivered on the main thread via typed runtime ingress.
pub enum UiCommand {
    Layer(LayerCommand),
    Completion(Box<CompletionCommand>),
    Picker(PickerCommand),
    /// Document-local async apply operations.
    Document(DocumentCommand),
    /// LSP navigation / overlays.
    Lsp(LspCommand),
    /// Async work completed with nothing to apply on the main loop.
    Nop,
    /// Full compositor redraw (e.g. after async prompt validation).
    NeedFullRedraw,
    /// Debugger prompts / overlays.
    Dap(DapCommand),
    /// Assistant panels / pickers.
    Assistant(AssistantCommand),
    /// File explorer prompts / confirmations.
    FileExplorer(FileExplorerCommand),
    /// Plugin-originated typed UI requests.
    Plugin(PluginCommand),
}

impl std::fmt::Debug for UiCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Layer(c) => f.debug_tuple("Layer").field(c).finish(),
            Self::Completion(c) => f.debug_tuple("Completion").field(c).finish(),
            Self::Picker(c) => f.debug_tuple("Picker").field(c).finish(),
            Self::Document(c) => f.debug_tuple("Document").field(c).finish(),
            Self::Lsp(c) => match c {
                LspCommand::Goto { .. } => f.write_str("Lsp(Goto(..))"),
                LspCommand::Hover { .. } => f.write_str("Lsp(Hover(..))"),
                LspCommand::CodeActions { .. } => f.write_str("Lsp(CodeActions(..))"),
                LspCommand::DocumentSymbols { .. } => f.write_str("Lsp(DocumentSymbols(..))"),
                LspCommand::HierarchyPrepare { .. } => f.write_str("Lsp(HierarchyPrepare(..))"),
                LspCommand::Hierarchy { .. } => f.write_str("Lsp(Hierarchy(..))"),
                LspCommand::SignatureHelp { .. } => f.write_str("Lsp(SignatureHelp(..))"),
                LspCommand::PrepareRename { .. } => f.write_str("Lsp(PrepareRename(..))"),
            },
            Self::Nop => f.write_str("Nop"),
            Self::NeedFullRedraw => f.write_str("NeedFullRedraw"),
            Self::Dap(c) => f.debug_tuple("Dap").field(c).finish(),
            Self::Assistant(c) => f.debug_tuple("Acp").field(c).finish(),
            Self::FileExplorer(c) => f.debug_tuple("FileExplorer").field(c).finish(),
            Self::Plugin(c) => f.debug_tuple("Plugin").field(c).finish(),
        }
    }
}
