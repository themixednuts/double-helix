#![allow(
    clippy::unnecessary_operation,
    reason = "rendering code scopes CellSurface cell borrows before subsequent buffer writes"
)]

#[macro_use]
extern crate helix_view;

pub mod application;
pub mod args;
pub mod commands;
pub mod compositor;
pub mod config;
pub(crate) mod effect;
pub mod embed;
pub(crate) mod fff;
pub mod health;
pub mod host;
pub mod keymap;
pub mod migration;
pub mod plugin_registry;
pub mod render;
pub mod runtime;
pub use runtime::AppEvent;
pub mod shutdown;
pub mod storybook;
pub mod ui;
pub mod widgets;

#[cfg(not(windows))]
use std::env::var_os;

use std::path::Path;

use futures_util::Future;
mod handlers;

#[cfg(test)]
pub(crate) mod test_support;

use ignore::DirEntry;
use url::Url;

#[cfg(windows)]
fn true_color() -> bool {
    true
}

#[cfg(not(windows))]
fn true_color() -> bool {
    if var_os("COLORTERM").is_some_and(|v| v == "truecolor" || v == "24bit")
        || var_os("WSL_DISTRO_NAME").is_some()
    {
        return true;
    }

    match termini::TermInfo::from_env() {
        Ok(t) => {
            t.extended_cap("RGB").is_some()
                || t.extended_cap("Tc").is_some()
                || (t.extended_cap("setrgbf").is_some() && t.extended_cap("setrgbb").is_some())
        }
        Err(_) => false,
    }
}

/// Function used for filtering dir entries in the various file pickers.
fn filter_picker_entry(entry: &DirEntry, root: &Path, dedup_symlinks: bool) -> bool {
    // We always want to ignore popular VCS directories, otherwise if
    // `ignore` is turned off, we end up with a lot of noise
    // in our picker.
    if matches!(
        entry.file_name().to_str(),
        Some(".git" | ".pijul" | ".jj" | ".hg" | ".svn")
    ) {
        return false;
    }

    // We also ignore symlinks that point inside the current directory
    // if `dedup_links` is enabled.
    if dedup_symlinks && entry.path_is_symlink() {
        return entry
            .path()
            .canonicalize()
            .ok()
            .is_some_and(|path| !path.starts_with(root));
    }

    true
}

/// Opens URL in external program; completes with a typed task event for the main loop.
pub(crate) fn open_external_url_task_event(
    url: Url,
) -> impl Future<Output = Result<crate::runtime::RuntimeTaskEvent, anyhow::Error>> + Send + 'static
{
    let commands = open::commands(url.as_str());
    async move {
        for cmd in commands {
            let mut command: tokio::process::Command = cmd.into();
            if command.output().await.is_ok() {
                return Ok(crate::runtime::RuntimeTaskEvent::Stub);
            }
        }
        Ok(crate::runtime::RuntimeTaskEvent::SetEditorError {
            message: "Opening URL in external program failed".to_owned(),
        })
    }
}
