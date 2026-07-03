//! Assistant domain (`docs/collaboration-assistant-architecture-spec.md`).

pub mod acp;
pub mod action;
pub mod auth;
pub mod backend;
pub mod change;
pub mod config;
pub mod context;
pub mod effect;
pub mod elicitation;
pub mod event;
pub mod history;
pub mod host;
pub mod layout;
pub mod mention;
pub mod mode;
pub mod model;
pub mod permission;
pub mod plan;
pub mod prompt;
pub mod review;
pub mod store;
pub mod terminal;
pub mod thread;
pub mod tool;

pub use action::Action;
pub use backend::{Command as BackendCommand, Driver as BackendDriver, Handle as BackendHandle};
pub use model::{EntryView, Follow, Panel, Pill, Tab, ThreadView};
pub use store::Store;
pub use thread::Id as ThreadId;
