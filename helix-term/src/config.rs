use crate::keymap;
use crate::keymap::{merge_keys, KeyTrie};
use helix_loader::merge_toml_values;
use helix_view::{
    document::Mode,
    icons::{Icons, ICONS},
    theme,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Display;
use std::fs;
use std::io::Error as IOError;
use std::sync::Arc;
use toml::de::Error as TomlError;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub theme: Option<theme::Config>,
    pub keys: HashMap<Mode, KeyTrie>,
    pub editor: helix_view::editor::Config,
    pub plugins: helix_plugin::PluginConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRaw {
    pub theme: Option<theme::Config>,
    pub keys: Option<HashMap<Mode, KeyTrie>>,
    pub editor: Option<toml::Value>,
    pub pkg: Option<toml::Value>,
    pub plugins: Option<toml::Value>,
    pub icons: Option<toml::Value>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            theme: None,
            keys: keymap::default(),
            editor: helix_view::editor::Config::default(),
            plugins: helix_plugin::PluginConfig::default(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigLoadError {
    BadConfig(TomlError),
    Plugin(helix_plugin::PluginConfigError),
    Error(IOError),
}

impl Default for ConfigLoadError {
    fn default() -> Self {
        ConfigLoadError::Error(IOError::new(std::io::ErrorKind::NotFound, "place holder"))
    }
}

impl Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigLoadError::BadConfig(err) => err.fmt(f),
            ConfigLoadError::Plugin(err) => err.fmt(f),
            ConfigLoadError::Error(err) => err.fmt(f),
        }
    }
}

impl Config {
    pub fn load(
        global: Result<String, ConfigLoadError>,
        local: Result<String, ConfigLoadError>,
    ) -> Result<Config, ConfigLoadError> {
        let global_config: Result<ConfigRaw, ConfigLoadError> =
            global.and_then(|file| toml::from_str(&file).map_err(ConfigLoadError::BadConfig));
        let local_config: Result<ConfigRaw, ConfigLoadError> =
            local.and_then(|file| toml::from_str(&file).map_err(ConfigLoadError::BadConfig));

        let res = match (global_config, local_config) {
            (Ok(global), Ok(local)) => {
                let mut keys = keymap::default();
                if let Some(global_keys) = global.keys {
                    merge_keys(&mut keys, global_keys)
                }
                if let Some(local_keys) = local.keys {
                    merge_keys(&mut keys, local_keys)
                }

                let mut editor = match (global.editor, local.editor) {
                    (None, None) => helix_view::editor::Config::default(),
                    (None, Some(val)) | (Some(val), None) => {
                        val.try_into().map_err(ConfigLoadError::BadConfig)?
                    }
                    (Some(global), Some(local)) => merge_toml_values(global, local, 3)
                        .try_into()
                        .map_err(ConfigLoadError::BadConfig)?,
                };
                if let Some(pkg) = merge_pkg_config(global.pkg, local.pkg)? {
                    editor.pkg = pkg;
                }
                let plugins =
                    merge_plugin_config(global.plugins, local.plugins)?.unwrap_or_default();
                plugins.validate().map_err(ConfigLoadError::Plugin)?;

                let icons: Icons = match (global.icons, local.icons) {
                    (None, None) => Icons::default(),
                    (None, Some(val)) | (Some(val), None) => {
                        val.try_into().map_err(ConfigLoadError::BadConfig)?
                    }
                    (Some(global), Some(local)) => merge_toml_values(global, local, 3)
                        .try_into()
                        .map_err(ConfigLoadError::BadConfig)?,
                };

                ICONS.store(Arc::new(icons));

                Config {
                    theme: local.theme.or(global.theme),
                    keys,
                    editor,
                    plugins,
                }
            }
            // if any configs are invalid return that first
            (_, Err(ConfigLoadError::BadConfig(err)))
            | (Err(ConfigLoadError::BadConfig(err)), _) => {
                return Err(ConfigLoadError::BadConfig(err))
            }
            (Ok(config), Err(_)) | (Err(_), Ok(config)) => {
                let mut keys = keymap::default();
                if let Some(keymap) = config.keys {
                    merge_keys(&mut keys, keymap);
                }

                let icons = config.icons.map_or_else(
                    || Ok(Icons::default()),
                    |val| val.try_into().map_err(ConfigLoadError::BadConfig),
                )?;

                ICONS.store(Arc::new(icons));

                let plugins = merge_plugin_config(config.plugins, None)?.unwrap_or_default();
                plugins.validate().map_err(ConfigLoadError::Plugin)?;

                Config {
                    theme: config.theme,
                    keys,
                    plugins,
                    editor: {
                        let mut editor = config.editor.map_or_else(
                            || Ok(helix_view::editor::Config::default()),
                            |val| val.try_into().map_err(ConfigLoadError::BadConfig),
                        )?;
                        if let Some(pkg) = merge_pkg_config(config.pkg, None)? {
                            editor.pkg = pkg;
                        }
                        editor
                    },
                }
            }

            // these are just two io errors return the one for the global config
            (Err(err), Err(_)) => return Err(err),
        };

        Ok(res)
    }

    pub fn load_default() -> Result<Config, ConfigLoadError> {
        let global_config =
            fs::read_to_string(helix_loader::config_file()).map_err(ConfigLoadError::Error);
        let local_config = fs::read_to_string(helix_loader::workspace_config_file())
            .map_err(ConfigLoadError::Error);
        Config::load(global_config, local_config)
    }
}

fn merge_pkg_config(
    global: Option<toml::Value>,
    local: Option<toml::Value>,
) -> Result<Option<helix_view::editor::PkgConfig>, ConfigLoadError> {
    match (global, local) {
        (None, None) => Ok(None),
        (None, Some(value)) | (Some(value), None) => value
            .try_into()
            .map(Some)
            .map_err(ConfigLoadError::BadConfig),
        (Some(global), Some(local)) => merge_toml_values(global, local, 3)
            .try_into()
            .map(Some)
            .map_err(ConfigLoadError::BadConfig),
    }
}

fn merge_plugin_config(
    global: Option<toml::Value>,
    local: Option<toml::Value>,
) -> Result<Option<helix_plugin::PluginConfig>, ConfigLoadError> {
    match (global, local) {
        (None, None) => Ok(None),
        (None, Some(value)) | (Some(value), None) => value
            .try_into()
            .map(Some)
            .map_err(ConfigLoadError::BadConfig),
        (Some(global), Some(local)) => merge_toml_values(global, local, 3)
            .try_into()
            .map(Some)
            .map_err(ConfigLoadError::BadConfig),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Config {
        fn load_test(config: &str) -> Config {
            Config::load(Ok(config.to_owned()), Err(ConfigLoadError::default())).unwrap()
        }
    }

    #[test]
    fn parsing_keymaps_config_file() {
        use crate::keymap;
        use helix_core::hashmap;
        use helix_view::document::Mode;

        let sample_keymaps = r#"
            [keys.insert]
            y = "move_line_down"
            S-C-a = "delete_selection"

            [keys.normal]
            A-F12 = "move_next_word_end"
        "#;

        let mut keys = keymap::default();
        merge_keys(
            &mut keys,
            hashmap! {
                Mode::Insert => keymap!({ "Insert mode"
                    "y" => move_line_down,
                    "S-C-a" => delete_selection,
                }),
                Mode::Normal => keymap!({ "Normal mode"
                    "A-F12" => move_next_word_end,
                }),
            },
        );

        assert_eq!(
            Config::load_test(sample_keymaps),
            Config {
                keys,
                ..Default::default()
            }
        );
    }

    #[test]
    fn parsing_bufferline_string_config_file() {
        let config = Config::load_test(
            r#"
            [editor]
            bufferline = "multiple"
            "#,
        );

        assert_eq!(
            config.editor.bufferline.render_mode,
            helix_view::editor::BufferLineRenderMode::Multiple
        );
        assert_eq!(config.editor.bufferline.separator, "│");
    }

    #[test]
    fn parsing_plugin_runtime_config() {
        let config = Config::load_test(
            r#"
            [plugins]
            enabled = true
            plugin_dirs = ["plugins", "workspace-plugins"]
            max_memory = 1048576
            max_instructions = 250000

            [[plugins.hosts]]
            name = "remote"
            command = "ssh"
            args = ["build-box", "dhx", "--plugin-host"]
            plugin_dirs = ["/srv/helix/plugins"]

            [[plugins.plugins]]
            name = "example"
            enabled = false

            [plugins.plugins.config]
            level = "trace"
            "#,
        );

        assert_eq!(
            config.plugins.plugin_dirs,
            [
                std::path::PathBuf::from("plugins"),
                std::path::PathBuf::from("workspace-plugins")
            ]
        );
        assert_eq!(config.plugins.max_memory, 1_048_576);
        assert_eq!(config.plugins.max_instructions, 250_000);
        assert_eq!(config.plugins.hosts.len(), 1);
        assert_eq!(config.plugins.hosts[0].name, "remote");
        assert_eq!(config.plugins.hosts[0].command, std::path::Path::new("ssh"));
        assert_eq!(
            config.plugins.hosts[0].args,
            ["build-box", "dhx", "--plugin-host"]
        );
        assert_eq!(config.plugins.plugins.len(), 1);
        assert!(!config.plugins.plugins[0].enabled);
        assert_eq!(
            config.plugins.plugins[0].config["level"],
            serde_json::json!("trace")
        );
    }

    #[test]
    fn local_plugin_config_overrides_only_declared_fields() {
        let config = Config::load(
            Ok(r#"
                [plugins]
                plugin_dirs = ["global-plugins"]
                max_memory = 2048
                max_instructions = 3000
                "#
            .to_owned()),
            Ok(r#"
                [plugins]
                enabled = false
                max_instructions = 4000
                "#
            .to_owned()),
        )
        .unwrap();

        assert!(!config.plugins.enabled);
        assert_eq!(
            config.plugins.plugin_dirs,
            [std::path::PathBuf::from("global-plugins")]
        );
        assert_eq!(config.plugins.max_memory, 2048);
        assert_eq!(config.plugins.max_instructions, 4000);
    }

    #[test]
    fn plugin_config_rejects_unknown_fields() {
        let result = Config::load(
            Ok("[plugins]\nunknown = true\n".to_owned()),
            Err(ConfigLoadError::default()),
        );

        assert!(matches!(result, Err(ConfigLoadError::BadConfig(_))));
    }

    #[test]
    fn plugin_config_rejects_duplicate_host_names() {
        let result = Config::load(
            Ok(r#"
                [[plugins.hosts]]
                name = "duplicate"
                command = "dhx"

                [[plugins.hosts]]
                name = "duplicate"
                command = "ssh"
                "#
            .to_owned()),
            Err(ConfigLoadError::default()),
        );

        assert!(matches!(
            result,
            Err(ConfigLoadError::Plugin(
                helix_plugin::PluginConfigError::DuplicateHostName { .. }
            ))
        ));
    }

    #[test]
    fn keys_resolve_to_correct_defaults() {
        // From serde default
        let default_keys = Config::load_test("").keys;
        assert_eq!(default_keys, keymap::default());

        // From the Default trait
        let default_keys = Config::default().keys;
        assert_eq!(default_keys, keymap::default());
    }
}
