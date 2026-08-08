use std::{path::PathBuf, sync::Arc, time::Instant};

use helix_view::{
    graphics::Rect,
    icons::{Icons, ICONS},
    theme::Style,
    traits::Bounded,
};

use crate::compositor::RenderContext;

use super::{
    selected_path_for_log, text_width, ExplorerRow, FileExplorerPanel, VcsStatus,
    FALLBACK_FILE_ICON, FALLBACK_FOLDER_ICON, FALLBACK_FOLDER_OPEN_ICON, HEADER_ROWS, SEARCH_ROWS,
};
#[derive(Clone)]
pub(super) struct ExplorerTreeItemStyles {
    pub(super) base: Style,
    pub(super) directory: Style,
    pub(super) label_selection: Option<std::ops::Range<usize>>,
    pub(super) show_icons: bool,
}

#[derive(Clone, Copy)]
pub(super) struct ExplorerStatusStyles {
    pub(super) added: Style,
    pub(super) modified: Style,
    pub(super) deleted: Style,
    pub(super) renamed: Style,
    pub(super) conflict: Style,
    pub(super) diagnostic_hint: Style,
    pub(super) diagnostic_info: Style,
    pub(super) diagnostic_warning: Style,
    pub(super) diagnostic_error: Style,
}

#[derive(Clone)]
struct RenderLabelEdit {
    row_index: usize,
    buffer: String,
}

pub(super) struct ExplorerRenderSnapshot {
    rows: Arc<[ExplorerRow]>,
    all_rows_len: usize,
    search_query: String,
    search_active: bool,
    search_pending: bool,
    tree_pending: bool,
    search_generation: u64,
    selection: usize,
    /// Absolute row indices covered by the `x` range (just the cursor row
    /// when no range is active). Every row in it renders selected.
    row_selection: std::ops::Range<usize>,
    scroll: usize,
    scroll_x: u16,
    focused: bool,
    show_icons: bool,
    /// Uniform icon-column width resolved by the panel for this frame.
    icon_column: u16,
    label_selection: helix_view::modal_text::ModalTextSelection,
    label_edit: Option<RenderLabelEdit>,
    jump_session: Option<helix_view::jump_labels::JumpSession>,
    active_path: Option<PathBuf>,
    styles: crate::ui::design::FileExplorerStyles,
    scrollbar_style: Style,
    jump_label_style: Style,
}

fn row_icon<'a>(
    row: &'a ExplorerRow,
    styles: ExplorerTreeItemStyles,
    icons: &'a Icons,
) -> Option<crate::widgets::TreeListIcon<'a>> {
    if !styles.show_icons {
        return None;
    }

    if row.is_dir {
        let kind_icon = if row.expanded {
            icons.kind().folder_open()
        } else {
            icons.kind().folder()
        };
        if let Some(icon) = kind_icon {
            let icon_style = icon
                .color()
                .map(|color| styles.base.patch(Style::default().fg(color)))
                .unwrap_or(styles.directory);
            return Some(crate::widgets::TreeListIcon::new(
                icon.glyph().to_string(),
                icon_style,
            ));
        }

        let mime_icon = if row.expanded {
            icons.mime().directory_open()
        } else {
            icons.mime().directory()
        };
        if let Some(icon) = mime_icon {
            return Some(crate::widgets::TreeListIcon::new(icon, styles.directory));
        }

        let fallback = if row.expanded {
            FALLBACK_FOLDER_OPEN_ICON
        } else {
            FALLBACK_FOLDER_ICON
        };
        return Some(crate::widgets::TreeListIcon::new(
            fallback,
            styles.directory,
        ));
    }

    let icon_path = row.path.icon_path().into_owned();
    if let Some(icon) = icons
        .mime()
        .get(Some(&icon_path), None)
        .or_else(|| icons.mime().get_or_default(Some(&icon_path), None))
    {
        let icon_style = icon
            .color()
            .map(|color| styles.base.patch(Style::default().fg(color)))
            .unwrap_or(styles.base);
        Some(crate::widgets::TreeListIcon::new(
            icon.glyph().to_owned(),
            icon_style,
        ))
    } else {
        Some(crate::widgets::TreeListIcon::new(
            FALLBACK_FILE_ICON,
            styles.base,
        ))
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "tree rows need separate rendering state for selection, filtering, and VCS styling"
)]
fn tree_item<'a>(
    row: &'a ExplorerRow,
    label: &'a str,
    selected: bool,
    ranged: bool,
    active: bool,
    styles: ExplorerTreeItemStyles,
    icons: &'a Icons,
    status_styles: ExplorerStatusStyles,
) -> crate::widgets::TreeListItem<'a> {
    let icon = row_icon(row, styles.clone(), icons);
    let label_selection = selected.then_some(()).and(styles.label_selection);
    let statuses = [
        row.vcs_status.map(|status| {
            crate::widgets::TreeListStatus::new(
                status.icon(icons),
                status.style(
                    status_styles.added,
                    status_styles.modified,
                    status_styles.deleted,
                    status_styles.renamed,
                    status_styles.conflict,
                ),
            )
        }),
        row.diagnostic_status.map(|diagnostic| {
            crate::widgets::TreeListStatus::new(
                diagnostic.icon(icons),
                diagnostic.style(
                    status_styles.diagnostic_hint,
                    status_styles.diagnostic_info,
                    status_styles.diagnostic_warning,
                    status_styles.diagnostic_error,
                ),
            )
        }),
    ];

    crate::widgets::TreeListItem::new(label)
        .directory(row.is_dir)
        .depth(row.depth)
        .last(row.is_last)
        .ancestors(row.ancestor_last.as_slice())
        .icon(icon)
        .label_selection(label_selection)
        .statuses(statuses)
        .selected(selected)
        .ranged(ranged)
        .active(active)
}

impl FileExplorerPanel {
    #[allow(
        clippy::too_many_arguments,
        reason = "tree rows need separate rendering state for selection, filtering, and VCS styling"
    )]
    #[cfg(test)]
    pub(super) fn tree_item<'a>(
        &'a self,
        row: &'a ExplorerRow,
        row_index: usize,
        selected: bool,
        ranged: bool,
        active: bool,
        styles: ExplorerTreeItemStyles,
        icons: &'a Icons,
        status_styles: ExplorerStatusStyles,
    ) -> crate::widgets::TreeListItem<'a> {
        let label = self.display_label_for(row, row_index);
        tree_item(
            row,
            label,
            selected,
            ranged,
            active,
            styles,
            icons,
            status_styles,
        )
    }

    pub(super) fn global_statusline(
        &self,
        cx: &RenderContext,
    ) -> crate::ui::statusline::PanelStatusline {
        use helix_core::diagnostic::Severity;

        let icons = ICONS.load();
        let mut modified_count = 0;
        let mut added_count = 0;
        let mut error_count = 0;
        let mut warning_count = 0;
        for row in self.rows.iter() {
            match row.vcs_status {
                Some(VcsStatus::Added) => added_count += 1,
                Some(VcsStatus::Modified | VcsStatus::Renamed) => modified_count += 1,
                _ => {}
            }
            match row.diagnostic_status.map(|status| status.severity) {
                Some(Severity::Error) => error_count += 1,
                Some(Severity::Warning) => warning_count += 1,
                _ => {}
            }
        }

        let mut right = Vec::new();
        let theme = cx.theme();
        if added_count > 0 {
            right.push(crate::ui::statusline::StatuslineText {
                text: format!(" {} {} ", icons.vcs().added(), added_count),
                style: theme.get("diff.plus"),
            });
        }
        if modified_count > 0 {
            right.push(crate::ui::statusline::StatuslineText {
                text: format!(" {} {} ", icons.vcs().modified(), modified_count),
                style: theme.get("diff.delta"),
            });
        }
        if warning_count > 0 {
            right.push(crate::ui::statusline::StatuslineText {
                text: format!(" {} {} ", icons.diagnostic().warning(), warning_count),
                style: theme.get("warning"),
            });
        }
        if error_count > 0 {
            right.push(crate::ui::statusline::StatuslineText {
                text: format!(" {} {} ", icons.diagnostic().error(), error_count),
                style: theme.get("error"),
            });
        }
        let current = if self.rows.is_empty() {
            0
        } else {
            self.selection + 1
        };
        right.push(crate::ui::statusline::StatuslineText {
            text: format!(" {current} · {} ", self.rows.len()),
            style: theme
                .try_get("ui.text.inactive")
                .or_else(|| theme.try_get("comment"))
                .unwrap_or_else(|| theme.get("ui.statusline")),
        });

        crate::ui::statusline::PanelStatusline {
            state: crate::ui::statusline::PanelStatuslineState::Mode(self.input.mode),
            identity: format!(
                "Files - {}",
                selected_path_for_log(&self.rows, self.selection)
            ),
            right,
        }
    }

    pub(super) fn render_snapshot(
        &mut self,
        area: Rect,
        cx: &RenderContext,
    ) -> ExplorerRenderSnapshot {
        self.set_area(area);
        self.clamp_viewport();

        // Mirror the list geometry used by render_surface so horizontal
        // scroll clamps against the real content viewport: indent column,
        // optional vertical scrollbar.
        let list_height = area.height.saturating_sub(HEADER_ROWS + SEARCH_ROWS);
        let list_width = area.width.saturating_sub(2).saturating_sub(1);
        let has_scrollbar = self.rows.len() > list_height as usize;
        let content_width = if has_scrollbar {
            list_width.saturating_sub(1)
        } else {
            list_width
        };
        self.sync_horizontal_viewport(content_width);

        let theme = cx.theme();
        let styles = crate::ui::design::FileExplorerStyles::from_theme(theme, self.focused);
        let jump_label_style = theme.try_get("ui.virtual.jump-label").unwrap_or_else(|| {
            theme
                .get("ui.text")
                .add_modifier(helix_view::graphics::Modifier::BOLD)
        });
        let active_path = cx
            .view(cx.focused_view_id())
            .and_then(|view| cx.document(view.doc))
            .and_then(|doc| doc.path().cloned());

        ExplorerRenderSnapshot {
            rows: Arc::clone(&self.rows),
            all_rows_len: self.all_rows.len(),
            search_query: self.search_query.clone(),
            search_active: self.search_active,
            search_pending: self.search_pending,
            tree_pending: self.tree_pending,
            search_generation: self.search_generation,
            selection: self.selection,
            row_selection: self.selected_row_range(),
            scroll: self.scroll,
            scroll_x: self.scroll_x,
            focused: self.focused,
            show_icons: self.config.icons,
            icon_column: self.icon_column,
            label_selection: self.label_selection,
            label_edit: self.label_edit.as_ref().map(|edit| RenderLabelEdit {
                row_index: edit.row_index,
                buffer: edit.buffer.clone(),
            }),
            jump_session: self.jump_session.clone(),
            active_path,
            styles,
            scrollbar_style: theme.get("ui.menu.scroll"),
            jump_label_style,
        }
    }
}

impl ExplorerRenderSnapshot {
    fn display_label_for<'a>(&'a self, row: &'a ExplorerRow, row_index: usize) -> &'a str {
        if let Some(edit) = &self.label_edit {
            if edit.row_index == row_index {
                return edit.buffer.as_str();
            }
        }
        row.label.as_str()
    }

    fn search_count_text(&self) -> Option<String> {
        if self.search_query.is_empty() {
            return self.tree_pending.then(|| "loading".to_owned());
        }
        if self.search_pending {
            Some("searching".to_owned())
        } else {
            Some(format!("{} results", self.rows.len()))
        }
    }

    fn row_label_offset(&self, row: &ExplorerRow) -> u16 {
        crate::widgets::tree_list_label_offset(row.ancestor_last.len(), row.depth, self.icon_column)
    }

    pub(super) fn render_surface(
        &self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        cancellation: &crate::render::RenderCancellation,
    ) {
        let render_start = Instant::now();
        if area.width == 0 || area.height == 0 {
            log::trace!(
                "[file_explorer] render skipped=empty_area area={}x{}+{},{} elapsed_us={}",
                area.width,
                area.height,
                area.x,
                area.y,
                render_start.elapsed().as_micros()
            );
            return;
        }

        let styles = self.styles;

        let inner = crate::widgets::Panel::edge(
            crate::widgets::PanelStyle::new(styles.background, styles.border, styles.header),
            crate::widgets::PanelEdge::Right,
        )
        .render(surface, area);
        if inner.width == 0 {
            log::trace!(
                "[file_explorer] render skipped=empty_inner area={}x{}+{},{} elapsed_us={}",
                area.width,
                area.height,
                area.x,
                area.y,
                render_start.elapsed().as_micros()
            );
            return;
        }

        crate::widgets::header(surface, inner, " FILES", styles.header);
        let search_area = Rect::new(
            inner.x,
            inner.y.saturating_add(HEADER_ROWS),
            inner.width,
            SEARCH_ROWS,
        );
        self.render_search_bar(surface, search_area, styles);

        let list = inner.clip_top(HEADER_ROWS + SEARCH_ROWS).clip_left(1);
        if list.height == 0 {
            log::trace!(
                "[file_explorer] render skipped=empty_list area={}x{}+{},{} inner={}x{} elapsed_us={}",
                area.width,
                area.height,
                area.x,
                area.y,
                inner.width,
                inner.height,
                render_start.elapsed().as_micros()
            );
            return;
        }

        if cancellation.is_cancelled() {
            return;
        }
        let has_scrollbar = self.rows.len() > list.height as usize;
        let tree_area = if has_scrollbar {
            list.clip_right(1)
        } else {
            list
        };
        let icons = ICONS.load();
        let item_styles = ExplorerTreeItemStyles {
            base: styles.text,
            directory: styles.directory,
            label_selection: None,
            show_icons: self.show_icons,
        };
        let status_styles = ExplorerStatusStyles {
            added: styles.status.added,
            modified: styles.status.modified,
            deleted: styles.status.deleted,
            renamed: styles.status.renamed,
            conflict: styles.status.conflict,
            diagnostic_hint: styles.status.diagnostic_hint,
            diagnostic_info: styles.status.diagnostic_info,
            diagnostic_warning: styles.status.diagnostic_warning,
            diagnostic_error: styles.status.diagnostic_error,
        };
        // Resolve the path of the document currently open in the focused view
        // so we can mark its tree row as "active" — a quiet cue that's
        // distinct from the cursor row in the tree. Guarded against the
        // no-focused-view case (e.g. early-startup, headless tests).
        let active_path = self.active_path.as_ref();
        let visible_items = self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(tree_area.height as usize)
            .map(|(screen_row, row)| {
                let mut styles = item_styles.clone();
                // When inline-editing this row, the cursor + selection
                // operate on the edit buffer rather than the stored label.
                let label_source = self.display_label_for(row, screen_row);
                styles.label_selection = (screen_row == self.selection)
                    .then(|| self.label_selection.span(label_source))
                    .flatten();
                let is_active = !row.is_dir && active_path.is_some_and(|path| path == &row.path);
                // A one-row "range" is just the cursor, which keeps its
                // existing look; the fill only appears once `x` has actually
                // extended the selection.
                let ranged =
                    self.row_selection.len() > 1 && self.row_selection.contains(&screen_row);
                tree_item(
                    row,
                    label_source,
                    screen_row == self.selection,
                    ranged,
                    is_active,
                    styles,
                    &icons,
                    status_styles,
                )
            })
            .collect::<Vec<_>>();
        let visible_rows = crate::widgets::tree_list_scrolled(
            surface,
            tree_area,
            &visible_items,
            crate::widgets::TreeListStyles {
                background: styles.background,
                text: styles.text,
                inactive: styles.inactive,
                directory: styles.directory,
                guide: styles.guide,
                selection: styles.selection,
            },
            Some("No files"),
            self.scroll_x,
            self.icon_column,
        );
        drop(visible_items);

        if has_scrollbar {
            let scrollbar_area = Rect::new(list.right().saturating_sub(1), list.y, 1, list.height);
            crate::widgets::Scrollbar::new(self.rows.len(), self.scroll, tree_area.height as usize)
                .thumb_style(self.scrollbar_style)
                .track(" ", styles.background)
                .render(scrollbar_area, surface);
        }

        // After the tree paint, if a jump-label session is active,
        // overlay each visible row's first 1–2 cells (at the row
        // label's offset, past the indent guides + icon) with the
        // session's label. Same visual model as the editor's `gw`
        // virtual `Overlay` text — the first label char replaces the
        // first label character of the row, the second replaces the
        // second.
        if let Some(session) = self.jump_session.as_ref() {
            self.render_jump_labels(surface, tree_area, session);
        }

        log::trace!(
            "[file_explorer] render_snapshot rows={} all_rows={} visible_rows={} area={}x{}+{},{} list={}x{} scroll={} selection={} selected={} focused={} query={:?} search_active={} search_pending={} search_generation={} elapsed_us={}",
            self.rows.len(),
            self.all_rows_len,
            visible_rows,
            area.width,
            area.height,
            area.x,
            area.y,
            list.width,
            list.height,
            self.scroll,
            self.selection,
            selected_path_for_log(&self.rows, self.selection),
            self.focused,
            self.search_query,
            self.search_active,
            self.search_pending,
            self.search_generation,
            render_start.elapsed().as_micros()
        );
    }

    fn render_search_bar(
        &self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        styles: crate::ui::design::FileExplorerStyles,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        surface.set_style(
            tui::ratatui::to_ratatui_rect(area),
            tui::ratatui::to_ratatui_style(styles.background),
        );

        let marker_style = if self.search_active {
            styles.header
        } else {
            styles.inactive
        };
        if area.width > 1 {
            surface.set_stringn(
                area.x.saturating_add(1),
                area.y,
                "/",
                1,
                tui::ratatui::to_ratatui_style(marker_style),
            );
        }

        let mut input_area = Rect::new(
            area.x.saturating_add(3),
            area.y,
            area.width.saturating_sub(4),
            1,
        );

        let count = self.search_count_text();
        if let Some(count) = count.as_ref() {
            let width = text_width(count);
            if input_area.width > width.saturating_add(1) {
                surface.set_stringn(
                    area.right().saturating_sub(width).saturating_sub(1),
                    area.y,
                    count,
                    width as usize,
                    tui::ratatui::to_ratatui_style(styles.inactive),
                );
                input_area = input_area.clip_right(width.saturating_add(2));
            }
        }

        if self.search_query.is_empty() && !self.search_active {
            surface.set_stringn(
                input_area.x,
                input_area.y,
                "search files",
                input_area.width as usize,
                tui::ratatui::to_ratatui_style(styles.inactive),
            );
            return;
        }

        crate::widgets::text_input(
            surface,
            input_area,
            &self.search_query,
            self.search_query.len(),
            styles.text,
            styles.selection,
        );
    }

    /// Overlay the active jump session's labels on each visible row.
    ///
    /// Each visible row gets a two-character label drawn over the
    /// first 1–2 cells of its **label** (after the indent guides and
    /// icon — i.e., starting at `list.x + row_label_offset(row)`),
    /// matching the editor's `goto_word` overlay model. The label
    /// style uses the theme's `ui.virtual.jump-label` if defined,
    /// otherwise falls back to `ui.text` with bold so labels stand
    /// out without depending on theme work.
    fn render_jump_labels(
        &self,
        surface: &mut crate::render::CellSurface,
        list: Rect,
        session: &helix_view::jump_labels::JumpSession,
    ) {
        let style = tui::ratatui::to_ratatui_style(self.jump_label_style);

        let visible_count = (self.rows.len().saturating_sub(self.scroll)).min(list.height as usize);
        for screen_row in 0..visible_count {
            let target_id = screen_row as u32;
            let Some(label) = session.label_at(target_id) else {
                break;
            };
            let absolute_row = self.scroll.saturating_add(screen_row);
            let Some(row) = self.rows.get(absolute_row) else {
                break;
            };
            let label_offset = self.row_label_offset(row);
            if label_offset < self.scroll_x {
                continue;
            }
            let label_x = list
                .x
                .saturating_add(label_offset.saturating_sub(self.scroll_x));
            let label_y = list.y.saturating_add(screen_row as u16);
            // Defensive: if the label offset already runs past the
            // visible width, there's no room to draw — skip rather
            // than truncate to a half-label.
            if label_x.saturating_add(2) > list.x.saturating_add(list.width) {
                continue;
            }
            if let Some(cell) = surface.cell_mut((label_x, label_y)) {
                cell.set_char(label.first).set_style(style);
            }
            if let Some(cell) = surface.cell_mut((label_x.saturating_add(1), label_y)) {
                cell.set_char(label.second).set_style(style);
            }
        }
    }
}
