use crate::contract::metadata::{Capability, API_VERSION};
use crate::error::{PluginError, Result};
use crate::types::{Plugin, PluginMetadata};
use log::{debug, info, warn};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Plugin loader responsible for discovering and loading plugins
pub struct PluginLoader {
    /// Directories whose children are plugin roots.
    plugin_dirs: Vec<PathBuf>,
    /// Exact plugin roots selected by the package activation snapshot.
    plugin_roots: Vec<PathBuf>,
}

impl PluginLoader {
    /// Create a new plugin loader with the given plugin directories
    pub fn new(plugin_dirs: Vec<PathBuf>) -> Self {
        Self {
            plugin_dirs,
            plugin_roots: Vec::new(),
        }
    }

    /// Add exact plugin roots. Unlike `plugin_dirs`, these paths are loaded directly.
    pub fn with_plugin_roots(mut self, plugin_roots: Vec<PathBuf>) -> Self {
        self.plugin_roots = plugin_roots;
        self
    }

    /// Read the exact managed plugin roots from the active runtime snapshot.
    pub fn active_plugin_roots() -> Result<Vec<PathBuf>> {
        let assets = helix_loader::runtime_assets()
            .map_err(|error| PluginError::InitializationFailed(error.to_string()))?;
        Ok(assets.plugin_roots())
    }

    /// Discover all plugins in the configured directories
    pub fn discover_plugins(&self) -> Result<Vec<Plugin>> {
        let mut plugins = Vec::new();
        let mut seen = HashSet::new();

        for dir in &self.plugin_dirs {
            if !dir.exists() {
                debug!("Plugin directory does not exist: {:?}", dir);
                continue;
            }

            if !dir.is_dir() {
                warn!("Plugin path is not a directory: {:?}", dir);
                continue;
            }

            // Iterate through subdirectories
            let entries = std::fs::read_dir(dir)?;

            for entry in entries {
                let entry = entry?;
                let path = entry.path();

                if path.is_dir() {
                    self.discover_plugin_root(&path, &mut seen, &mut plugins);
                }
            }
        }

        for root in &self.plugin_roots {
            if !root.is_dir() {
                warn!("Activated plugin root is not a directory: {:?}", root);
                continue;
            }
            self.discover_plugin_root(root, &mut seen, &mut plugins);
        }

        Ok(plugins)
    }

    fn discover_plugin_root(
        &self,
        path: &Path,
        seen: &mut HashSet<PathBuf>,
        plugins: &mut Vec<Plugin>,
    ) {
        let identity = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !seen.insert(identity) {
            return;
        }

        match self.load_plugin_metadata(path) {
            Ok(plugin) => {
                info!("Discovered plugin: {} at {:?}", plugin.metadata.name, path);
                plugins.push(plugin);
            }
            Err(error) => warn!("Failed to load plugin at {:?}: {}", path, error),
        }
    }

    /// Load plugin metadata from a directory
    fn load_plugin_metadata(&self, path: &Path) -> Result<Plugin> {
        // Check for plugin.toml
        let metadata_file = path.join("plugin.toml");
        let metadata = if metadata_file.exists() {
            let content = std::fs::read_to_string(&metadata_file)?;
            toml::from_str::<PluginMetadata>(&content).map_err(|e| {
                PluginError::ConfigError(format!("Failed to parse plugin.toml: {}", e))
            })?
        } else {
            // Generate default metadata from directory name
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| {
                    PluginError::InvalidPluginStructure("Invalid plugin directory name".to_string())
                })?
                .to_string();

            PluginMetadata {
                name,
                ..Default::default()
            }
        };

        // Verify entry point exists
        let entry = metadata.entry.as_deref().unwrap_or("init.lua");
        let entry_path = path.join(entry);

        if !entry_path.exists() {
            return Err(PluginError::InvalidPluginStructure(format!(
                "Entry point '{}' not found in {:?}",
                entry, path
            )));
        }

        if metadata.api_version != API_VERSION {
            return Err(PluginError::InvalidPluginStructure(format!(
                "plugin targets API version {}, host requires exactly {API_VERSION}",
                metadata.api_version
            )));
        }

        for capability in &metadata.capabilities {
            Capability::from_str(capability).map_err(PluginError::ConfigError)?;
        }

        Ok(Plugin {
            metadata,
            path: path.to_path_buf(),
            enabled: true,
        })
    }

    /// Get default plugin directories
    pub fn default_plugin_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();

        // User config directory
        let config_dir = helix_loader::config_dir();
        dirs.push(config_dir.join("plugins"));

        // System-wide plugin directory (if installed)
        #[cfg(target_os = "linux")]
        {
            dirs.push(PathBuf::from("/usr/share/helix/plugins"));
            dirs.push(PathBuf::from("/usr/local/share/helix/plugins"));
        }

        #[cfg(target_os = "macos")]
        {
            dirs.push(PathBuf::from("/Library/Application Support/helix/plugins"));
        }

        dirs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_discover_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let loader = PluginLoader::new(vec![temp_dir.path().to_path_buf()]);

        let plugins = loader.discover_plugins().unwrap();
        assert_eq!(plugins.len(), 0);
    }

    #[test]
    fn test_load_plugin_with_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("test-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        // Create plugin.toml
        let metadata = r#"
            name = "test-plugin"
            version = "1.0.0"
            description = "A test plugin"
            author = "Test Author"
            entry = "init.lua"
            api_version = 1
        "#;
        std::fs::write(plugin_dir.join("plugin.toml"), metadata).unwrap();

        // Create init.lua
        std::fs::write(plugin_dir.join("init.lua"), "-- Test plugin").unwrap();

        let loader = PluginLoader::new(vec![temp_dir.path().to_path_buf()]);
        let plugins = loader.discover_plugins().unwrap();

        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].metadata.name, "test-plugin");
        assert_eq!(plugins[0].metadata.version, "1.0.0");
    }

    #[test]
    fn test_load_plugin_without_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("simple-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        // Create only init.lua
        std::fs::write(plugin_dir.join("init.lua"), "-- Simple plugin").unwrap();

        let loader = PluginLoader::new(vec![temp_dir.path().to_path_buf()]);
        let plugins = loader.discover_plugins().unwrap();

        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].metadata.name, "simple-plugin");
        assert_eq!(plugins[0].metadata.version, "0.1.0");
    }

    #[test]
    fn activated_plugin_root_is_loaded_directly_and_deduplicated() {
        let temp_dir = TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("managed-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("init.lua"), "-- managed plugin").unwrap();

        let loader = PluginLoader::new(vec![temp_dir.path().to_path_buf()])
            .with_plugin_roots(vec![plugin_dir.clone()]);
        let plugins = loader.discover_plugins().unwrap();

        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].metadata.name, "managed-plugin");
        assert_eq!(plugins[0].path, plugin_dir);
    }

    #[test]
    fn test_missing_entry_point() {
        let temp_dir = TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("broken-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        // Create plugin.toml but no init.lua
        let metadata = r#"
            name = "broken-plugin"
            version = "1.0.0"
            api_version = 1
        "#;
        std::fs::write(plugin_dir.join("plugin.toml"), metadata).unwrap();

        let loader = PluginLoader::new(vec![temp_dir.path().to_path_buf()]);
        let plugins = loader.discover_plugins().unwrap();

        // Should skip the broken plugin
        assert_eq!(plugins.len(), 0);
    }

    #[test]
    fn test_mismatched_api_version_refuses_plugin() {
        let temp_dir = TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("future-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        let metadata = r#"
            name = "future-plugin"
            version = "1.0.0"
            api_version = 999
        "#;
        std::fs::write(plugin_dir.join("plugin.toml"), metadata).unwrap();
        std::fs::write(plugin_dir.join("init.lua"), "-- future plugin").unwrap();

        let loader = PluginLoader::new(vec![temp_dir.path().to_path_buf()]);
        let plugins = loader.discover_plugins().unwrap();

        assert!(plugins.is_empty());
    }
}
