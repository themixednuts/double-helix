//! Owned declarative keymap contributions.

use serde::{Deserialize, Serialize};

use super::handles::KeymapHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeymapMode {
    Normal,
    Insert,
    Select,
}

/// All populated fields must match for a contribution to apply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapScope {
    pub language: Option<String>,
    pub path_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapBinding {
    /// Canonical key names, one entry per chord in the sequence.
    pub keys: Vec<String>,
    /// Mappable command strings. Multiple entries execute as a sequence.
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapDefinition {
    pub mode: KeymapMode,
    #[serde(default)]
    pub scope: KeymapScope,
    pub bindings: Vec<KeymapBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapUpdateRequest {
    pub keymap: KeymapHandle,
    pub definition: KeymapDefinition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapRemoveRequest {
    pub keymap: KeymapHandle,
}
