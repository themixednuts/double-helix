//! Command logic that operates on editor state.
//!
//! These functions contain the core editing/movement logic and are frontend-agnostic.
//! helix-term wraps them with Context and dispatches key events to them.

pub mod context;
pub mod editing;
pub mod movement;
