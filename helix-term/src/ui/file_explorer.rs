use std::{
    collections::HashSet,
    error::Error as _,
    path::{Path, PathBuf},
};

use helix_view::{
    editor::{Action, EditingEngineConfig},
    graphics::{Modifier, Rect},
    icons::ICONS,
    input::{KeyEvent, MouseButton, MouseEventKind},
    model::{FocusTarget, PanelId, PanelSide, PanelSize, TreePanelModel, TreePanelNode},
    theme::Style,
    traits::{Bounded, Focusable, Scrollable},
    Editor,
};
use tui::{buffer::Buffer as Surface, text::Span};

use crate::{
    alt, component_traits,
    compositor::{Component, Context, Event, EventResult, PostAction, RenderContext},
    ctrl, key,
    runtime::{ui::command::FileExplorerCommand, UiCommand},
};

use super::prompt::Movement;

pub const ID: &str = "file-explorer-panel";

const HEADER_ROWS: u16 = 2;
const PANEL_WIDTH: u16 = 34;

/// Explorer rows are the current visible tree, not the full filesystem.
#[derive(Clone, Debug)]
struct ExplorerRow {
    path: PathBuf,
    is_dir: bool,
    depth: usize,
    expanded: bool,
}

pub struct FileExplorerPanel {
    root: PathBuf,
    rows: Vec<ExplorerRow>,
    expanded_dirs: HashSet<PathBuf>,
    selection: usize,
    scroll: usize,
    area: Rect,
    focused: bool,
    model_panel_id: Option<PanelId>,
}

fn path_prefill(path: &Path) -> String {
    let mut path = path.display().to_string();
    if !path.ends_with(std::path::MAIN_SEPARATOR) && !path.ends_with('/') {
        path.push(std::path::MAIN_SEPARATOR);
    }
    path
}

fn selected_cursor(selection: usize) -> u32 {
    u32::try_from(selection).unwrap_or(u32::MAX)
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn directory_children(
    root: &Path,
    editor: &Editor,
) -> Result<Vec<(PathBuf, bool)>, std::io::Error> {
    use ignore::WalkBuilder;

    let config = editor.config();
    let explorer = &config.file_explorer;

    let mut walk_builder = WalkBuilder::new(root);
    let mut content: Vec<(PathBuf, bool)> = walk_builder
        .hidden(explorer.hidden)
        .parents(explorer.parents)
        .ignore(explorer.ignore)
        .follow_links(explorer.follow_symlinks)
        .git_ignore(explorer.git_ignore)
        .git_global(explorer.git_global)
        .git_exclude(explorer.git_exclude)
        .max_depth(Some(1))
        .add_custom_ignore_filename(helix_loader::config_dir().join("ignore"))
        .add_custom_ignore_filename(helix_loader::workspace_ignore_file_name())
        .types(super::get_excluded_types())
        .build()
        .filter_map(|entry| {
            entry
                .map(|entry| {
                    let path = entry.path();
                    let is_dir = path.is_dir();
                    let mut path = path.to_path_buf();
                    if is_dir && path != root && explorer.flatten_dirs {
                        while let Some(single_child_directory) =
                            super::get_child_if_single_dir(&path)
                        {
                            path = single_child_directory;
                        }
                    }
                    (path, is_dir)
                })
                .ok()
                .filter(|entry| entry.0 != root)
        })
        .collect();

    content.sort_by(|(path1, is_dir1), (path2, is_dir2)| (!is_dir1, path1).cmp(&(!is_dir2, path2)));
    Ok(content)
}

fn draw_spans(surface: &mut Surface, mut x: u16, y: u16, width: u16, spans: &[Span<'_>]) {
    let end = x.saturating_add(width);
    for span in spans {
        if x >= end {
            break;
        }
        let remaining = end.saturating_sub(x) as usize;
        let (next_x, _) = surface.set_stringn(x, y, span.content.as_ref(), remaining, span.style);
        if next_x <= x {
            break;
        }
        x = next_x;
    }
}

impl FileExplorerPanel {
    pub fn new(root: PathBuf, editor: &Editor) -> Result<Self, std::io::Error> {
        let root = helix_stdx::path::normalize(&root);
        let mut panel = Self {
            root: root.clone(),
            rows: Vec::new(),
            expanded_dirs: HashSet::from([root]),
            selection: 0,
            scroll: 0,
            area: Rect::default(),
            focused: true,
            model_panel_id: None,
        };
        panel.refresh(editor, None, None)?;
        Ok(panel)
    }

    pub fn refresh(
        &mut self,
        editor: &Editor,
        root: Option<PathBuf>,
        cursor: Option<usize>,
    ) -> Result<(), std::io::Error> {
        if let Some(root) = root {
            let root = helix_stdx::path::normalize(&root);
            if root != self.root {
                self.root = root.clone();
                self.expanded_dirs.clear();
                self.expanded_dirs.insert(root);
            }
        }

        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(canonical_root) = self.root.canonicalize() {
            seen.insert(canonical_root);
        }
        self.collect_rows(editor, &self.root, 0, &mut seen, &mut rows)?;
        self.rows = rows;

        if self.rows.is_empty() {
            self.selection = 0;
            self.scroll = 0;
        } else {
            self.selection = cursor.unwrap_or(self.selection).min(self.rows.len() - 1);
            self.ensure_selection_visible();
        }
        Ok(())
    }

    fn collect_rows(
        &self,
        editor: &Editor,
        root: &Path,
        depth: usize,
        seen: &mut HashSet<PathBuf>,
        rows: &mut Vec<ExplorerRow>,
    ) -> Result<(), std::io::Error> {
        for (path, is_dir) in directory_children(root, editor)? {
            let expanded = is_dir && self.expanded_dirs.contains(&path);
            rows.push(ExplorerRow {
                path: path.clone(),
                is_dir,
                depth,
                expanded,
            });

            if !expanded {
                continue;
            }

            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if seen.insert(canonical) {
                self.collect_rows(editor, &path, depth + 1, seen, rows)?;
            }
        }
        Ok(())
    }

    fn sync_to_model(&mut self, editor: &mut Editor) {
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
        row.path
            .strip_prefix(&self.root)
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| display_name(&row.path))
    }

    fn visible_height(&self) -> usize {
        self.area.height.saturating_sub(HEADER_ROWS) as usize
    }

    fn ensure_selection_visible(&mut self) {
        let viewport = self.visible_height();
        if viewport == 0 {
            return;
        }
        if self.selection < self.scroll {
            self.scroll = self.selection;
        } else if self.selection >= self.scroll + viewport {
            self.scroll = self.selection + 1 - viewport;
        }
    }

    fn selected(&self) -> Option<&ExplorerRow> {
        self.rows.get(self.selection)
    }

    fn selected_base_dir(&self) -> PathBuf {
        self.selected()
            .map(|row| {
                if row.is_dir {
                    row.path.clone()
                } else {
                    row.path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.root.clone())
                }
            })
            .unwrap_or_else(|| self.root.clone())
    }

    fn move_selection_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.selection = self
            .selection
            .saturating_add_signed(delta)
            .min(self.rows.len() - 1);
        self.ensure_selection_visible();
    }

    fn page_by(&mut self, delta: isize) {
        let amount = self.visible_height().max(1) as isize;
        self.move_selection_by(delta.saturating_mul(amount));
    }

    fn select_first(&mut self) {
        self.selection = 0;
        self.ensure_selection_visible();
    }

    fn select_last(&mut self) {
        if !self.rows.is_empty() {
            self.selection = self.rows.len() - 1;
            self.ensure_selection_visible();
        }
    }

    fn toggle_selected_dir(&mut self, editor: &Editor) {
        let Some(row) = self.selected().filter(|row| row.is_dir).cloned() else {
            return;
        };
        if row.expanded {
            self.expanded_dirs.remove(&row.path);
        } else {
            self.expanded_dirs.insert(row.path);
        }
        if let Err(err) = self.refresh(editor, None, Some(self.selection)) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    fn collapse_or_select_parent(&mut self, editor: &Editor) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        if row.is_dir && row.expanded {
            self.expanded_dirs.remove(&row.path);
            if let Err(err) = self.refresh(editor, None, Some(self.selection)) {
                log::error!("failed to refresh file explorer: {err}");
            }
            return;
        }

        if row.depth == 0 {
            return;
        }

        if let Some(parent_index) = self.rows[..self.selection]
            .iter()
            .rposition(|candidate| candidate.depth + 1 == row.depth)
        {
            self.selection = parent_index;
            self.ensure_selection_visible();
        }
    }

    fn root_parent(&mut self, editor: &Editor) {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else {
            return;
        };
        if let Err(err) = self.refresh(editor, Some(parent), Some(0)) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    fn refresh_current(&mut self, editor: &Editor) {
        if let Err(err) = self.refresh(editor, None, Some(self.selection)) {
            log::error!("failed to refresh file explorer: {err}");
        }
    }

    fn open_selected(&mut self, cx: &mut Context, action: Action) {
        let Some(row) = self.selected().cloned() else {
            return;
        };

        if row.is_dir {
            self.toggle_selected_dir(cx.editor);
            return;
        }

        match cx.editor.open(&row.path, action) {
            Ok(_) => {
                self.focused = false;
            }
            Err(err) => {
                let message = err
                    .source()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("unable to open \"{}\"", row.path.display()));
                cx.editor.set_error(message);
            }
        }
    }

    fn yank_selected_path(&self, cx: &mut Context) {
        let Some(row) = self.selected() else {
            return;
        };
        let register = cx
            .editor
            .frontend()
            .focused_modal_input
            .selected_register
            .unwrap_or(cx.editor.config().default_yank_register);
        let path = helix_stdx::path::get_relative_path(&row.path);
        let path = path.to_string_lossy().to_string();
        let message = format!("Yanked path {} to register {register}", path);

        match cx.editor.registers.write(register, vec![path]) {
            Ok(()) => cx.editor.set_status(message),
            Err(err) => cx.editor.set_error(err.to_string()),
        };
    }

    fn prompt_create(&self, cx: &mut Context) {
        let root = self.root.clone();
        let cursor = selected_cursor(self.selection);
        let prefill = path_prefill(&self.selected_base_dir());
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptCreate {
                root,
                cursor,
                prefill,
            }))
        });
    }

    fn prompt_move(&self, cx: &mut Context) {
        let Some(row) = self.selected() else {
            return;
        };
        let movement = row
            .path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| Movement::BackwardChar(ext.chars().count() + 1));
        let source = row.path.clone();
        let root = self.root.clone();
        let cursor = selected_cursor(self.selection);
        let prefill = row.path.display().to_string();
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptMove {
                source,
                root,
                cursor,
                prefill,
                movement,
            }))
        });
    }

    fn prompt_delete(&self, cx: &mut Context) {
        let Some(row) = self.selected() else {
            return;
        };
        let target = row.path.clone();
        let root = self.root.clone();
        let cursor = selected_cursor(self.selection);
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptDelete {
                target,
                root,
                cursor,
            }))
        });
    }

    fn prompt_copy(&self, cx: &mut Context) {
        let Some(row) = self.selected() else {
            return;
        };
        let source = row.path.clone();
        let root = self.root.clone();
        let cursor = selected_cursor(self.selection);
        let prefill = path_prefill(&self.selected_base_dir());
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptCopy {
                source,
                root,
                cursor,
                prefill,
            }))
        });
    }

    fn close(&mut self, cx: &mut Context) -> EventResult {
        if let Some(id) = self.model_panel_id.take() {
            cx.editor.model.remove_panel(id);
        }
        EventResult::Consumed(Some(PostAction::RemoveById(ID)))
    }

    fn handle_key(&mut self, key: KeyEvent, cx: &mut Context) -> EventResult {
        match key {
            key!(Esc) | ctrl!('c') | key!('q') => return self.close(cx),
            key!(Up) | key!('k') | ctrl!('p') => self.move_selection_by(-1),
            key!(Down) | key!('j') | ctrl!('n') => self.move_selection_by(1),
            key!(PageUp) | ctrl!('u') => self.page_by(-1),
            key!(PageDown) | ctrl!('d') => self.page_by(1),
            key!(Home) => self.select_first(),
            key!(End) => self.select_last(),
            key!(Enter) | key!('l') | key!(Right) => self.open_selected(cx, Action::Replace),
            alt!(Enter) => self.open_selected(cx, Action::Replace),
            key!(' ') => self.toggle_selected_dir(cx.editor),
            key!('h') | key!(Left) => self.collapse_or_select_parent(cx.editor),
            key!(Backspace) => self.root_parent(cx.editor),
            key!('u') | key!('r') => self.refresh_current(cx.editor),
            key!('m') | alt!('m') => self.prompt_move(cx),
            key!('d') | alt!('d') => self.prompt_delete(cx),
            key!('c') | alt!('c') => self.prompt_copy(cx),
            key!('y') | alt!('y') => self.yank_selected_path(cx),
            key!('n') | alt!('n')
                if cx.editor.config().editing_engine == EditingEngineConfig::Helix =>
            {
                self.prompt_create(cx);
            }
            key!('a') | alt!('a')
                if cx.editor.config().editing_engine == EditingEngineConfig::Vim =>
            {
                self.prompt_create(cx);
            }
            _ => return EventResult::Ignored(None),
        }

        EventResult::Consumed(None)
    }

    fn handle_mouse(&mut self, event: &helix_view::input::MouseEvent) -> EventResult {
        if !matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
            return EventResult::Ignored(None);
        }
        if !self.area.contains(event.column, event.row) {
            return EventResult::Ignored(None);
        }
        let list_y = self.area.y.saturating_add(HEADER_ROWS);
        if event.row < list_y {
            return EventResult::Consumed(None);
        }
        let index = self
            .scroll
            .saturating_add(event.row.saturating_sub(list_y) as usize);
        if index < self.rows.len() {
            self.selection = index;
            self.ensure_selection_visible();
        }
        EventResult::Consumed(None)
    }

    fn row_spans(
        &self,
        row: &ExplorerRow,
        base_style: Style,
        directory_style: Style,
    ) -> Vec<Span<'static>> {
        let mut spans = Vec::with_capacity(5);
        spans.push(Span::styled("  ".repeat(row.depth), base_style));

        let disclosure = if row.is_dir {
            if row.expanded {
                "▾ "
            } else {
                "▸ "
            }
        } else {
            "  "
        };
        spans.push(Span::styled(disclosure, base_style));

        let icons = ICONS.load();
        if row.is_dir {
            if let Some(icon) = icons.kind().folder() {
                let icon_style = icon
                    .color()
                    .map(|color| base_style.patch(Style::default().fg(color)))
                    .unwrap_or(directory_style);
                spans.push(Span::styled(format!("{}  ", icon.glyph()), icon_style));
            }
        } else if let Some(icon) = icons.mime().get(Some(&row.path), None) {
            let icon_style = icon
                .color()
                .map(|color| base_style.patch(Style::default().fg(color)))
                .unwrap_or(base_style);
            spans.push(Span::styled(format!("{}  ", icon.glyph()), icon_style));
        }

        let label_style = if row.is_dir {
            directory_style
        } else {
            base_style
        };
        spans.push(Span::styled(self.label_for(row), label_style));
        if row.is_dir {
            spans.push(Span::styled("/", directory_style));
        }
        spans
    }
}

impl Focusable for FileExplorerPanel {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl Bounded for FileExplorerPanel {
    fn area(&self) -> Rect {
        self.area
    }

    fn set_area(&mut self, area: Rect) {
        self.area = area;
    }
}

impl Scrollable for FileExplorerPanel {
    fn scroll(&self) -> usize {
        self.scroll
    }

    fn scroll_to(&mut self, offset: usize) {
        self.scroll = offset.min(self.max_scroll());
    }

    fn content_height(&self) -> usize {
        self.rows.len()
    }
}

impl Component for FileExplorerPanel {
    fn sync(&mut self, editor: &mut Editor) {
        self.sync_to_model(editor);
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if !self.focused {
            return EventResult::Ignored(None);
        }

        match event {
            Event::Key(key) => self.handle_key(*key, cx),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(..) => EventResult::Consumed(None),
            Event::Paste(_) | Event::IdleTimeout | Event::FocusGained | Event::FocusLost => {
                EventResult::Ignored(None)
            }
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &RenderContext) {
        self.set_area(area);
        if area.width == 0 || area.height == 0 {
            return;
        }

        let background = cx.editor.theme.get("ui.background");
        let text_style = cx.editor.theme.get("ui.text");
        let inactive = cx.editor.theme.get("ui.text.inactive");
        let selected = cx.editor.theme.get("ui.menu.selected");
        let directory_style = cx.editor.theme.get("ui.text.directory");
        let border_style = if self.focused {
            cx.editor
                .theme
                .get("ui.window")
                .add_modifier(Modifier::BOLD)
        } else {
            inactive
        };

        surface.clear_with(area, background);
        for y in area.y..area.bottom() {
            surface.set_stringn(area.x, y, "│", 1, border_style);
        }

        let inner = area.clip_left(1);
        if inner.width == 0 {
            return;
        }

        let root_label = display_name(&self.root);
        let title = format!("Files  {root_label}");
        surface.set_stringn(inner.x, inner.y, title, inner.width as usize, text_style);

        if inner.height > 1 {
            surface.set_stringn(
                inner.x,
                inner.y + 1,
                "─".repeat(inner.width as usize),
                inner.width as usize,
                inactive,
            );
        }

        let list = inner.clip_top(HEADER_ROWS);
        if list.height == 0 {
            return;
        }

        self.ensure_selection_visible();
        if self.rows.is_empty() {
            surface.set_stringn(list.x, list.y, "No files", list.width as usize, inactive);
            return;
        }

        for (screen_row, row) in self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(list.height as usize)
        {
            let y = list.y + (screen_row - self.scroll) as u16;
            let is_selected = screen_row == self.selection;
            let row_style = if is_selected { selected } else { background };
            surface.clear_with(Rect::new(list.x, y, list.width, 1), row_style);
            let base_style = if is_selected {
                selected.patch(text_style)
            } else {
                text_style
            };
            let directory_style = if is_selected {
                selected.patch(directory_style)
            } else {
                directory_style
            };
            let spans = self.row_spans(row, base_style, directory_style);
            draw_spans(surface, list.x, y, list.width, &spans);
        }
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }

    fn layout_role(&self) -> crate::compositor::LayoutRole {
        crate::compositor::LayoutRole::Docked
    }

    fn panel_id(&self) -> Option<PanelId> {
        self.model_panel_id
    }

    component_traits!(focusable, scrollable);
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use std::{fs, sync::Arc};

    fn test_editor(width: u16, height: u16, runtime: helix_runtime::Runtime) -> Editor {
        let theme_loader = helix_view::theme::Loader::new(helix_loader::runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = helix_view::editor::Config::default();
        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = helix_view::handlers::Handlers::dummy();
        Editor::new(
            Rect::new(0, 0, width, height),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(
                config,
                |c: &helix_view::editor::Config| c,
            )),
            runtime,
            handlers,
        )
    }

    #[test]
    fn panel_builds_sorted_directory_tree() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("README.md"), "").unwrap();
        fs::write(temp.path().join("src").join("main.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let editor = test_editor(100, 30, rt.runtime());

            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();
            assert_eq!(panel.rows.len(), 2);
            assert_eq!(display_name(&panel.rows[0].path), "src");
            assert!(panel.rows[0].is_dir);
            assert_eq!(display_name(&panel.rows[1].path), "README.md");

            panel.toggle_selected_dir(&editor);
            assert!(panel
                .rows
                .iter()
                .any(|row| display_name(&row.path) == "main.rs" && row.depth == 1));
        });
    }

    #[test]
    fn panel_syncs_docked_tree_model() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("lib.rs"), "").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let mut editor = test_editor(100, 30, rt.runtime());
            let mut panel = FileExplorerPanel::new(temp.path().to_path_buf(), &editor).unwrap();

            panel.sync(&mut editor);

            let panel_id = panel.panel_id().expect("panel id");
            let entry = editor.model.panels.get(panel_id).expect("model panel");
            assert_eq!(entry.side, PanelSide::Left);
            assert!(entry.content.is::<TreePanelModel>());
        });
    }
}
