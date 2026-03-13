//! Command context types.
//!
//! Previously held `UiBridge` and `CommandContext` as a transitional layer
//! between commands and frontends. These have been replaced by direct
//! `Model` mutation: commands write to `editor.model`, frontends read it.
//! No intermediate bridge trait is needed.
