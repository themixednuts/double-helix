use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Instant,
};

use helix_vcs::FileChange;
use helix_view::{
    modal_text::ModalTextSelection as LabelSelection,
    model::{FocusTarget, PanelSide, PanelSize, TreePanelModel, TreePanelNode},
    Editor,
};

use crate::compositor::Context;

use super::{
    model::{DiagnosticSnapshot, ExplorerRow, RowBuildContext, VcsSnapshot},
    path_ops::{display_name, display_path, relative_display},
    scan::DirectoryScanner,
    selected_path_for_log, FileExplorerPanel, PANEL_WIDTH,
};
impl FileExplorerPanel {
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
            root.as_deref().map(display_path).unwrap_or_else(|| String::from("<unchanged>")),
            cursor,
            follow_current_file,
            self.children_cache.len()
        );
        self.children_cache.clear();
        self.invalidate_vcs_snapshot(editor);
        self.refresh_with_follow(editor, root, cursor, follow_current_file)
    }

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

    pub(super) fn refresh_preserving_tree(
        &mut self,
        editor: &Editor,
        root: Option<PathBuf>,
        cursor: Option<usize>,
    ) -> Result<(), std::io::Error> {
        log::info!(
            "[file_explorer] refresh_request kind=preserve_tree root={} requested_root={} cursor={:?} cache_entries_before={}",
            display_path(&self.root),
            root.as_deref().map(display_path).unwrap_or_else(|| String::from("<unchanged>")),
            cursor,
            self.children_cache.len()
        );
        self.refresh_with_follow(editor, root, cursor, false)
    }

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
                self.children_cache.clear();
                self.invalidate_vcs_snapshot(editor);
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
        if !self.vcs_snapshot.is_current(editor, &self.root) {
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
        self.rows = rows;
        let build_us = build_start.elapsed().as_micros();

        let selection_start = Instant::now();
        if self.rows.is_empty() {
            self.label_selection = LabelSelection::default();
            self.seek_to(0); // empty list: nav resets selection + scroll
        } else {
            let followed_selection = followed_file
                .as_deref()
                .and_then(|path| self.selection_for_path(path));
            let target = cursor
                .or(followed_selection)
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
            followed_file.as_deref().map(display_path).unwrap_or_else(|| String::from("<none>")),
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

    fn invalidate_vcs_snapshot(&mut self, editor: &Editor) {
        self.vcs_snapshot = VcsSnapshot::empty(&self.root, editor.config().file_explorer.vcs);
    }

    pub(super) fn refresh_diagnostic_snapshot(&mut self, editor: &Editor) -> bool {
        let snapshot = DiagnosticSnapshot::from_editor(&self.root, editor);
        if snapshot == self.diagnostic_snapshot {
            return false;
        }
        self.diagnostic_snapshot = snapshot;
        true
    }

    pub fn apply_vcs_snapshot(
        &mut self,
        editor: &Editor,
        root: PathBuf,
        changes: Vec<FileChange>,
    ) -> Result<(), std::io::Error> {
        let start = Instant::now();
        let root = helix_stdx::path::normalize(root);
        if root != self.root {
            log::info!(
                "[file_explorer] vcs_snapshot phase=ignored root={} current_root={} changes={} elapsed_us={}",
                display_path(&root),
                display_path(&self.root),
                changes.len(),
                start.elapsed().as_micros()
            );
            return Ok(());
        }

        if editor.config().file_explorer.vcs {
            self.vcs_snapshot = VcsSnapshot::from_changes(&root, changes);
        } else {
            self.invalidate_vcs_snapshot(editor);
        }
        log::info!(
            "[file_explorer] vcs_snapshot phase=applied root={} entries={} elapsed_us={}",
            display_path(&root),
            self.vcs_snapshot.len(),
            start.elapsed().as_micros()
        );
        self.refresh_preserving_tree(editor, None, Some(self.selection))
    }

    fn followed_file(&self, editor: &Editor) -> Option<PathBuf> {
        let view = editor.tree.try_get(editor.tree.focus)?;
        let doc = editor.document(view.doc)?;
        let path = doc.path()?;
        let path = helix_stdx::path::normalize(path);
        path.starts_with(&self.root).then_some(path)
    }

    fn expand_to_path(&mut self, path: &Path) {
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

    fn select_path_or_index(&mut self, path: &Path, fallback: usize) {
        if self.rows.is_empty() {
            self.label_selection = LabelSelection::default();
            self.seek_to(0);
            return;
        }

        let path = helix_stdx::path::normalize(path);
        let target = self
            .selection_for_path(&path)
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

        model.root = self.root.clone();
        model.items = self
            .rows
            .iter()
            .map(|row| TreePanelNode {
                label: self.label_for(row),
                path: Some(row.path.clone()),
                is_dir: row.is_dir,
                depth: row.depth,
                expanded: row.expanded,
            })
            .collect();
        model.selection = (!self.rows.is_empty()).then_some(self.selection);
    }

    fn label_for(&self, row: &ExplorerRow) -> String {
        row.label.clone()
    }

    pub(super) fn refresh_current(&mut self, editor: &Editor) {
        self.children_cache.clear();
        self.invalidate_vcs_snapshot(editor);
        if let Err(err) = self.refresh_preserving_tree(editor, None, Some(self.selection)) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    pub(super) fn queue_vcs_refresh(&self, cx: &mut Context) {
        crate::runtime::ui::file_explorer::queue_file_explorer_vcs_snapshot(
            cx.editor,
            cx.ingress.clone(),
            self.root.clone(),
        );
    }
}
