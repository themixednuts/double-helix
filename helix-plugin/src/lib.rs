//! Helix Plugin System
//!
//! This crate provides a Lua-based plugin system for the Helix text editor.
//! Plugins can register event handlers, custom commands, and interact with
//! the editor through a host-agnostic contract API.
//!
//! # Example Plugin
//!
//! ```lua
//! -- init.lua
//! helix.events.subscribe("document_opened", function(event)
//!     helix.log.info("Document opened: " .. (event.path or "untitled"))
//! end)
//!
//! helix.events.subscribe("document_saved", function(event)
//!     helix.log.info("Document saved: " .. (event.path or "untitled"))
//! end)
//!
//! helix.commands.register({
//!     name = "greet",
//!     doc = "Prompt for a name and greet",
//!     handler = function()
//!         local name = helix.ui.prompt("Name:")
//!         helix.ui.info("Hello, " .. (name or "world") .. "!")
//!     end,
//! })
//! ```

pub(crate) use helix_plugin_api as contract;
pub mod error;
pub mod lua;
pub mod rpc;
pub mod types;

// Re-exports
pub use error::{PluginError, Result};
pub use lua::LuaEngine;
pub use mlua;
pub use types::{
    IndividualPluginConfig, Plugin, PluginConfig, PluginConfigError, PluginHostConfig,
    PluginMetadata, UiCallbackId,
};
