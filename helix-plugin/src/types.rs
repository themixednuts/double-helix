use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::num::NonZeroU64;
use std::path::PathBuf;

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
    /// Exact host contract version this plugin targets.
    pub api_version: u32,
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
            api_version: crate::contract::metadata::API_VERSION,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginConfigError {
    #[error("plugin host at index {index} has an empty name")]
    EmptyHostName { index: usize },
    #[error("plugin host '{host}' has an empty command")]
    EmptyHostCommand { host: String },
    #[error("plugin host name '{name}' is configured more than once")]
    DuplicateHostName { name: String },
    #[error("plugin entry at index {index} has an empty name")]
    EmptyPluginName { index: usize },
    #[error("plugin name '{name}' is configured more than once")]
    DuplicatePluginName { name: String },
}

impl PluginConfig {
    pub fn validate(&self) -> Result<(), PluginConfigError> {
        let mut host_names = HashSet::with_capacity(self.hosts.len());
        for (index, host) in self.hosts.iter().enumerate() {
            if host.name.trim().is_empty() {
                return Err(PluginConfigError::EmptyHostName { index });
            }
            if host.command.as_os_str().is_empty() {
                return Err(PluginConfigError::EmptyHostCommand {
                    host: host.name.clone(),
                });
            }
            if !host_names.insert(host.name.as_str()) {
                return Err(PluginConfigError::DuplicateHostName {
                    name: host.name.clone(),
                });
            }
        }

        let mut plugin_names = HashSet::with_capacity(self.plugins.len());
        for (index, plugin) in self.plugins.iter().enumerate() {
            if plugin.name.trim().is_empty() {
                return Err(PluginConfigError::EmptyPluginName { index });
            }
            if !plugin_names.insert(plugin.name.as_str()) {
                return Err(PluginConfigError::DuplicatePluginName {
                    name: plugin.name.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Configuration for an out-of-process plugin runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginHostConfig {
    /// Stable name used in logs and diagnostics.
    pub name: String,
    /// Executable to spawn, e.g. `dhx` or `ssh`.
    pub command: PathBuf,
    /// Command-line arguments passed as-is.
    #[serde(default)]
    pub args: Vec<String>,
    /// Plugin directories interpreted on the child host's filesystem.
    #[serde(default)]
    pub plugin_dirs: Vec<PathBuf>,
}

/// Configuration for an individual plugin
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_config_rejects_ambiguous_names() {
        let config = PluginConfig {
            hosts: vec![
                PluginHostConfig {
                    name: "host".into(),
                    command: "dhx".into(),
                    args: Vec::new(),
                    plugin_dirs: Vec::new(),
                },
                PluginHostConfig {
                    name: "host".into(),
                    command: "ssh".into(),
                    args: Vec::new(),
                    plugin_dirs: Vec::new(),
                },
            ],
            ..PluginConfig::default()
        };

        assert_eq!(
            config.validate(),
            Err(PluginConfigError::DuplicateHostName {
                name: "host".into()
            })
        );
    }

    #[test]
    fn plugin_config_rejects_empty_commands_and_plugin_names() {
        let config = PluginConfig {
            hosts: vec![PluginHostConfig {
                name: "host".into(),
                command: PathBuf::new(),
                args: Vec::new(),
                plugin_dirs: Vec::new(),
            }],
            ..PluginConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(PluginConfigError::EmptyHostCommand {
                host: "host".into()
            })
        );

        let config = PluginConfig {
            plugins: vec![IndividualPluginConfig {
                name: " ".into(),
                enabled: true,
                config: serde_json::Value::Null,
            }],
            ..PluginConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(PluginConfigError::EmptyPluginName { index: 0 })
        );
    }
}
