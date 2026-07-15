use std::sync::Arc;

use anyhow::Error;

use helix_view::{editor::ConfigEvent, theme, Editor};

use super::Application;
use crate::config::Config;

impl Application {
    fn reconfigure_terminal(&mut self, config: tui::terminal::Config) -> std::io::Result<()> {
        if let Some(presenter) = &self.presenter {
            presenter.reconfigure(config)
        } else {
            self.terminal
                .as_mut()
                .expect("startup terminal must exist before presenter handoff")
                .reconfigure(config)
        }
    }

    fn refresh_keymaps(&mut self) {
        let keys = self.config.load().keys.clone();
        if let Some(editor_view) = self.compositor.find::<crate::ui::EditorView>() {
            editor_view.keymaps.replace_base(keys.clone());
        }
        self.editor
            .set_modal_keymaps(crate::keymap::to_component_modal_keymaps(&keys));
        self.editor
            .set_semantic_modal_keymaps(crate::keymap::to_semantic_modal_keymaps(&keys));
    }

    pub fn handle_config_events(&mut self, config_event: ConfigEvent) {
        self.editor.bump_config_generation();

        match config_event {
            ConfigEvent::Refresh => {
                self.queue_config_reload(self.editor.config_gen);
            }
            ConfigEvent::Update(editor_config) => {
                let old_editor_config = self.editor.config();
                let mut app_config = (*self.config.load().clone()).clone();
                app_config.editor = *editor_config;
                if let Err(err) = self.reconfigure_terminal((&app_config.editor).into()) {
                    self.editor.set_error(err.to_string());
                };
                self.editor.diff_providers =
                    helix_vcs::DiffProviderRegistry::new(app_config.editor.vcs.provider.into());
                self.config.store(Arc::new(app_config));
                self.editor
                    .dispatch_editor_config_change(&old_editor_config);
                self.refresh_keymaps();

                self.editor.refresh_config(&old_editor_config);
                self.editor.refresh_all_language_servers();
                crate::effect::refresh_assistant_agent_cache(&self.editor, self.ingress_sender());
                self.editor.ensure_all_cursors_in_view();
            }
        }
    }

    fn queue_config_reload(&mut self, request: u64) {
        self.editor.set_status("Refreshing config...");
        let ingress = self.ingress_sender();
        let block = self.runtime.block().clone();
        self.runtime
            .work()
            .spawn(async move {
                let loaded = block
                    .spawn(move || -> Result<_, Error> {
                        let config = Config::load_default()
                            .map_err(|error| anyhow::anyhow!("Failed to load config: {error}"))?;
                        let language_loader = helix_core::config::user_lang_loader()?;
                        Ok((config, language_loader))
                    })
                    .await;
                let task = match loaded {
                    Ok(Ok((config, language_loader))) => {
                        crate::runtime::RuntimeTaskEvent::ApplyConfigReload(
                            crate::runtime::PreparedConfigReload {
                                request,
                                config: Box::new(config),
                                language_loader,
                            },
                        )
                    }
                    Ok(Err(error)) => crate::runtime::RuntimeTaskEvent::ConfigReloadFailed {
                        request,
                        message: error.to_string(),
                    },
                    Err(error) => crate::runtime::RuntimeTaskEvent::ConfigReloadFailed {
                        request,
                        message: format!("Config reload worker failed: {error}"),
                    },
                };
                let _ = ingress.send_task(task).await;
            })
            .detach();
    }

    pub(super) fn apply_prepared_config_reload(
        &mut self,
        prepared: crate::runtime::PreparedConfigReload,
    ) {
        if prepared.request != self.editor.config_gen {
            log::debug!(
                "discarding stale config reload request={} current={}",
                prepared.request,
                self.editor.config_gen
            );
            return;
        }

        let config = *prepared.config;
        if let Err(error) = self.reconfigure_terminal((&config.editor).into()) {
            self.editor
                .set_error(format!("Failed to apply terminal config: {error}"));
            return;
        }

        let old_editor_config = self.editor.config();
        match super::plugin_config(&config) {
            Ok(plugins) => {
                if let Err(error) = self.plugin_runtime.reconfigure(
                    &plugins,
                    self.ingress.tx.clone(),
                    self.foreground.clone(),
                    self.runtime.work().clone(),
                ) {
                    self.editor
                        .set_error(format!("Failed to refresh plugin runtime: {error}"));
                }
            }
            Err(error) => self
                .editor
                .set_error(format!("Failed to refresh plugin runtime: {error}")),
        }
        self.config.store(Arc::new(config));
        self.editor
            .replace_language_loader(prepared.language_loader);
        Self::load_configured_theme(
            &mut self.editor,
            &self.config.load(),
            self.terminal_state.supports_true_color,
            self.terminal_state.theme_mode,
        );
        self.editor.diff_providers =
            helix_vcs::DiffProviderRegistry::new(self.config.load().editor.vcs.provider.into());
        self.editor.refresh_document_languages();
        self.editor
            .dispatch_editor_config_change(&old_editor_config);
        self.refresh_keymaps();
        self.editor.refresh_config(&old_editor_config);
        self.editor.refresh_all_language_servers();
        crate::effect::refresh_assistant_agent_cache(&self.editor, self.ingress_sender());
        self.editor.ensure_all_cursors_in_view();
        self.editor.set_status("Config refreshed");
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
