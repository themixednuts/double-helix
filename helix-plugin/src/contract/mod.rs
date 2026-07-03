//! Host-agnostic plugin contract.
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
//! - [`pkg`] — package-manager backend request and response types
//! - [`host`] — capability traits that hosts implement
//! - [`codec`] — msgpack-based serialization for wire transport

pub mod adapt;
pub mod bridge;
pub mod codec;
pub mod errors;
pub mod events;
pub mod handles;
pub mod host;
pub mod metadata;
pub mod pkg;
pub mod requests;
pub mod snapshots;
pub mod value;

// Convenience re-exports for the most commonly used types.
pub use errors::{ContractError, ContractResult};
pub use handles::{
    CommandHandle, DocumentHandle, FloatHandle, PanelHandle, PluginId, RenderCallbackHandle,
    SubscriptionHandle, ThreadHandle, ViewHandle,
};
pub use host::UiCallbackToken;
pub use value::DynamicValue;
