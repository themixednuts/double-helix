//! Theme and config loading for the storybook binary.
//!
//! Interactive runs honor the user's helix config (so they see how the UI
//! looks with their personal theme + tweaks). `dump_story` paths use a
//! deterministic baseline config so tests and snapshots don't drift with
//! per-user customization.

use std::sync::OnceLock;

use anyhow::{bail, Result};
use helix_view::theme;

use super::model::LoadedTheme;

pub(super) fn theme_loader() -> Result<theme::Loader> {
    ensure_loader_paths();
    Ok(theme::Loader::new(&[helix_loader::config_dir()])
        .with_runtime_assets(helix_loader::runtime_assets()?.clone()))
}

pub(super) fn ensure_loader_paths() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        helix_loader::initialize_config_file(None);
        helix_loader::initialize_log_file(None);
    });
}

pub(super) fn load_storybook_config() -> Result<crate::config::Config> {
    ensure_loader_paths();
    match crate::config::Config::load_default() {
        Ok(config) => Ok(config),
        Err(crate::config::ConfigLoadError::Error(_)) => Ok(crate::config::Config::default()),
        Err(err) => Err(anyhow::anyhow!("failed to load config: {err}")),
    }
}

pub(super) fn load_named_storybook_theme(name: &str) -> Result<LoadedTheme> {
    let config = load_storybook_config()?;
    Ok(LoadedTheme::new(theme_loader()?.load(name)?, config.editor))
}

pub(super) fn load_storybook_theme(
    choice: &ThemeChoice,
    mode: Option<theme::Mode>,
) -> Result<LoadedTheme> {
    let loader = theme_loader()?;
    let config = load_storybook_config()?;
    let theme = match choice {
        ThemeChoice::Configured => match config.theme.as_ref() {
            Some(theme_config) => loader.load(theme_config.choose(mode))?,
            None => loader.default(),
        },
        ThemeChoice::Named(name) => loader.load(name)?,
    };

    Ok(LoadedTheme::new(theme, config.editor))
}

pub(super) fn available_theme_names() -> Result<Vec<String>> {
    theme_loader()?.names()
}

pub(super) fn parse_theme_mode(value: &str) -> Result<theme::Mode> {
    match value {
        "dark" => Ok(theme::Mode::Dark),
        "light" => Ok(theme::Mode::Light),
        other => bail!("unsupported theme mode: {other}; expected dark or light"),
    }
}

#[derive(Clone, Debug)]
pub(super) enum ThemeChoice {
    Configured,
    Named(String),
}

impl ThemeChoice {
    pub(super) fn parse(value: &str) -> Self {
        match value {
            "config" | "configured" | "current" => Self::Configured,
            name => Self::Named(name.to_string()),
        }
    }
}
