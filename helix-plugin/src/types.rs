use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;
use std::path::PathBuf;

/// Lightweight notification signal for the plugin event channel.
///
/// Produced by editor hooks and sent through the async channel. The application
/// event loop converts these to full [`crate::contract::events::PluginEvent`]
/// payloads (with editor context) before dispatching to plugin handlers.
#[derive(Debug, Clone)]
pub enum PluginNotification {
    BufferOpen {
        document_id: helix_view::DocumentId,
        path: Option<PathBuf>,
    },
    SelectionChange {
        document_id: helix_view::DocumentId,
        path: Option<PathBuf>,
    },
    ModeChange {
        old_mode: String,
        new_mode: String,
    },
    KeyPress {
        key: String,
    },
    LspDiagnostic {
        document_id: helix_view::DocumentId,
        diagnostic_count: usize,
    },
}

/// Plugin metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMetadata {
    /// Plugin name
    pub name: String,
    /// Plugin version
    pub version: String,
    /// Plugin description
    pub description: Option<String>,
    /// Plugin author
    pub author: Option<String>,
    /// Plugin entry point (default: init.lua)
    pub entry: Option<String>,
    /// Minimum host API version required by this plugin.
    pub min_api_version: Option<u32>,
    /// Required host capability names.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl Default for PluginMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: "0.1.0".to_string(),
            description: None,
            author: None,
            entry: Some("init.lua".to_string()),
            min_api_version: None,
            capabilities: Vec::new(),
        }
    }
}

/// Represents a loaded plugin
#[derive(Debug)]
pub struct Plugin {
    /// Plugin metadata
    pub metadata: PluginMetadata,
    /// Plugin root path
    pub path: PathBuf,
    /// Whether the plugin is enabled
    pub enabled: bool,
}

/// Plugin configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    /// Whether plugins are enabled globally
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Plugin directories to search
    #[serde(default)]
    pub plugin_dirs: Vec<PathBuf>,
    /// Individual plugin configurations
    #[serde(default)]
    pub plugins: Vec<IndividualPluginConfig>,
    /// Out-of-process plugin hosts to spawn.
    #[serde(default)]
    pub hosts: Vec<PluginHostConfig>,
    /// Maximum Lua heap in bytes. Use 0 to disable the limit.
    #[serde(default = "default_max_memory")]
    pub max_memory: usize,
    /// Maximum VM instructions per plugin dispatch. Use 0 to disable the watchdog.
    #[serde(default = "default_max_instructions")]
    pub max_instructions: u64,
}

fn default_true() -> bool {
    true
}

fn default_max_memory() -> usize {
    256 * 1024 * 1024
}

fn default_max_instructions() -> u64 {
    5_000_000
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            plugin_dirs: vec![],
            plugins: vec![],
            hosts: vec![],
            max_memory: default_max_memory(),
            max_instructions: default_max_instructions(),
        }
    }
}

/// Configuration for an out-of-process plugin runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginHostConfig {
    /// Stable name used in logs and diagnostics.
    pub name: String,
    /// Executable to spawn, e.g. `helix-plugin-host` or `ssh`.
    pub command: String,
    /// Command-line arguments passed as-is.
    #[serde(default)]
    pub args: Vec<String>,
    /// Plugin directories interpreted on the child host's filesystem.
    #[serde(default)]
    pub plugin_dirs: Vec<PathBuf>,
}

/// Configuration for an individual plugin
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndividualPluginConfig {
    /// Plugin name
    pub name: String,
    /// Whether this plugin is enabled
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Plugin-specific configuration
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Metadata for a registered command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandMetadata {
    /// Command name
    pub name: String,
    /// Command documentation/help text
    pub doc: String,
    /// Arguments description (optional)
    pub args: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UiCallbackId(NonZeroU64);

impl UiCallbackId {
    pub fn new(id: u64) -> Option<Self> {
        NonZeroU64::new(id).map(Self)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl From<UiCallbackId> for u64 {
    fn from(id: UiCallbackId) -> Self {
        id.get()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PluginCallbackKey {
    pub plugin_name: String,
    pub callback_id: UiCallbackId,
}

impl PluginCallbackKey {
    pub fn new(plugin_name: String, callback_id: UiCallbackId) -> Self {
        Self {
            plugin_name,
            callback_id,
        }
    }
}

/// Wrapper for UI callbacks to store in Lua app data
pub struct UiCallbackRegistry(
    pub  std::sync::Arc<
        parking_lot::RwLock<std::collections::HashMap<PluginCallbackKey, mlua::RegistryKey>>,
    >,
);

/// Wrapper for UI callback counter to store in Lua app data
pub struct UiCallbackCounter(pub std::sync::Arc<std::sync::atomic::AtomicU64>);

impl UiCallbackCounter {
    pub fn next(&self) -> UiCallbackId {
        loop {
            let raw = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(id) = UiCallbackId::new(raw) {
                return id;
            }
        }
    }
}

/// A typed rendering command emitted by the plugin ABI.
///
/// Plugins record commands during Lua render callbacks. The terminal frontend
/// owns the actual Ratatui buffer and applies these commands after Lua returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceRenderOp {
    SetString {
        x: u16,
        y: u16,
        text: String,
        style: helix_view::graphics::Style,
    },
    SetStringN {
        x: u16,
        y: u16,
        text: String,
        max_width: usize,
        style: helix_view::graphics::Style,
    },
    Clear {
        area: helix_view::graphics::Rect,
        style: helix_view::graphics::Style,
    },
    SetStyle {
        area: helix_view::graphics::Rect,
        style: helix_view::graphics::Style,
    },
    Header {
        area: helix_view::graphics::Rect,
        title: String,
        style: helix_view::graphics::Style,
    },
    HeaderWithCounts {
        area: helix_view::graphics::Rect,
        title: String,
        current: usize,
        total: usize,
        style: helix_view::graphics::Style,
    },
    HDivider {
        area: helix_view::graphics::Rect,
        style: helix_view::graphics::Style,
    },
    VDivider {
        area: helix_view::graphics::Rect,
        style: helix_view::graphics::Style,
    },
    TextInput {
        area: helix_view::graphics::Rect,
        text: String,
        cursor: usize,
        style: helix_view::graphics::Style,
        cursor_style: helix_view::graphics::Style,
    },
    Scrollbar {
        area: helix_view::graphics::Rect,
        total: usize,
        offset: usize,
        visible: usize,
        thumb_style: helix_view::graphics::Style,
        track_symbol: Option<String>,
        track_style: helix_view::graphics::Style,
    },
}

/// Ordered render commands emitted by one plugin render callback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceRenderOps {
    ops: Vec<SurfaceRenderOp>,
}

impl SurfaceRenderOps {
    pub fn push(&mut self, op: SurfaceRenderOp) {
        self.ops.push(op);
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn as_slice(&self) -> &[SurfaceRenderOp] {
        &self.ops
    }
}

impl IntoIterator for SurfaceRenderOps {
    type Item = SurfaceRenderOp;
    type IntoIter = std::vec::IntoIter<SurfaceRenderOp>;

    fn into_iter(self) -> Self::IntoIter {
        self.ops.into_iter()
    }
}
