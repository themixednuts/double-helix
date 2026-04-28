//! `helix_vcs` provides types for working with diffs from a Version Control System (VCS).
//! Git provides full diff support. Changed-file status can use Git, Jujutsu, or automatic
//! detection that checks Jujutsu before falling back to Git.

use anyhow::{anyhow, bail, Result};
use arc_swap::ArcSwap;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(feature = "git")]
mod git;
#[cfg(feature = "git")]
pub use git::blame::FileBlame;

mod jj;

mod diff;

pub use diff::{DiffHandle, Hunk};

mod status;

pub use status::FileChange;

/// Contains all active diff providers. Diff providers are compiled in via features when they
/// need optional dependencies.
#[derive(Clone, Debug)]
pub struct DiffProviderRegistry {
    provider: VcsProvider,
    providers: Vec<DiffProvider>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum VcsProvider {
    #[default]
    Auto,
    Git,
    Jj,
    None,
}

impl DiffProviderRegistry {
    pub fn new(provider: VcsProvider) -> Self {
        let providers = vec![
            #[cfg(feature = "git")]
            DiffProvider::Git,
            DiffProvider::None,
        ];
        Self {
            provider,
            providers,
        }
    }

    pub const fn provider(&self) -> VcsProvider {
        self.provider
    }

    /// Get the given file from the VCS. This provides the unedited document as a "base"
    /// for a diff to be created.
    pub fn get_diff_base(&self, file: &Path) -> Option<Vec<u8>> {
        if !matches!(self.provider, VcsProvider::Auto | VcsProvider::Git) {
            return None;
        }

        self.providers
            .iter()
            .find_map(|provider| match provider.get_diff_base(file) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to open diff base for {}", file.display());
                    None
                }
            })
    }

    /// Get the current name of the current [HEAD](https://stackoverflow.com/questions/2304087/what-is-head-in-git).
    pub fn get_current_head_name(&self, file: &Path) -> Option<Arc<ArcSwap<Box<str>>>> {
        if !matches!(self.provider, VcsProvider::Auto | VcsProvider::Git) {
            return None;
        }

        self.providers
            .iter()
            .find_map(|provider| match provider.get_current_head_name(file) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to obtain current head name for {}", file.display());
                    None
                }
            })
    }

    /// Fire-and-forget changed file iteration. Runs everything in a background task. Keeps
    /// iteration until `on_change` returns `false`.
    pub fn for_each_changed_file(
        self,
        cwd: PathBuf,
        f: impl FnMut(Result<FileChange>) -> bool + Send + 'static,
    ) {
        tokio::task::spawn_blocking(move || {
            let mut f = f;
            if let Err(err) = self.for_each_changed_file_sync(&cwd, &mut f) {
                f(Err(err));
            }
        });
    }

    /// Collect changed files synchronously. UI components that render persistent state can use
    /// this to build a snapshot without going through picker injectors.
    pub fn changed_files(&self, cwd: &Path) -> Result<Vec<FileChange>> {
        let mut changes = Vec::new();
        self.for_each_changed_file_sync(cwd, |change| match change {
            Ok(change) => {
                changes.push(change);
                true
            }
            Err(err) => {
                log::debug!("failed to collect changed file: {err:#?}");
                true
            }
        })?;
        Ok(changes)
    }

    fn for_each_changed_file_sync(
        &self,
        cwd: &Path,
        mut f: impl FnMut(Result<FileChange>) -> bool,
    ) -> Result<()> {
        match self.provider {
            VcsProvider::Auto => {
                if jj::for_each_changed_file(cwd, &mut f).is_ok() {
                    return Ok(());
                }
                self.for_each_diff_provider_changed_file(cwd, f)
            }
            VcsProvider::Git => self.for_each_diff_provider_changed_file(cwd, f),
            VcsProvider::Jj => jj::for_each_changed_file(cwd, f),
            VcsProvider::None => Ok(()),
        }
    }

    fn for_each_diff_provider_changed_file(
        &self,
        cwd: &Path,
        mut f: impl FnMut(Result<FileChange>) -> bool,
    ) -> Result<()> {
        self.providers
            .iter()
            .find_map(
                |provider| match provider.for_each_changed_file(cwd, &mut f) {
                    Ok(()) => Some(Ok(())),
                    Err(err) => {
                        log::debug!("{err:#?}");
                        log::debug!("failed to collect changed files for {}", cwd.display());
                        None
                    }
                },
            )
            .unwrap_or_else(|| Err(anyhow!("no diff provider returns success")))
    }
}

impl Default for DiffProviderRegistry {
    fn default() -> Self {
        Self::new(VcsProvider::Auto)
    }
}

/// A union type that includes all types that implement [DiffProvider]. We need this type to allow
/// cloning [DiffProviderRegistry] as `Clone` cannot be used in trait objects.
///
/// `Copy` is simply to ensure the `clone()` call is the simplest it can be.
#[derive(Copy, Clone, Debug)]
enum DiffProvider {
    #[cfg(feature = "git")]
    Git,
    None,
}

impl DiffProvider {
    fn get_diff_base(&self, _file: &Path) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_diff_base(_file),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn get_current_head_name(&self, _file: &Path) -> Result<Arc<ArcSwap<Box<str>>>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_current_head_name(_file),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn for_each_changed_file(
        &self,
        _cwd: &Path,
        _f: impl FnMut(Result<FileChange>) -> bool,
    ) -> Result<()> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::for_each_changed_file(_cwd, _f),
            Self::None => bail!("No diff support compiled in"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DiffProviderRegistry, VcsProvider};

    #[test]
    fn default_provider_is_auto() {
        assert_eq!(
            DiffProviderRegistry::default().provider(),
            VcsProvider::Auto
        );
    }

    #[test]
    fn explicit_none_provider_returns_no_changed_files() {
        let registry = DiffProviderRegistry::new(VcsProvider::None);

        assert_eq!(
            registry.changed_files(std::path::Path::new(".")).unwrap(),
            []
        );
    }
}
