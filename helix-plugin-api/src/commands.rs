//! Serializable command discovery types.

use serde::{Deserialize, Serialize};

/// How a command is invoked by the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandKind {
    Typable,
    Static,
    Plugin,
}

/// Editor state required to execute a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandScope {
    Viewport,
    Tree,
    Frontend,
}

/// A command-line flag accepted by a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandFlagDescriptor {
    pub name: String,
    pub alias: Option<char>,
    pub doc: String,
    pub takes_value: bool,
    #[serde(default)]
    pub values: Vec<String>,
}

/// Parser-level command signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSignatureDescriptor {
    pub min_positionals: usize,
    pub max_positionals: Option<usize>,
    pub raw_after: Option<u8>,
    #[serde(default)]
    pub flags: Vec<CommandFlagDescriptor>,
}

/// Discoverable metadata for a built-in or plugin command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDescriptor {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub doc: String,
    /// Human-readable argument names supplied by plugin commands.
    #[serde(default)]
    pub arguments: Vec<String>,
    pub signature: Option<CommandSignatureDescriptor>,
    pub kind: CommandKind,
    pub scope: CommandScope,
}
