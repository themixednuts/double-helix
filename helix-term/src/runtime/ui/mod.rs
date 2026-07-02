//! Typed UI / main-thread ingress.
//!
//! Layout (per runtime plan): [`command`] (`UiCommand` schema), [`apply`] (central apply path).
//! Add `runtime/ui/<domain>.rs` when a concern grows enough to split (e.g. LSP UI helpers)
//! without pulling `handlers` ↔ `runtime` cycles—keep compositor work here, editor-only effects in [`crate::effect`].

pub mod apply;
pub(crate) mod assistant;
pub mod command;
pub(crate) mod completion;
pub(crate) mod dap;
pub(crate) mod document;
pub(crate) mod file_explorer;
pub(crate) mod layer;
pub(crate) mod lsp;
pub(crate) mod picker;
pub(crate) mod plugin;
pub(crate) mod snapshot;

pub use apply::{apply_ui_command, apply_ui_command_opt};
pub use command::{
    AssistantCommand, CompletionCommand, DapCommand, DocumentCommand, LayerCommand, PickerCommand,
    UiCommand,
};
