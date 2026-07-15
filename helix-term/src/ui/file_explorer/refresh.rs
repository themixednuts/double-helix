use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[cfg(test)]
use helix_vcs::FileChange;
use helix_view::{
    editor::FileExplorerConfig,
    modal_text::ModalTextSelection as LabelSelection,
    model::{FocusTarget, PanelSide, PanelSize, TreePanelModel, TreePanelNode},
    Editor,
};

use crate::compositor::Context;

use super::{
    model::{DiagnosticSnapshot, ExplorerRow, RowBuildContext, VcsSnapshot},
    path_ops::{display_name, display_path, relative_display},
    scan::{DirectoryScanner, ExplorerChild},
    selected_path_for_log, FileExplorerPanel, PANEL_WIDTH,
};

pub(crate) struct FileExplorerTreeWork {
    generation: u64,
    root: PathBuf,
    expanded_dirs: HashSet<PathBuf>,
    config: FileExplorerConfig,
    vcs: VcsSnapshot,
    diagnostics: DiagnosticSnapshot,
    children_cache: HashMap<PathBuf, Vec<ExplorerChild>>,
    cursor: Option<usize>,
    select_path: Option<PathBuf>,
    followed_file: Option<PathBuf>,
    original_selection: usize,
    original_selection_path: Option<PathBuf>,
}

pub(crate) struct PreparedFileExplorerTree {
    pub(crate) generation: u64,
    pub(crate) root: PathBuf,
    expanded_dirs: HashSet<PathBuf>,
    rows: Vec<ExplorerRow>,
    children_cache: HashMap<PathBuf, Vec<ExplorerChild>>,
    cursor: Option<usize>,
    select_path: Option<PathBuf>,
    followed_file: Option<PathBuf>,
    original_selection: usize,
    original_selection_path: Option<PathBuf>,
}

impl FileExplorerTreeWork {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn execute(mut self) -> Result<PreparedFileExplorerTree, std::io::Error> {
        let scanner = DirectoryScanner::new(&self.config);
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(canonical_root) = self.root.canonicalize() {
            seen.insert(canonical_root);
        }
        let root_expanded = self.expanded_dirs.contains(&self.root);
        rows.push(ExplorerRow {
            path: self.root.clone(),
            label: display_name(&self.root),
            is_dir: true,
            depth: 0,
            expanded: root_expanded,
            is_last: true,
            ancestor_last: Vec::new(),
            vcs_status: self.vcs.status(&self.root),
            diagnostic_status: self.diagnostics.status(&self.root),
        });
        if root_expanded {
            let mut build = RowBuildContext {
                scanner,
                vcs: &self.vcs,
                diagnostics: &self.diagnostics,
                seen: &mut seen,
                rows: &mut rows,
                children_cache: &mut self.children_cache,
                cache_hits: 0,
                cache_misses: 0,
                scan_us: 0,
                scanned_children: 0,
            };
            FileExplorerPanel::collect_rows(&self.root, 1, &[], &self.expanded_dirs, &mut build)?;
        }
        Ok(PreparedFileExplorerTree {
            generation: self.generation,
            root: self.root,
            expanded_dirs: self.expanded_dirs,
            rows,
            children_cache: self.children_cache,
            cursor: self.cursor,
            select_path: self.select_path,
            followed_file: self.followed_file,
            original_selection: self.original_selection,
            original_selection_path: self.original_selection_path,
        })
    }
}

impl FileExplorerPanel {
    #[cfg(any(test, feature = "storybook"))]
    pub fn refresh(
        &mut self,
        editor: &Editor,
        root: Option<PathBuf>,
        cursor: Option<usize>,
    ) -> Result<(), std::io::Error> {
        let follow_current_file = cursor.is_none();
        log::info!(
            "[file_explorer] refresh_request kind=external root={} requested_root={} cursor={:?} follow_current_file={} cache_entries_before={}",
            display_path(&self.root),
            root.as_deref()
                .map(display_path)
                .unwrap_or_else(|| String::from("<unchanged>")),
            cursor,
            follow_current_file,
            self.children_cache.len()
        );
        self.children_cache.clear();
        self.invalidate_vcs_snapshot(editor);
        self.refresh_with_follow(editor, root, cursor, follow_current_file)
    }

    #[cfg(test)]
    pub fn refresh_selecting_path(
        &mut self,
        editor: &Editor,
        root: Option<PathBuf>,
        path: &Path,
        fallback_cursor: usize,
    ) -> Result<(), std::io::Error> {
        self.refresh(editor, root, Some(fallback_cursor))?;
        self.select_path_or_index(path, fallback_cursor);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn refresh_preserving_tree(
        &mut self,
        editor: &Editor,
        root: Option<PathBuf>,
        cursor: Option<usize>,
    ) -> Result<(), std::io::Error> {
        log::info!(
            "[file_explorer] refresh_request kind=preserve_tree root={} requested_root={} cursor={:?} cache_entries_before={}",
            display_path(&self.root),
            root.as_deref()
                .map(display_path)
                .unwrap_or_else(|| String::from("<unchanged>")),
            cursor,
            self.children_cache.len()
        );
        self.refresh_with_follow(editor, root, cursor, false)
    }

    #[cfg(any(test, feature = "storybook"))]
    fn refresh_with_follow(
        &mut self,
        editor: &Editor,
        root: Option<PathBuf>,
        cursor: Option<usize>,
        follow_current_file: bool,
    ) -> Result<(), std::io::Error> {
        let refresh_start = Instant::now();
        let original_root = self.root.clone();
        let original_selection = self.selection;
        let original_selection_path = self.rows.get(self.selection).map(|row| row.path.clone());
        let original_rows = self.rows.len();
        let original_cache_entries = self.children_cache.len();
        let mut root_changed = false;
        if let Some(root) = root {
            let root = helix_stdx::path::normalize(&root);
            if root != self.root {
                root_changed = true;
                self.root = root.clone();
                self.expanded_dirs.clear();
                self.expanded_dirs.insert(root);
                self.search_saved_expanded_dirs = None;
                self.children_cache.clear();
                self.invalidate_vcs_snapshot(editor);
                self.prewarm_search_index(editor);
            }
        }

        let followed_file = follow_current_file
            .then(|| self.followed_file(editor))
            .flatten();
        if let Some(path) = &followed_file {
            self.expand_to_path(path);
        }
        let follow_us = refresh_start.elapsed().as_micros();
        let vcs_start = Instant::now();
        if !self
            .vcs_snapshot
            .is_current_for(&self.root, self.config.vcs)
        {
            self.invalidate_vcs_snapshot(editor);
        }
        let vcs_us = vcs_start.elapsed().as_micros();

        let diagnostics_start = Instant::now();
        self.refresh_diagnostic_snapshot(editor);
        let diagnostics_us = diagnostics_start.elapsed().as_micros();

        let build_start = Instant::now();
        let config = editor.config();
        let scanner = DirectoryScanner::new(&config.file_explorer);
        let root = self.root.clone();
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(canonical_root) = root.canonicalize() {
            seen.insert(canonical_root);
        }
        let root_expanded = self.expanded_dirs.contains(&root);
        rows.push(ExplorerRow {
            path: root.clone(),
            label: display_name(&root),
            is_dir: true,
            depth: 0,
            expanded: root_expanded,
            is_last: true,
            ancestor_last: Vec::new(),
            vcs_status: self.vcs_snapshot.status(&root),
            diagnostic_status: self.diagnostic_snapshot.status(&root),
        });
        if root_expanded {
            let mut build = RowBuildContext {
                scanner,
                vcs: &self.vcs_snapshot,
                diagnostics: &self.diagnostic_snapshot,
                seen: &mut seen,
                rows: &mut rows,
                children_cache: &mut self.children_cache,
                cache_hits: 0,
                cache_misses: 0,
                scan_us: 0,
                scanned_children: 0,
            };
            Self::collect_rows(&root, 1, &[], &self.expanded_dirs, &mut build)?;
            log::info!(
                "[file_explorer] tree_build root={} cache_hits={} cache_misses={} scanned_children={} scan_us={} rows_so_far={}",
                display_path(&root),
                build.cache_hits,
                build.cache_misses,
                build.scanned_children,
                build.scan_us,
                build.rows.len()
            );
        }
        self.all_rows = rows.into();
        if self.rebuild_diagnostic_snapshot(editor) {
            self.sync_row_diagnostics();
        }
        self.apply_search_filter(editor);
        let build_us = build_start.elapsed().as_micros();

        let selection_start = Instant::now();
        if self.rows.is_empty() {
            self.label_selection = LabelSelection::default();
            self.seek_to(0); // empty list: nav resets selection + scroll
        } else {
            let followed_selection = followed_file
                .as_deref()
                .and_then(|path| self.selection_for_path(path));
            let restored_selection = original_selection_path
                .as_deref()
                .and_then(|path| self.selection_for_path(path));
            let target = cursor
                .or(followed_selection)
                .or(restored_selection)
                .unwrap_or(self.selection)
                .min(self.rows.len() - 1);
            self.seek_to(target);
            self.clamp_label_selection();
        }
        let selection_us = selection_start.elapsed().as_micros();
        log::info!(
            "[file_explorer] refresh_done root={} previous_root={} root_changed={} follow_current_file={} followed_file={} rows_before={} rows_after={} selection_before={} selection_after={} selected={} expanded_dirs={} cache_entries_before={} cache_entries_after={} vcs_entries={} diagnostic_entries={} follow_us={} vcs_us={} diagnostics_us={} build_us={} selection_us={} total_us={}",
            display_path(&self.root),
            display_path(&original_root),
            root_changed,
            follow_current_file,
            followed_file
                .as_deref()
                .map(display_path)
                .unwrap_or_else(|| String::from("<none>")),
            original_rows,
            self.rows.len(),
            original_selection,
            self.selection,
            selected_path_for_log(&self.rows, self.selection),
            self.expanded_dirs.len(),
            original_cache_entries,
            self.children_cache.len(),
            self.vcs_snapshot.len(),
            self.diagnostic_snapshot.len(),
            follow_us,
            vcs_us,
            diagnostics_us,
            build_us,
            selection_us,
            refresh_start.elapsed().as_micros()
        );
        Ok(())
    }

    pub(crate) fn prepare_tree_refresh(
        &mut self,
        editor: &Editor,
        root: Option<PathBuf>,
        cursor: Option<usize>,
        select_path: Option<PathBuf>,
        follow_current_file: bool,
        clear_cache: bool,
    ) -> FileExplorerTreeWork {
        let original_selection = self.selection;
        let original_selection_path = self.rows.get(self.selection).map(|row| row.path.clone());
        if clear_cache {
            self.children_cache.clear();
            self.invalidate_vcs_snapshot(editor);
        }
        if let Some(root) = root {
            if root != self.root {
                self.root = root.clone();
                self.expanded_dirs.clear();
                self.expanded_dirs.insert(root);
                self.search_saved_expanded_dirs = None;
                self.children_cache.clear();
                self.invalidate_vcs_snapshot(editor);
                self.prewarm_search_index(editor);
            }
        }
        let followed_file = follow_current_file
            .then(|| self.followed_file(editor))
            .flatten();
        if let Some(path) = &followed_file {
            self.expand_to_path(path);
        }
        if let Some(path) = &select_path {
            self.expand_to_path(path);
        }
        if !self
            .vcs_snapshot
            .is_current_for(&self.root, self.config.vcs)
        {
            self.invalidate_vcs_snapshot(editor);
        }
        self.refresh_diagnostic_snapshot(editor);
        self.tree_generation = self.tree_generation.wrapping_add(1).max(1);
        self.tree_pending = true;

        FileExplorerTreeWork {
            generation: self.tree_generation,
            root: self.root.clone(),
            expanded_dirs: self.expanded_dirs.clone(),
            config: self.config.clone(),
            vcs: self.vcs_snapshot.clone(),
            diagnostics: self.diagnostic_snapshot.clone(),
            children_cache: std::mem::take(&mut self.children_cache),
            cursor,
            select_path,
            followed_file,
            original_selection,
            original_selection_path,
        }
    }

    pub(crate) fn apply_prepared_tree(
        &mut self,
        editor: &Editor,
        prepared: PreparedFileExplorerTree,
    ) -> bool {
        if prepared.generation != self.tree_generation
            || prepared.root != self.root
            || prepared.expanded_dirs != self.expanded_dirs
        {
            log::info!(
                "[file_explorer] tree_apply_skip generation={} current_generation={} root={} current_root={} reason=stale",
                prepared.generation,
                self.tree_generation,
                display_path(&prepared.root),
                display_path(&self.root),
            );
            return false;
        }

        self.tree_pending = false;
        self.children_cache = prepared.children_cache;
        self.all_rows = prepared.rows.into();
        if self.rebuild_diagnostic_snapshot(editor) {
            self.sync_row_diagnostics();
        }
        self.apply_search_filter(editor);
        if self.rows.is_empty() {
            self.label_selection = LabelSelection::default();
            self.seek_to(0);
        } else {
            let followed_selection = prepared
                .followed_file
                .as_deref()
                .and_then(|path| self.selection_for_path(path));
            let requested_selection = prepared
                .select_path
                .as_deref()
                .and_then(|path| self.selection_for_path(path));
            let restored_selection = prepared
                .original_selection_path
                .as_deref()
                .and_then(|path| self.selection_for_path(path));
            let target = requested_selection
                .or(prepared.cursor)
                .or(followed_selection)
                .or(restored_selection)
                .unwrap_or(prepared.original_selection)
                .min(self.rows.len() - 1);
            self.seek_to(target);
            if prepared.select_path.is_some() {
                self.center_selection();
            }
            self.clamp_label_selection();
            self.collapse_label_selection_to_cursor();
        }
        log::info!(
            "[file_explorer] tree_apply_done generation={} root={} rows={} cache_entries={} selection={} selected={}",
            prepared.generation,
            display_path(&self.root),
            self.rows.len(),
            self.children_cache.len(),
            self.selection,
            selected_path_for_log(&self.rows, self.selection),
        );
        true
    }

    fn invalidate_vcs_snapshot(&mut self, _editor: &Editor) {
        self.vcs_snapshot = VcsSnapshot::empty(&self.root, self.config.vcs);
    }

    pub(super) fn refresh_diagnostic_snapshot(&mut self, editor: &Editor) -> bool {
        let revision = editor.diagnostics_revision();
        let enabled = self.config.diagnostics;
        if self.diagnostic_snapshot_revision == revision
            && self.diagnostic_snapshot.is_current(&self.root, enabled)
        {
            return false;
        }

        self.rebuild_diagnostic_snapshot(editor)
    }

    fn rebuild_diagnostic_snapshot(&mut self, editor: &Editor) -> bool {
        let snapshot = DiagnosticSnapshot::from_editor(&self.root, editor, self.config.diagnostics);
        self.diagnostic_snapshot_revision = editor.diagnostics_revision();
        if snapshot == self.diagnostic_snapshot {
            return false;
        }
        self.diagnostic_snapshot = snapshot;
        true
    }

    pub(super) fn sync_row_diagnostics(&mut self) {
        let snapshot = &self.diagnostic_snapshot;
        for row in Arc::make_mut(&mut self.all_rows) {
            row.diagnostic_status = snapshot.status(&row.path);
        }
        for row in Arc::make_mut(&mut self.rows) {
            row.diagnostic_status = snapshot.status(&row.path);
        }
    }

    #[cfg(test)]
    pub fn apply_vcs_snapshot(
        &mut self,
        editor: &Editor,
        root: PathBuf,
        changes: Vec<FileChange>,
    ) -> Result<(), std::io::Error> {
        let snapshot = VcsSnapshot::from_changes(&root, changes);
        if !self.apply_vcs_snapshot_state(editor, root, snapshot) {
            return Ok(());
        }
        self.refresh_preserving_tree(editor, None, Some(self.selection))
    }

    pub(crate) fn apply_vcs_snapshot_state(
        &mut self,
        editor: &Editor,
        root: PathBuf,
        snapshot: VcsSnapshot,
    ) -> bool {
        let start = Instant::now();
        if root != self.root {
            log::info!(
                "[file_explorer] vcs_snapshot phase=ignored root={} current_root={} changes={} elapsed_us={}",
                display_path(&root),
                display_path(&self.root),
                snapshot.len(),
                start.elapsed().as_micros()
            );
            return false;
        }

        if self.config.vcs {
            if !snapshot.is_current_for(&root, true) {
                log::info!(
                    "[file_explorer] vcs_snapshot phase=ignored root={} reason=stale_snapshot elapsed_us={}",
                    display_path(&root),
                    start.elapsed().as_micros()
                );
                return false;
            }
            self.vcs_snapshot = snapshot;
        } else {
            self.invalidate_vcs_snapshot(editor);
        }
        log::info!(
            "[file_explorer] vcs_snapshot phase=applied root={} entries={} elapsed_us={}",
            display_path(&root),
            self.vcs_snapshot.len(),
            start.elapsed().as_micros()
        );
        true
    }

    fn followed_file(&self, editor: &Editor) -> Option<PathBuf> {
        let view = editor.tree.try_get(editor.tree.focus)?;
        let doc = editor.document(view.doc)?;
        let path = doc.path()?;
        let path = path.to_path_buf();
        path.starts_with(&self.root).then_some(path)
    }

    pub(super) fn expand_to_path(&mut self, path: &Path) {
        let mut ancestor = path.parent();
        while let Some(dir) = ancestor {
            if !dir.starts_with(&self.root) {
                break;
            }
            self.expanded_dirs.insert(dir.to_path_buf());
            if dir == self.root {
                break;
            }
            ancestor = dir.parent();
        }
    }

    pub(super) fn collapse_dir_preserving_descendant_state(&mut self, path: &Path) {
        self.expanded_dirs.remove(path);
    }

    fn selection_for_path(&self, path: &Path) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.path == path)
            .or_else(|| {
                self.rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| path.starts_with(&row.path))
                    .max_by_key(|(_, row)| row.depth)
                    .map(|(index, _)| index)
            })
    }

    #[cfg(test)]
    fn select_path_or_index(&mut self, path: &Path, fallback: usize) {
        if self.rows.is_empty() {
            self.label_selection = LabelSelection::default();
            self.seek_to(0);
            return;
        }

        let target = self
            .selection_for_path(path)
            .unwrap_or(fallback)
            .min(self.rows.len() - 1);
        self.seek_to(target);
        self.clamp_label_selection();
        self.collapse_label_selection_to_cursor();
    }

    fn collect_rows(
        root: &Path,
        depth: usize,
        ancestor_last: &[bool],
        expanded_dirs: &HashSet<PathBuf>,
        build: &mut RowBuildContext<'_>,
    ) -> Result<(), std::io::Error> {
        let children = build.children_for(root)?;
        let last = children.len().saturating_sub(1);
        for (index, child) in children.into_iter().enumerate() {
            let expanded = child.is_dir && expanded_dirs.contains(&child.path);
            let is_last = index == last;
            build.rows.push(ExplorerRow {
                path: child.path.clone(),
                label: relative_display(root, &child.path),
                is_dir: child.is_dir,
                depth,
                expanded,
                is_last,
                ancestor_last: ancestor_last.to_vec(),
                vcs_status: build.vcs.status(&child.path),
                diagnostic_status: build.diagnostics.status(&child.path),
            });

            if !expanded {
                continue;
            }

            let canonical = child
                .path
                .canonicalize()
                .unwrap_or_else(|_| child.path.clone());
            if build.seen.insert(canonical) {
                let mut child_ancestors = ancestor_last.to_vec();
                child_ancestors.push(is_last);
                Self::collect_rows(
                    &child.path,
                    depth + 1,
                    &child_ancestors,
                    expanded_dirs,
                    build,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn sync_to_model(&mut self, editor: &mut Editor) {
        let panel_id = match self.model_panel_id {
            Some(id) if editor.model.panels.contains_key(id) => id,
            _ => {
                let existing = editor
                    .model
                    .panels
                    .iter()
                    .find(|(_, panel)| {
                        panel.title == "Files" && panel.content.is::<TreePanelModel>()
                    })
                    .map(|(id, _)| id);
                let id = existing.unwrap_or_else(|| {
                    editor.model.insert_panel(
                        "Files",
                        Box::new(TreePanelModel::default()),
                        PanelSide::Left,
                        PanelSize::fixed(PANEL_WIDTH),
                    )
                });
                self.model_panel_id = Some(id);
                id
            }
        };

        if self.focused {
            editor.model.focus_panel(panel_id);
        } else if editor.model.focus == FocusTarget::Panel(panel_id) {
            editor.model.pop_focus();
        }

        let Some(model) = editor.model.panel_model_mut::<TreePanelModel>(panel_id) else {
            return;
        };

        let selection = (!self.rows.is_empty()).then_some(self.selection);
        let items_current = model.items.len() == self.rows.len()
            && model.items.iter().zip(self.rows.iter()).all(|(item, row)| {
                item.label == row.label
                    && item.path.as_deref() == Some(row.path.as_path())
                    && item.is_dir == row.is_dir
                    && item.depth == row.depth
                    && item.expanded == row.expanded
            });

        if model.root == self.root && model.selection == selection && items_current {
            return;
        }

        model.root.clone_from(&self.root);
        model.items.clear();
        model.items.reserve(self.rows.len());
        model
            .items
            .extend(self.rows.iter().map(|row| TreePanelNode {
                label: row.label.clone(),
                path: Some(row.path.clone()),
                is_dir: row.is_dir,
                depth: row.depth,
                expanded: row.expanded,
            }));
        model.selection = selection;
    }

    #[cfg(test)]
    pub(super) fn refresh_current(&mut self, editor: &Editor) {
        self.children_cache.clear();
        self.invalidate_vcs_snapshot(editor);
        if let Err(err) = self.refresh_preserving_tree(editor, None, Some(self.selection)) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    pub(super) fn queue_refresh_current(&mut self, cx: &mut Context) {
        crate::runtime::ui::file_explorer::queue_file_explorer_tree_refresh(
            self,
            cx.editor,
            cx.ingress.clone(),
            None,
            Some(self.selection),
            None,
            false,
            true,
        );
    }

    pub(super) fn queue_vcs_refresh(&self, cx: &mut Context) {
        crate::runtime::ui::file_explorer::queue_file_explorer_vcs_snapshot(
            cx.editor,
            cx.ingress.clone(),
            self.root.clone(),
        );
    }
}
