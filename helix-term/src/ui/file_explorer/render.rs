use std::time::Instant;

use helix_view::{
    graphics::Rect,
    icons::{Icons, ICONS},
    theme::Style,
    traits::Bounded,
};

use crate::compositor::RenderContext;

use super::{
    selected_path_for_log, ExplorerRow, FileExplorerPanel, VcsStatus, FALLBACK_FILE_ICON,
    FALLBACK_FOLDER_ICON, FALLBACK_FOLDER_OPEN_ICON, FOOTER_ROWS, HEADER_ROWS,
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

impl FileExplorerPanel {
    fn row_icon<'a>(
        &self,
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

        if let Some(icon) = icons
            .mime()
            .get(Some(&row.path), None)
            .or_else(|| icons.mime().get_or_default(Some(&row.path), None))
        {
            let icon_style = icon
                .color()
                .map(|color| styles.base.patch(Style::default().fg(color)))
                .unwrap_or(styles.base);
            Some(crate::widgets::TreeListIcon::new(icon.glyph(), icon_style))
        } else {
            Some(crate::widgets::TreeListIcon::new(
                FALLBACK_FILE_ICON,
                styles.base,
            ))
        }
    }

    pub(super) fn tree_item<'a>(
        &'a self,
        row: &'a ExplorerRow,
        row_index: usize,
        selected: bool,
        active: bool,
        styles: ExplorerTreeItemStyles,
        icons: &'a Icons,
        status_styles: ExplorerStatusStyles,
    ) -> crate::widgets::TreeListItem<'a> {
        let icon = self.row_icon(row, styles.clone(), icons);
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

        // The row shows the live edit buffer when this is the row being
        // inline-edited, otherwise the stored file label. The buffer lives
        // on `self`, so the returned TreeListItem borrows from `&'a self`.
        let label = self.display_label_for(row, row_index);

        crate::widgets::TreeListItem::new(label)
            .directory(row.is_dir)
            .depth(row.depth)
            .last(row.is_last)
            .ancestors(row.ancestor_last.as_slice())
            .icon(icon)
            .label_selection(label_selection)
            .statuses(statuses)
            .selected(selected)
            .active(active)
    }
}

impl FileExplorerPanel {
    pub(super) fn render_surface(
        &mut self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        cx: &RenderContext,
    ) {
        let render_start = Instant::now();
        self.set_area(area);
        if area.width == 0 || area.height == 0 {
            log::info!(
                "[file_explorer] render skipped=empty_area area={}x{}+{},{} elapsed_us={}",
                area.width,
                area.height,
                area.x,
                area.y,
                render_start.elapsed().as_micros()
            );
            return;
        }

        let theme = cx.theme();
        let config = cx.config();
        let styles = crate::ui::design::FileExplorerStyles::from_theme(theme, self.focused);

        let inner = crate::widgets::Panel::edge(
            crate::widgets::PanelStyle::new(styles.background, styles.border, styles.header),
            crate::widgets::PanelEdge::Right,
        )
        .render(surface, area);
        if inner.width == 0 {
            log::info!(
                "[file_explorer] render skipped=empty_inner area={}x{}+{},{} elapsed_us={}",
                area.width,
                area.height,
                area.x,
                area.y,
                render_start.elapsed().as_micros()
            );
            return;
        }

        let current = if self.rows.is_empty() {
            0
        } else {
            self.selection + 1
        };
        // Header is now just the section label — counts moved to the
        // statusline below so the top reads as a clean orientation cue.
        crate::widgets::header(surface, inner, " FILES", styles.header);

        let list = inner
            .clip_top(HEADER_ROWS)
            .clip_bottom(FOOTER_ROWS)
            .clip_left(1);
        if list.height == 0 {
            log::info!(
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

        self.ensure_selection_visible();
        let icons = ICONS.load();
        let item_styles = ExplorerTreeItemStyles {
            base: styles.text,
            directory: styles.directory,
            label_selection: None,
            show_icons: config.file_explorer.icons,
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
        let active_path = cx
            .view(cx.focused_view_id())
            .and_then(|view| cx.document(view.doc))
            .and_then(|doc| doc.path().cloned());
        let visible_items = self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(list.height as usize)
            .map(|(screen_row, row)| {
                let mut styles = item_styles.clone();
                // When inline-editing this row, the cursor + selection
                // operate on the edit buffer rather than the stored label.
                let label_source = self.display_label_for(row, screen_row);
                styles.label_selection = (screen_row == self.selection)
                    .then(|| self.label_selection.span(label_source))
                    .flatten();
                let is_active =
                    !row.is_dir && active_path.as_ref().is_some_and(|path| path == &row.path);
                self.tree_item(
                    row,
                    screen_row,
                    screen_row == self.selection,
                    is_active,
                    styles,
                    &icons,
                    status_styles,
                )
            })
            .collect::<Vec<_>>();
        let visible_rows = crate::widgets::tree_list(
            surface,
            list,
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
        );
        drop(visible_items);

        // After the tree paint, if a jump-label session is active,
        // overlay each visible row's first 1–2 cells (at the row
        // label's offset, past the indent guides + icon) with the
        // session's label. Same visual model as the editor's `gw`
        // virtual `Overlay` text — the first label char replaces the
        // first label character of the row, the second replaces the
        // second.
        if let Some(session) = self.jump_session.as_ref() {
            self.render_jump_labels(surface, list, session, theme, &config);
        }

        // Two-row footer anchored at the bottom of the panel: statusline
        // strip + error/info line. Mirrors the editor view's chrome so the
        // panel's bottom edge aligns instead of running past it.
        let footer_area = Rect::new(
            inner.x,
            inner.bottom().saturating_sub(FOOTER_ROWS),
            inner.width,
            FOOTER_ROWS,
        );
        self.render_footer(surface, footer_area, theme, cx, current, self.rows.len());

        log::info!(
            "[file_explorer] render rows={} visible_rows={} area={}x{}+{},{} list={}x{} scroll={} selection={} selected={} focused={} preview={:?} focused_view={:?} focused_doc={:?} documents={} component_documents={} elapsed_us={}",
            self.rows.len(),
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
            self.preview,
            cx.focused_view_id(),
            cx.focused_document_id(),
            cx.document_count(),
            cx.component_document_count(),
            render_start.elapsed().as_micros()
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
        theme: &helix_view::Theme,
        config: &helix_view::editor::Config,
    ) {
        use helix_view::graphics::Modifier;

        // Theme keys: prefer the explicit jump-label slot the editor
        // uses, fall back to a sensible default so the feature works
        // out of the box on themes that haven't styled it yet.
        let label_style = theme
            .try_get("ui.virtual.jump-label")
            .unwrap_or_else(|| theme.get("ui.text").add_modifier(Modifier::BOLD));
        let style = tui::ratatui::to_ratatui_style(label_style);

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
            let label_offset = self.row_label_offset(row, config.file_explorer.icons);
            let label_x = list.x.saturating_add(label_offset);
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

    /// Single-row statusline strip for the file explorer panel.
    ///
    /// Layout: ` MODE ` chip (left, only when focused) · diagnostic + vcs
    /// summary chips (centre, only non-zero ones) · `cur · total` counts
    /// (right). Transient error / info messages don't live here — the
    /// editor's bottom row owns that channel globally.
    fn render_footer(
        &self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        theme: &helix_view::Theme,
        cx: &RenderContext,
        current: usize,
        total: usize,
    ) {
        // `cx` is read for `config().statusline.mode` so the mode
        // chip honors the user's `[editor.statusline.mode]` labels.
        if area.width == 0 || area.height == 0 {
            return;
        }

        let base_style = if self.focused {
            theme.get("ui.statusline")
        } else {
            theme.get("ui.statusline.inactive")
        };
        surface.set_style(
            tui::ratatui::to_ratatui_rect(area),
            tui::ratatui::to_ratatui_style(base_style),
        );

        let muted_style = theme
            .try_get("ui.text.inactive")
            .or_else(|| theme.try_get("comment"))
            .unwrap_or(base_style);

        // --- Left: mode chip when focused -----------------------------------
        // Uses the shared `statusline_mode` helpers so the file
        // explorer's chip respects `[editor.statusline.mode]` config
        // (same labels the editor's statusline shows) and resolves
        // theme scopes the same way every other surface does.
        let mut left_cursor = area.x;
        if self.focused {
            use helix_view::statusline_mode::{mode_style, padded_mode_name};
            let label = padded_mode_name(self.input.mode, &cx.config().statusline.mode);
            let mode_chip_style = mode_style(self.input.mode, theme, base_style);
            let chip = crate::widgets::Chip::new(label.as_str(), base_style.patch(mode_chip_style));
            left_cursor = crate::widgets::chip_strip_left(
                surface,
                left_cursor,
                area.right(),
                area.y,
                std::slice::from_ref(&chip),
            );
        }

        // --- Right: ` current · total ` counts -------------------------------
        // Right-anchored via the shared chip_strip helper — the only
        // chip on the right cluster. The returned anchor becomes the
        // budget cap for the middle chips below.
        let count_label = format!(" {current} · {total} ");
        let right_chips = [crate::widgets::Chip::new(
            &count_label,
            base_style.patch(muted_style),
        )];
        let right_anchor = crate::widgets::chip_strip_right(
            surface,
            left_cursor,
            area.right(),
            area.y,
            &right_chips,
        );

        // --- Centre: summary chips for vcs + diagnostics ---------------------
        // Only render non-zero totals. Each chip is ` <glyph> <count> `; we
        // paint them muted by default and tint with their semantic colour so
        // the dots read at a glance. Drawn via the same shared helper that
        // future panel footers will use.
        let icons = ICONS.load();
        let modified_count = self
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row.vcs_status,
                    Some(VcsStatus::Modified) | Some(VcsStatus::Renamed)
                )
            })
            .count();
        let added_count = self
            .rows
            .iter()
            .filter(|row| matches!(row.vcs_status, Some(VcsStatus::Added)))
            .count();
        use helix_core::diagnostic::Severity;
        let error_count = self
            .rows
            .iter()
            .filter(|row| {
                row.diagnostic_status
                    .is_some_and(|s| s.severity == Severity::Error)
            })
            .count();
        let warning_count = self
            .rows
            .iter()
            .filter(|row| {
                row.diagnostic_status
                    .is_some_and(|s| s.severity == Severity::Warning)
            })
            .count();
        // Build chip labels into owned Strings first so we can pass
        // borrowed `&str` views to the strip helper. (Chip<'a> borrows
        // its label.)
        let mut chip_labels: Vec<(String, helix_view::theme::Style)> = Vec::new();
        if added_count > 0 {
            chip_labels.push((
                format!(" {} {} ", icons.vcs().added(), added_count),
                base_style.patch(theme.get("diff.plus")),
            ));
        }
        if modified_count > 0 {
            chip_labels.push((
                format!(" {} {} ", icons.vcs().modified(), modified_count),
                base_style.patch(theme.get("diff.delta")),
            ));
        }
        if warning_count > 0 {
            chip_labels.push((
                format!(" {} {} ", icons.diagnostic().warning(), warning_count),
                base_style.patch(theme.get("warning")),
            ));
        }
        if error_count > 0 {
            chip_labels.push((
                format!(" {} {} ", icons.diagnostic().error(), error_count),
                base_style.patch(theme.get("error")),
            ));
        }
        let chips: Vec<crate::widgets::Chip<'_>> = chip_labels
            .iter()
            .map(|(label, style)| crate::widgets::Chip::new(label.as_str(), *style))
            .collect();
        crate::widgets::chip_strip_left(surface, left_cursor, right_anchor, area.y, &chips);
    }
}
