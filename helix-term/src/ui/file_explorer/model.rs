use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Instant,
};

use helix_core::diagnostic::Severity as DiagnosticSeverity;
use helix_vcs::FileChange;
use helix_view::{editor::WorkspaceDiagnosticCounts, icons::Icons, theme::Style, Editor};

use super::{
    path_ops::display_path,
    scan::{DirectoryScanner, ExplorerChild},
};

/// Explorer rows are the current visible tree, not the full filesystem.
#[derive(Clone, Debug)]
pub(super) struct ExplorerRow {
    pub(super) path: PathBuf,
    pub(super) label: String,
    pub(super) is_dir: bool,
    pub(super) depth: usize,
    pub(super) expanded: bool,
    pub(super) is_last: bool,
    pub(super) ancestor_last: Vec<bool>,
    pub(super) vcs_status: Option<VcsStatus>,
    pub(super) diagnostic_status: Option<DiagnosticStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VcsStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Conflict,
}

impl VcsStatus {
    fn from_change(change: &FileChange) -> Self {
        match change {
            FileChange::Untracked { .. } => Self::Added,
            FileChange::Modified { .. } => Self::Modified,
            FileChange::Conflict { .. } => Self::Conflict,
            FileChange::Deleted { .. } => Self::Deleted,
            FileChange::Renamed { .. } => Self::Renamed,
        }
    }

    const fn priority(self) -> u8 {
        match self {
            Self::Added => 1,
            Self::Modified => 2,
            Self::Renamed => 3,
            Self::Deleted => 4,
            Self::Conflict => 5,
        }
    }

    pub(super) fn merge(self, other: Self) -> Self {
        if other.priority() > self.priority() {
            other
        } else {
            self
        }
    }

    pub(super) fn icon(self, icons: &Icons) -> &str {
        fn configured_or<'a>(configured: &'a str, fallback: &'static str) -> &'a str {
            if configured.is_empty() {
                fallback
            } else {
                configured
            }
        }

        match self {
            Self::Added => configured_or(icons.vcs().added(), super::VCS_ADDED_ICON),
            Self::Modified => configured_or(icons.vcs().modified(), super::VCS_MODIFIED_ICON),
            Self::Deleted => configured_or(icons.vcs().removed(), super::VCS_DELETED_ICON),
            Self::Renamed => configured_or(icons.vcs().renamed(), super::VCS_RENAMED_ICON),
            Self::Conflict => configured_or(icons.vcs().conflict(), super::VCS_CONFLICT_ICON),
        }
    }

    pub(super) fn style(
        self,
        added: Style,
        modified: Style,
        deleted: Style,
        renamed: Style,
        conflict: Style,
    ) -> Style {
        match self {
            Self::Added => added,
            Self::Modified => modified,
            Self::Deleted => deleted,
            Self::Renamed => renamed,
            Self::Conflict => conflict,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct VcsSnapshot {
    root: PathBuf,
    enabled: bool,
    statuses: HashMap<PathBuf, VcsStatus>,
}

impl VcsSnapshot {
    pub(crate) fn empty(root: &Path, enabled: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            enabled,
            statuses: HashMap::new(),
        }
    }

    pub(crate) fn from_changes(root: &Path, changes: impl IntoIterator<Item = FileChange>) -> Self {
        let root = helix_stdx::path::normalize(root);
        let mut snapshot = Self {
            root: root.clone(),
            enabled: true,
            statuses: HashMap::new(),
        };

        for change in changes {
            let status = VcsStatus::from_change(&change);
            match &change {
                FileChange::Renamed { from_path, to_path } => {
                    snapshot.insert_path_and_ancestors(&root, to_path, status);
                    snapshot.insert_path_and_ancestors(&root, from_path, status);
                }
                _ => snapshot.insert_path_and_ancestors(&root, change.path(), status),
            }
        }

        snapshot
    }

    pub(super) fn status(&self, path: &Path) -> Option<VcsStatus> {
        if !self.enabled {
            return None;
        }
        self.statuses.get(path).copied()
    }

    pub(crate) fn is_current_for(&self, root: &Path, enabled: bool) -> bool {
        self.root == root && self.enabled == enabled
    }

    pub(super) fn len(&self) -> usize {
        self.statuses.len()
    }

    fn insert_path_and_ancestors(&mut self, root: &Path, path: &Path, status: VcsStatus) {
        let path = helix_stdx::path::normalize(path);
        if !path.starts_with(root) {
            return;
        }

        let mut cursor = Some(path.as_path());
        while let Some(path) = cursor {
            if path.starts_with(root) {
                self.insert_status(path, status);
            }
            if path == root {
                break;
            }
            cursor = path.parent();
        }
    }

    fn insert_status(&mut self, path: &Path, status: VcsStatus) {
        self.statuses
            .entry(path.to_path_buf())
            .and_modify(|existing| *existing = existing.merge(status))
            .or_insert(status);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DiagnosticStatus {
    pub(super) severity: DiagnosticSeverity,
    pub(super) count: usize,
}

impl DiagnosticStatus {
    fn from_counts(counts: WorkspaceDiagnosticCounts) -> Option<Self> {
        let severity = if counts.errors != 0 {
            DiagnosticSeverity::Error
        } else if counts.warnings != 0 {
            DiagnosticSeverity::Warning
        } else if counts.info != 0 {
            DiagnosticSeverity::Info
        } else if counts.hints != 0 {
            DiagnosticSeverity::Hint
        } else {
            return None;
        };
        Some(Self {
            severity,
            count: counts.total() as usize,
        })
    }

    pub(super) fn icon(self, icons: &Icons) -> &str {
        match self.severity {
            DiagnosticSeverity::Error => icons.diagnostic().error(),
            DiagnosticSeverity::Warning => icons.diagnostic().warning(),
            DiagnosticSeverity::Info => icons.diagnostic().info(),
            DiagnosticSeverity::Hint => icons.diagnostic().hint(),
        }
    }

    pub(super) const fn style(
        self,
        hint: Style,
        info: Style,
        warning: Style,
        error: Style,
    ) -> Style {
        match self.severity {
            DiagnosticSeverity::Error => error,
            DiagnosticSeverity::Warning => warning,
            DiagnosticSeverity::Info => info,
            DiagnosticSeverity::Hint => hint,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct DiagnosticSnapshot {
    root: PathBuf,
    enabled: bool,
    statuses: HashMap<PathBuf, DiagnosticStatus>,
}

impl DiagnosticSnapshot {
    pub(super) fn empty(root: &Path, enabled: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            enabled,
            statuses: HashMap::new(),
        }
    }

    pub(super) fn from_editor(root: &Path, editor: &Editor, enabled: bool) -> Self {
        if !enabled {
            return Self::empty(root, false);
        }

        let mut snapshot = Self {
            root: root.to_path_buf(),
            enabled,
            statuses: HashMap::new(),
        };

        for (path, summary) in editor.workspace_diagnostic_path_summaries_under(root) {
            if let Some(status) = DiagnosticStatus::from_counts(summary) {
                snapshot.statuses.insert(path.to_path_buf(), status);
            }
        }

        snapshot
    }

    pub(super) fn is_current(&self, root: &Path, enabled: bool) -> bool {
        self.root == root && self.enabled == enabled
    }

    pub(super) fn status(&self, path: &Path) -> Option<DiagnosticStatus> {
        if !self.enabled {
            return None;
        }
        self.statuses.get(path).copied()
    }

    pub(super) fn len(&self) -> usize {
        self.statuses.len()
    }
}

pub(super) struct RowBuildContext<'a> {
    pub(super) scanner: DirectoryScanner<'a>,
    pub(super) vcs: &'a VcsSnapshot,
    pub(super) diagnostics: &'a DiagnosticSnapshot,
    pub(super) seen: &'a mut HashSet<PathBuf>,
    pub(super) rows: &'a mut Vec<ExplorerRow>,
    pub(super) children_cache: &'a mut HashMap<PathBuf, Vec<ExplorerChild>>,
    pub(super) cache_hits: usize,
    pub(super) cache_misses: usize,
    pub(super) scan_us: u128,
    pub(super) scanned_children: usize,
}

impl RowBuildContext<'_> {
    pub(super) fn children_for(
        &mut self,
        root: &Path,
    ) -> Result<Vec<ExplorerChild>, std::io::Error> {
        if let Some(children) = self.children_cache.get(root) {
            self.cache_hits += 1;
            log::info!(
                "[file_explorer] tree_cache phase=hit root={} children={}",
                display_path(root),
                children.len()
            );
            return Ok(children.clone());
        }

        let scan_start = Instant::now();
        let children = self.scanner.children(root)?;
        let scan_elapsed = scan_start.elapsed();
        self.cache_misses += 1;
        self.scan_us += scan_elapsed.as_micros();
        self.scanned_children += children.len();
        log::info!(
            "[file_explorer] tree_cache phase=miss root={} children={} scan_us={}",
            display_path(root),
            children.len(),
            scan_elapsed.as_micros()
        );
        self.children_cache
            .insert(root.to_path_buf(), children.clone());
        Ok(children)
    }
}
