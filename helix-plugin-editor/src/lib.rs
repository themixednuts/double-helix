//! Editor-owned implementations of the host-agnostic plugin contract.
//!
//! The plugin process depends only on `helix-plugin`. Frontends built around
//! `helix-view::Editor` use this crate to translate editor state and apply
//! contract operations on the editor thread.

pub mod adapt;
pub mod bridge;
