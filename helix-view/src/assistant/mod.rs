//! Assistant domain (`docs/collaboration-assistant-architecture-spec.md`).

pub mod acp;
pub mod action;
pub mod backend;
pub mod change;
pub mod config;
pub mod context;
pub mod effect;
pub mod event;
pub mod history;
pub mod host;
pub mod layout;
pub mod mode;
pub mod view;
pub mod permission;
pub mod plan;
pub mod prompt;
pub mod store;
pub mod terminal;
pub mod thread;
pub mod tool;

pub use action::Action;
pub use backend::{Command as BackendCommand, Driver as BackendDriver, Handle as BackendHandle};
pub use view::View as AssistantView;
pub use store::Store;
pub use thread::Id as ThreadId;
