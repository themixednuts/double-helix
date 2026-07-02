use std::sync::Arc;

use anyhow::Error;
use tui::backend::Backend;

use helix_view::{editor::ConfigEvent, theme, Editor};

use super::Application;
use crate::config::Config;

impl Application {
    pub fn handle_config_events(&mut self, config_event: ConfigEvent) {
        self.editor.bump_config_generation();
        let old_editor_config = self.editor.config();

        match config_event {
            ConfigEvent::Refresh => self.refresh_config(),
            ConfigEvent::Update(editor_config) => {
                let mut app_config = (*self.config.load().clone()).clone();
                app_config.editor = *editor_config;
                if let Err(err) = self.terminal.reconfigure((&app_config.editor).into()) {
                    self.editor.set_error(err.to_string());
                };
                self.editor.diff_providers =
                    helix_vcs::DiffProviderRegistry::new(app_config.editor.vcs.provider.into());
                self.config.store(Arc::new(app_config));
                self.editor
                    .dispatch_editor_config_change(&old_editor_config);
                self.editor
                    .set_modal_keymaps(crate::keymap::to_component_modal_keymaps(
                        &self.config.load().keys,
                    ));
                self.editor
                    .set_semantic_modal_keymaps(crate::keymap::to_semantic_modal_keymaps(
                        &self.config.load().keys,
                    ));
            }
        }

        self.editor.refresh_config(&old_editor_config);
        self.editor.ensure_all_cursors_in_view();
    }

    fn refresh_config(&mut self) {
        let mut refresh_config = || -> Result<(), Error> {
            let default_config = Config::load_default()
                .map_err(|err| anyhow::anyhow!("Failed to load config: {}", err))?;

            let lang_loader = helix_core::config::user_lang_loader()?;
            self.editor.replace_language_loader(lang_loader);
            Self::load_configured_theme(
                &mut self.editor,
                &default_config,
                self.terminal.backend().supports_true_color(),
                self.terminal_state.theme_mode,
            );
            self.editor.refresh_document_languages();

            self.terminal.reconfigure((&default_config.editor).into())?;
            self.editor.diff_providers =
                helix_vcs::DiffProviderRegistry::new(default_config.editor.vcs.provider.into());
            self.config.store(Arc::new(default_config));
            self.editor
                .set_modal_keymaps(crate::keymap::to_component_modal_keymaps(
                    &self.config.load().keys,
                ));
            self.editor
                .set_semantic_modal_keymaps(crate::keymap::to_semantic_modal_keymaps(
                    &self.config.load().keys,
                ));
            Ok(())
        };

        match refresh_config() {
            Ok(_) => self.editor.set_status("Config refreshed"),
            Err(err) => self.editor.set_error(err.to_string()),
        }
    }

    pub fn load_configured_theme(
        editor: &mut Editor,
        config: &Config,
        terminal_true_color: bool,
        mode: Option<theme::Mode>,
    ) {
        let true_color = terminal_true_color || config.editor.true_color || crate::true_color();
        let theme = config
            .theme
            .as_ref()
            .and_then(|theme_config| {
                let theme = theme_config.choose(mode);
                editor
                    .theme_loader
                    .load(theme)
                    .map_err(|e| {
                        log::warn!("failed to load theme `{}` - {}", theme, e);
                        e
                    })
                    .ok()
                    .filter(|theme| {
                        let colors_ok = true_color || theme.is_16_color();
                        if !colors_ok {
                            log::warn!(
                                "loaded theme `{}` but cannot use it because true color support is not enabled",
                                theme.name()
                            );
                        }
                        colors_ok
                    })
            })
            .unwrap_or_else(|| editor.theme_loader.default_theme(true_color));
        editor.set_theme(theme);
    }
}
