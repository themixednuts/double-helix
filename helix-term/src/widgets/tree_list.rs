//! Shared tree-list content.
//!
//! Components own tree state and compose this content widget inside `Panel`.

use std::{borrow::Cow, ops::Range};

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};
use tui::ratatui::widgets::Widget;

pub const TREE_GUIDE: &str = "│ ";
pub const TREE_MIDDLE: &str = "├╴";
pub const TREE_LAST: &str = "└╴";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TreeListStyles {
    pub background: Style,
    pub text: Style,
    pub inactive: Style,
    pub directory: Style,
    pub guide: Style,
    pub selection: Style,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeListIcon<'a> {
    pub text: Cow<'a, str>,
    pub style: Style,
}

impl<'a> TreeListIcon<'a> {
    pub fn new(text: impl Into<Cow<'a, str>>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeListStatus<'a> {
    pub text: &'a str,
    pub style: Style,
}

impl<'a> TreeListStatus<'a> {
    pub const fn new(text: &'a str, style: Style) -> Self {
        Self { text, style }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeListItem<'a> {
    pub label: &'a str,
    pub depth: usize,
    pub is_dir: bool,
    pub is_last: bool,
    pub ancestor_last: &'a [bool],
    pub icon: Option<TreeListIcon<'a>>,
    pub label_selection: Option<Range<usize>>,
    pub statuses: [Option<TreeListStatus<'a>>; 2],
    /// When `true` the row gets `styles.selection` as a background fill and a
    /// `▎` accent rail on the left edge. Used by file-explorer-style trees to
    /// show "this is the cursor row" without depending on label-character
    /// fuzzy-match highlighting.
    pub selected: bool,
    /// When `true` an extra muted dot is drawn after the label, marking
    /// "this row's file is the one currently open in the focused view".
    /// Distinct from `selected` (which is the cursor in the tree).
    pub active: bool,
}

impl<'a> TreeListItem<'a> {
    pub const fn new(label: &'a str) -> Self {
        Self {
            label,
            depth: 0,
            is_dir: false,
            is_last: true,
            ancestor_last: &[],
            icon: None,
            label_selection: None,
            statuses: [None, None],
            selected: false,
            active: false,
        }
    }

    pub const fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub const fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub const fn directory(mut self, is_dir: bool) -> Self {
        self.is_dir = is_dir;
        self
    }

    pub const fn depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    pub const fn last(mut self, is_last: bool) -> Self {
        self.is_last = is_last;
        self
    }

    pub const fn ancestors(mut self, ancestor_last: &'a [bool]) -> Self {
        self.ancestor_last = ancestor_last;
        self
    }

    pub fn icon(mut self, icon: Option<TreeListIcon<'a>>) -> Self {
        self.icon = icon;
        self
    }

    pub fn label_selection(mut self, label_selection: Option<Range<usize>>) -> Self {
        self.label_selection = label_selection;
        self
    }

    pub const fn statuses(mut self, statuses: [Option<TreeListStatus<'a>>; 2]) -> Self {
        self.statuses = statuses;
        self
    }

    fn status_width(&self) -> u16 {
        self.statuses
            .iter()
            .flatten()
            .map(|status| status_icon_width(status.text))
            .sum()
    }
}

pub fn tree_list_label_offset(ancestor_count: usize, depth: usize, icon_width: u16) -> u16 {
    let guide_count: u16 = ancestor_count.try_into().unwrap_or(u16::MAX);
    let connector_width = if depth > 0 {
        text_width(TREE_MIDDLE)
    } else {
        0
    };
    guide_count
        .saturating_mul(text_width(TREE_GUIDE))
        .saturating_add(connector_width)
        .saturating_add(icon_width)
}

pub fn tree_list(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    items: &[TreeListItem<'_>],
    styles: TreeListStyles,
    empty_message: Option<&str>,
) -> usize {
    if area.width == 0 || area.height == 0 {
        return 0;
    }

    let rat_area = tui::ratatui::to_ratatui_rect(area);
    tui::ratatui::widgets::Clear.render(rat_area, surface);
    surface.set_style(rat_area, tui::ratatui::to_ratatui_style(styles.background));

    if items.is_empty() {
        if let Some(empty_message) = empty_message {
            surface.set_stringn(
                area.x,
                area.y,
                empty_message,
                area.width as usize,
                tui::ratatui::to_ratatui_style(styles.inactive),
            );
        }
        return 0;
    }

    let mut visible_rows = 0usize;
    for (row, item) in items.iter().take(area.height as usize).enumerate() {
        visible_rows += 1;
        let y = area.y + row as u16;
        let row_area = Rect::new(area.x, y, area.width, 1);

        // No bg fill for selected rows — the cue is the accent-coloured
        // tree guide drawn inside `draw_item`, plus the terminal cursor
        // sitting on the label.

        let status_width = item.status_width();
        let content = Rect::new(
            row_area.x,
            row_area.y,
            row_area.width.saturating_sub(status_width),
            1,
        );
        draw_item(surface, content, item, styles);
        draw_statuses(surface, row_area, item);
    }
    visible_rows
}

fn draw_item(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    item: &TreeListItem<'_>,
    styles: TreeListStyles,
) {
    let mut x = area.x;
    let mut remaining = area.width;

    // The selected row's connector glyph is drawn in the selection foreground
    // colour, so the eye lands on the tree symbol (├╴ / └╴) without needing
    // a row-wide background fill. Ancestor pipes stay muted so the lineage
    // doesn't shout.
    let connector_style = if item.selected {
        let accent_fg = styles
            .selection
            .fg
            .map(|fg| Style::default().fg(fg))
            .unwrap_or(styles.guide);
        styles.guide.patch(accent_fg)
    } else {
        styles.guide
    };

    for ancestor_last in item.ancestor_last {
        let guide = if *ancestor_last { "  " } else { TREE_GUIDE };
        draw_segment(surface, &mut x, area.y, &mut remaining, guide, styles.guide);
    }

    if item.depth > 0 {
        let connector = if item.is_last { TREE_LAST } else { TREE_MIDDLE };
        draw_segment(
            surface,
            &mut x,
            area.y,
            &mut remaining,
            connector,
            connector_style,
        );
    }

    if let Some(icon) = item.icon.as_ref() {
        draw_segment(
            surface,
            &mut x,
            area.y,
            &mut remaining,
            icon.text.as_ref(),
            icon.style,
        );
        draw_segment(surface, &mut x, area.y, &mut remaining, "  ", icon.style);
    }

    let label_style = if item.is_dir {
        styles.directory
    } else {
        styles.text
    };
    // The active file (the one open in the focused view) renders bold so
    // it's distinguishable from the cursor row in the tree without taking
    // any extra column width — important on a 34-col side panel.
    let label_style = if item.active {
        label_style.add_modifier(helix_view::graphics::Modifier::BOLD)
    } else {
        label_style
    };
    draw_label(
        surface,
        &mut x,
        area.y,
        &mut remaining,
        item.label,
        label_style,
        item.label_selection.as_ref(),
        styles.selection,
    );

    if item.is_dir {
        draw_segment(
            surface,
            &mut x,
            area.y,
            &mut remaining,
            "/",
            styles.directory,
        );
    }
}

fn draw_label(
    surface: &mut crate::render::CellSurface,
    x: &mut u16,
    y: u16,
    remaining: &mut u16,
    label: &str,
    base_style: Style,
    selection: Option<&Range<usize>>,
    selection_style: Style,
) {
    let Some(selection) = selection.filter(|selection| !selection.is_empty()) else {
        draw_segment(surface, x, y, remaining, label, base_style);
        return;
    };

    let selected_style = base_style.patch(selection_style);
    let mut current = String::new();
    let mut current_style = None;
    for (char_idx, ch) in label.chars().enumerate() {
        let style = if selection.contains(&char_idx) {
            selected_style
        } else {
            base_style
        };
        if current_style != Some(style) {
            if let Some(style) = current_style {
                draw_segment(surface, x, y, remaining, &current, style);
                current.clear();
            }
            current_style = Some(style);
        }
        current.push(ch);
    }

    if let Some(style) = current_style {
        draw_segment(surface, x, y, remaining, &current, style);
    }
}

fn draw_statuses(surface: &mut crate::render::CellSurface, area: Rect, item: &TreeListItem<'_>) {
    let mut right = area.right();
    for status in item.statuses.iter().flatten() {
        draw_status_icon_right(surface, &mut right, area.y, status.text, status.style);
    }
}

fn draw_status_icon_right(
    surface: &mut crate::render::CellSurface,
    right: &mut u16,
    y: u16,
    icon: &str,
    style: Style,
) {
    let icon_width = text_width(icon).max(1);
    let x = right.saturating_sub(icon_width);
    surface.set_stringn(
        x,
        y,
        icon,
        icon_width as usize,
        tui::ratatui::to_ratatui_style(style),
    );
    *right = x.saturating_sub(1);
}

fn draw_segment(
    surface: &mut crate::render::CellSurface,
    x: &mut u16,
    y: u16,
    remaining: &mut u16,
    text: &str,
    style: Style,
) {
    if *remaining == 0 || text.is_empty() {
        return;
    }

    surface.set_stringn(
        *x,
        y,
        text,
        *remaining as usize,
        tui::ratatui::to_ratatui_style(style),
    );
    let width = text_width(text).min(*remaining);
    *x = (*x).saturating_add(width);
    *remaining = (*remaining).saturating_sub(width);
}

fn status_icon_width(icon: &str) -> u16 {
    text_width(icon).max(1).saturating_add(1)
}

fn text_width(text: &str) -> u16 {
    text.width().try_into().unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::graphics::Color;
    use tui::ratatui::{buffer::Buffer, layout::Rect as RatatuiRect};

    #[test]
    fn tree_list_renders_connectors_inside_caller_panel() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 30, 4));
        let ancestors = [false];
        let items = [
            TreeListItem::new("src").directory(true),
            TreeListItem::new("storybook.rs")
                .depth(2)
                .last(true)
                .ancestors(&ancestors),
        ];

        tree_list(
            &mut surface,
            Rect::new(0, 0, 30, 4),
            &items,
            TreeListStyles::default(),
            Some("No files"),
        );

        assert_eq!(surface[(0, 0)].symbol(), "s");
        assert_eq!(surface[(0, 1)].symbol(), "│");
        assert_eq!(surface[(2, 1)].symbol(), "└");
    }

    #[test]
    fn tree_list_highlights_only_selected_label_range() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 30, 1));
        let selection = Style::default().bg(Color::Rgb(20, 40, 80));
        let item = TreeListItem::new("alpha-beta.rs").label_selection(Some(0..5));

        tree_list(
            &mut surface,
            Rect::new(0, 0, 30, 1),
            &[item],
            TreeListStyles {
                selection,
                ..TreeListStyles::default()
            },
            None,
        );

        let selected = tui::ratatui::to_ratatui_style(selection);
        assert_eq!(surface[(0, 0)].symbol(), "a");
        let selected_bg = selected.bg.expect("selection background");
        assert_eq!(surface[(0, 0)].bg, selected_bg);
        assert_eq!(surface[(5, 0)].symbol(), "-");
        assert_ne!(surface[(5, 0)].bg, selected_bg);
    }
}
