//! FFF_SCAN_OPTIONS_BLOCKER: Helix needs file-picker scan semantics finer than
//! upstream's coarse options. Upstream hard-codes the walker policy
//! (`hidden(!is_git_repo)`, every ignore source on, no depth limit, no custom
//! ignore files); Helix threads [`FilePickerScanOptions`] into the filesystem
//! walker (`walk::walk_collect_files`) and into the background watcher's event
//! filter so both agree on what belongs in the index.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SymlinkTargetScope {
    #[default]
    Any,
    BaseDirectory,
}

#[derive(Debug, Clone)]
pub struct FilePickerScanOptions {
    pub hidden: bool,
    pub parents: bool,
    pub ignore: bool,
    pub git_ignore: bool,
    pub git_global: bool,
    pub git_exclude: bool,
    pub follow_links: bool,
    pub max_depth: Option<usize>,
    pub custom_ignore_files: Box<[PathBuf]>,
    pub deduplicate_links: bool,
    pub symlink_target_scope: SymlinkTargetScope,
}

impl Default for FilePickerScanOptions {
    fn default() -> Self {
        Self {
            hidden: true,
            parents: true,
            ignore: true,
            git_ignore: true,
            git_global: true,
            git_exclude: true,
            follow_links: false,
            max_depth: None,
            custom_ignore_files: Box::default(),
            deduplicate_links: true,
            symlink_target_scope: SymlinkTargetScope::Any,
        }
    }
}

impl FilePickerScanOptions {
    /// Options for walking `dir`, a subtree of `base_path`, so that
    /// `max_depth` keeps counting from `base_path`. Returns `None` when `dir`
    /// itself already lies beyond the depth limit.
    pub(crate) fn for_subtree(&self, base_path: &Path, dir: &Path) -> Option<Self> {
        let Some(max_depth) = self.max_depth else {
            return Some(self.clone());
        };
        let Ok(relative) = dir.strip_prefix(base_path) else {
            return Some(self.clone());
        };
        let depth = relative.components().count();
        let remaining = max_depth.checked_sub(depth)?;
        Some(Self {
            max_depth: Some(remaining),
            ..self.clone()
        })
    }

    /// Structural exclusions that do not depend on ignore files: `.git`
    /// internals, hidden components, depth, and symlink policy. Mirrors what
    /// the walker filters for an existing `path` below `base_path`.
    pub(crate) fn excludes_path(&self, path: &Path, base_path: &Path) -> bool {
        if crate::watch::is_git_file(path) {
            return true;
        }

        if self.hidden && has_hidden_component(path, base_path) {
            return true;
        }

        if let Some(max_depth) = self.max_depth
            && let Ok(relative) = path.strip_prefix(base_path)
            && relative.components().count() > max_depth
        {
            return true;
        }

        let is_symlink =
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink());
        if is_symlink {
            if !self.follow_links {
                return true;
            }
            if !is_supported_symlink(
                path,
                base_path,
                self.deduplicate_links,
                self.symlink_target_scope,
            ) {
                return true;
            }
        }

        false
    }
}

/// Walker entry filter: drops `.git` internals and unsupported symlinks.
pub(crate) fn is_supported_entry(
    path: &Path,
    base_path: &Path,
    deduplicate_links: bool,
    symlink_target_scope: SymlinkTargetScope,
    is_symlink: bool,
) -> bool {
    if crate::watch::is_git_file(path) {
        return false;
    }

    if is_symlink {
        return is_supported_symlink(path, base_path, deduplicate_links, symlink_target_scope);
    }

    true
}

pub(crate) fn is_supported_symlink(
    path: &Path,
    base_path: &Path,
    deduplicate_links: bool,
    target_scope: SymlinkTargetScope,
) -> bool {
    let Ok(target) = crate::path_utils::canonicalize(path) else {
        return false;
    };
    is_supported_symlink_target(&target, base_path, deduplicate_links, target_scope)
}

fn is_supported_symlink_target(
    target: &Path,
    base_path: &Path,
    deduplicate_links: bool,
    target_scope: SymlinkTargetScope,
) -> bool {
    let within_base = target.starts_with(base_path);
    if target_scope == SymlinkTargetScope::BaseDirectory && !within_base {
        return false;
    }
    !(deduplicate_links && within_base)
}

pub(crate) fn has_hidden_component(path: &Path, base_path: &Path) -> bool {
    let relative = path.strip_prefix(base_path).unwrap_or(path);
    relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|segment| segment.starts_with('.') && segment != "." && segment != "..")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_target_policy_separates_scope_from_deduplication() {
        let base = Path::new("workspace");
        let internal = base.join("target");
        let external = Path::new("outside").join("target");

        assert!(is_supported_symlink_target(
            &internal,
            base,
            false,
            SymlinkTargetScope::BaseDirectory,
        ));
        assert!(!is_supported_symlink_target(
            &external,
            base,
            false,
            SymlinkTargetScope::BaseDirectory,
        ));
        assert!(is_supported_symlink_target(
            &external,
            base,
            false,
            SymlinkTargetScope::Any,
        ));
        assert!(!is_supported_symlink_target(
            &internal,
            base,
            true,
            SymlinkTargetScope::Any,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_scope_keeps_workspace_scans_contained() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("inside.txt"), "inside").unwrap();
        std::fs::write(outside.path().join("outside.txt"), "outside").unwrap();
        let internal_link = workspace.path().join("internal-link");
        let external_link = workspace.path().join("external-link");
        symlink(workspace.path().join("inside.txt"), &internal_link).unwrap();
        symlink(outside.path().join("outside.txt"), &external_link).unwrap();
        let base = crate::path_utils::canonicalize(workspace.path()).unwrap();

        assert!(is_supported_symlink(
            &internal_link,
            &base,
            false,
            SymlinkTargetScope::BaseDirectory,
        ));
        assert!(!is_supported_symlink(
            &external_link,
            &base,
            false,
            SymlinkTargetScope::BaseDirectory,
        ));
        assert!(is_supported_symlink(
            &external_link,
            &base,
            false,
            SymlinkTargetScope::Any,
        ));
        assert!(!is_supported_symlink(
            &internal_link,
            &base,
            true,
            SymlinkTargetScope::BaseDirectory,
        ));
    }

    #[test]
    fn subtree_options_keep_depth_relative_to_base() {
        let base = Path::new("workspace");
        let options = FilePickerScanOptions {
            max_depth: Some(3),
            ..Default::default()
        };

        let nested = options.for_subtree(base, &base.join("a")).unwrap();
        assert_eq!(nested.max_depth, Some(2));
        let at_limit = options.for_subtree(base, &base.join("a/b/c")).unwrap();
        assert_eq!(at_limit.max_depth, Some(0));
        assert!(options.for_subtree(base, &base.join("a/b/c/d")).is_none());

        let unlimited = FilePickerScanOptions::default()
            .for_subtree(base, &base.join("a/b/c/d"))
            .unwrap();
        assert_eq!(unlimited.max_depth, None);
    }

    #[test]
    fn structural_exclusions_follow_scan_options() {
        let base = Path::new("workspace");
        let options = FilePickerScanOptions {
            max_depth: Some(2),
            ..Default::default()
        };

        assert!(options.excludes_path(&base.join(".git/config"), base));
        assert!(options.excludes_path(&base.join(".hidden/file.rs"), base));
        assert!(options.excludes_path(&base.join("a/b/c.rs"), base));
        assert!(!options.excludes_path(&base.join("a/b.rs"), base));

        let show_hidden = FilePickerScanOptions {
            hidden: false,
            ..Default::default()
        };
        assert!(!show_hidden.excludes_path(&base.join(".hidden/file.rs"), base));
    }
}
