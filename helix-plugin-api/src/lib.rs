//! Host-agnostic plugin contract and wire types.
//!
//! This module defines the canonical types and traits that form the public
//! plugin API boundary. Language-host adapters (Lua today, potentially
//! wasm/RPC later) are built on top of this contract rather than reaching
//! into editor internals directly.
//!
//! # Module layout
//!
//! - [`handles`] — opaque identity tokens (`DocumentHandle`, `ViewHandle`, …)
//! - [`snapshots`] — immutable point-in-time state views
//! - [`requests`] — mutation request types (the primary mutation mechanism)
//! - [`events`] — typed event enum and payloads
//! - [`errors`] — structured error types for all contract operations
//! - [`metadata`] — API version, capability discovery, event catalog
//! - [`host`] — capability traits that hosts implement
//! - [`codec`] — msgpack-based serialization for wire transport

pub mod codec;
pub mod commands;
pub mod errors;
pub mod events;
pub mod handles;
pub mod host;
pub mod keymaps;
pub mod layout;
pub mod metadata;
pub mod requests;
pub mod snapshots;
pub mod tasks;
pub mod value;

// Convenience re-exports for the most commonly used types.
pub use commands::{
    CommandDescriptor, CommandFlagDescriptor, CommandKind, CommandScope, CommandSignatureDescriptor,
};
pub use errors::{ContractError, ContractResult};
pub use handles::{
    CommandHandle, DocumentHandle, FloatHandle, KeymapHandle, PanelHandle, PluginId,
    PluginOperationToken, SubscriptionHandle, ThreadHandle, UiCallbackToken, ViewHandle,
};
pub use keymaps::{
    KeymapBinding, KeymapDefinition, KeymapMode, KeymapRemoveRequest, KeymapScope,
    KeymapUpdateRequest,
};
pub use tasks::{
    LspCallRequest, PluginTaskRequest, PluginTaskResult, SyntaxCapture, SyntaxQueryRequest,
};
pub use value::DynamicValue;
