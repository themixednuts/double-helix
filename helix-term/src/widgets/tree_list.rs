//! Shared tree-list content.
//!
//! Components own tree state and compose this content widget inside `Panel`.

use std::{borrow::Cow, ops::Range};

use helix_core::unicode::width::{UnicodeWidthChar, UnicodeWidthStr};
use helix_view::graphics::{Rect, Style};
use tui::ratatui::widgets::Widget;

/// Blank columns between the icon glyph and the label.
pub const TREE_ICON_GAP: u16 = 2;

/// Width of the icon column: the widest glyph in the set plus [`TREE_ICON_GAP`],
/// or zero when the tree draws no icons.
///
/// The column is uniform across rows on purpose. Sizing it per row would make a
/// label's x-offset depend on which glyph that row happens to show, so a
/// terminal that draws one glyph wider than `unicode-width` reports would shove
/// that row's label right — and an expanded directory (open-folder glyph) could
/// end up further right than its own children (closed-folder glyph). With a
/// fixed column an over-wide glyph bleeds into the gap instead, and the
/// two-column parent/child step holds by construction.
pub fn tree_list_icon_column(widest_glyph: u16) -> u16 {
    if widest_glyph == 0 {
        0
    } else {
        widest_glyph.saturating_add(TREE_ICON_GAP)
    }
}

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
    /// The cursor row. Draws the tree connector in the selection *foreground*
    /// colour. Note this is a no-op cue on its own for the common theme where
    /// `ui.selection` sets only a background — the cursor row is really
    /// identified by the terminal cursor sitting on its label.
    pub selected: bool,
    /// Part of a multi-row range. Fills the row with `styles.selection`, which
    /// is the cue that actually shows up under a background-only selection
    /// theme (see `selected`). Rows off the cursor have no terminal cursor to
    /// identify them, so this fill is the only thing that marks them.
    pub ranged: bool,
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
            ranged: false,
            active: false,
        }
    }

    pub const fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub const fn ranged(mut self, ranged: bool) -> Self {
        self.ranged = ranged;
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
            .map(|status| tree_list_status_icon_width(status.text))
            .sum()
    }
}

/// Column the label starts at. `icon_column` is the uniform width from
/// [`tree_list_icon_column`] — never one row's measured glyph width.
pub fn tree_list_label_offset(ancestor_count: usize, depth: usize, icon_column: u16) -> u16 {
    let guide_count: u16 = ancestor_count.try_into().unwrap_or(u16::MAX);
    let connector_width = if depth > 0 {
        text_width(TREE_MIDDLE)
    } else {
        0
    };
    guide_count
        .saturating_mul(text_width(TREE_GUIDE))
        .saturating_add(connector_width)
        .saturating_add(icon_column)
}

pub fn tree_list_item_content_width(item: &TreeListItem<'_>, icon_column: u16) -> u16 {
    let label_end = tree_list_label_offset(item.ancestor_last.len(), item.depth, icon_column)
        .saturating_add(text_width(item.label));
    if item.is_dir {
        label_end.saturating_add(1)
    } else {
        label_end
    }
}

pub fn tree_list(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    items: &[TreeListItem<'_>],
    styles: TreeListStyles,
    empty_message: Option<&str>,
) -> usize {
    // Callers without a tree-wide icon set get the column sized from the rows
    // they passed, which is uniform across this paint.
    let icon_column = tree_list_icon_column(widest_icon_glyph(items));
    tree_list_scrolled(surface, area, items, styles, empty_message, 0, icon_column)
}

/// Widest icon glyph among `items`, or zero when none of them draw an icon.
pub fn widest_icon_glyph(items: &[TreeListItem<'_>]) -> u16 {
    items
        .iter()
        .filter_map(|item| item.icon.as_ref())
        .map(|icon| text_width(icon.text.as_ref()))
        .max()
        .unwrap_or(0)
}

pub fn tree_list_scrolled(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    items: &[TreeListItem<'_>],
    styles: TreeListStyles,
    empty_message: Option<&str>,
    scroll_x: u16,
    icon_column: u16,
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

        // The cursor row gets no fill — its cue is the accent connector in
        // `draw_item` plus the terminal cursor on its label. Ranged rows have
        // neither, so they get the selection fill; glyphs drawn afterwards
        // only set the fields their own style specifies, so the fill shows
        // through behind the text.
        if item.ranged {
            surface.set_style(
                tui::ratatui::to_ratatui_rect(row_area),
                tui::ratatui::to_ratatui_style(styles.selection),
            );
        }

        let status_width = item.status_width();
        let content = Rect::new(
            row_area.x,
            row_area.y,
            row_area.width.saturating_sub(status_width),
            1,
        );
        draw_item(surface, content, item, styles, scroll_x, icon_column);
        draw_statuses(surface, row_area, item);
    }
    visible_rows
}

fn draw_item(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    item: &TreeListItem<'_>,
    styles: TreeListStyles,
    scroll_x: u16,
    icon_column: u16,
) {
    let mut content_x = 0u16;

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
        draw_segment_scrolled(surface, area, &mut content_x, guide, styles.guide, scroll_x);
    }

    if item.depth > 0 {
        let connector = if item.is_last { TREE_LAST } else { TREE_MIDDLE };
        draw_segment_scrolled(
            surface,
            area,
            &mut content_x,
            connector,
            connector_style,
            scroll_x,
        );
    }

    // The icon column is always `icon_column` wide, whatever this row's glyph
    // measures — a narrower glyph is padded out, so every label at this depth
    // starts at the same column. Rows with no icon still consume the column.
    if icon_column > 0 {
        let glyph = item.icon.as_ref().map_or(0, |icon| {
            let style = icon.style;
            let glyph = text_width(icon.text.as_ref());
            draw_segment_scrolled(
                surface,
                area,
                &mut content_x,
                icon.text.as_ref(),
                style,
                scroll_x,
            );
            glyph
        });
        let style = item.icon.as_ref().map_or(styles.text, |icon| icon.style);
        let padding = usize::from(icon_column.saturating_sub(glyph));
        draw_segment_scrolled(
            surface,
            area,
            &mut content_x,
            &" ".repeat(padding),
            style,
            scroll_x,
        );
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
    draw_label_scrolled(
        surface,
        area,
        &mut content_x,
        item.label,
        label_style,
        item.label_selection.as_ref(),
        styles.selection,
        scroll_x,
    );

    if item.is_dir {
        draw_segment_scrolled(
            surface,
            area,
            &mut content_x,
            "/",
            styles.directory,
            scroll_x,
        );
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "tree label drawing keeps cursor, widths, icons, and selection styling independent"
)]
fn draw_label_scrolled(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    content_x: &mut u16,
    label: &str,
    base_style: Style,
    selection: Option<&Range<usize>>,
    selection_style: Style,
    scroll_x: u16,
) {
    let Some(selection) = selection.filter(|selection| !selection.is_empty()) else {
        draw_segment_scrolled(surface, area, content_x, label, base_style, scroll_x);
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
                draw_segment_scrolled(surface, area, content_x, &current, style, scroll_x);
                current.clear();
            }
            current_style = Some(style);
        }
        current.push(ch);
    }

    if let Some(style) = current_style {
        draw_segment_scrolled(surface, area, content_x, &current, style, scroll_x);
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

fn draw_segment_scrolled(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    content_x: &mut u16,
    text: &str,
    style: Style,
    scroll_x: u16,
) {
    if text.is_empty() || area.width == 0 {
        return;
    }

    let right = area.right();
    let style = tui::ratatui::to_ratatui_style(style);
    let view_end = scroll_x.saturating_add(area.width);

    for ch in text.chars() {
        let width = ch.width().unwrap_or(1).max(1) as u16;
        let char_start = *content_x;
        *content_x = content_x.saturating_add(width);

        // Skip glyphs that are fully left of the viewport, or that would be
        // split by the left clip edge.
        if char_start.saturating_add(width) <= scroll_x || char_start < scroll_x {
            continue;
        }
        // Fully right of the viewport — still advance content_x above.
        if char_start >= view_end {
            continue;
        }

        let screen_x = area.x.saturating_add(char_start.saturating_sub(scroll_x));
        if screen_x >= right {
            continue;
        }
        let remaining = right.saturating_sub(screen_x);
        if remaining == 0 {
            continue;
        }
        let mut buf = [0u8; 4];
        let rendered = ch.encode_utf8(&mut buf);
        surface.set_stringn(screen_x, area.y, rendered, remaining as usize, style);
    }
}

/// Columns a single status icon reserves at the right edge of a row: the
/// glyph plus one column of separation. `tree_list_scrolled` subtracts these
/// from the row's content rect, so anything that clamps horizontal scrolling
/// must reserve the same width or the row's tail becomes unreachable.
pub fn tree_list_status_icon_width(icon: &str) -> u16 {
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
    fn tree_list_scrolled_hides_leading_columns() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 10, 1));
        let item = TreeListItem::new("abcdefghij");

        tree_list_scrolled(
            &mut surface,
            Rect::new(0, 0, 10, 1),
            &[item],
            TreeListStyles::default(),
            None,
            3,
            0,
        );

        assert_eq!(surface[(0, 0)].symbol(), "d");
        assert_eq!(surface[(1, 0)].symbol(), "e");
    }

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

    /// Label placement is glyph-independent.
    ///
    /// This offset used to be computed from each row's *own* icon width, so a
    /// row whose glyph measured wider than its neighbours' had its label
    /// pushed right. Directory rows are the case that bit: an expanded parent
    /// and its collapsed children draw different glyphs (the file explorer's
    /// fallbacks straddle a BMP glyph, U+E5FF, and a plane-15 one, U+F0770),
    /// and a skew of three columns was enough to render children left of their
    /// parent. Every row now shares one column width, so the only input that
    /// can move a label is its depth.
    #[test]
    fn label_offset_is_independent_of_which_glyph_a_row_draws() {
        let column = tree_list_icon_column(1);
        // Same depth, same offset — there is no per-row glyph term left to
        // vary. (Widths 1 and 4 stood for a narrow vs. an over-wide glyph.)
        assert_eq!(tree_list_label_offset(1, 2, column), 2 + 2 + column);
        for widest in [1, 2, 4] {
            let column = tree_list_icon_column(widest);
            assert_eq!(
                tree_list_label_offset(1, 2, column),
                tree_list_label_offset(1, 2, column),
            );
            // …and the parent/child step stays two columns whatever the set.
            assert_eq!(
                tree_list_label_offset(1, 2, column) - tree_list_label_offset(0, 1, column),
                2
            );
        }
    }

    #[test]
    fn icon_column_reserves_the_gap_and_collapses_when_icons_are_off() {
        assert_eq!(tree_list_icon_column(1), 1 + TREE_ICON_GAP);
        assert_eq!(tree_list_icon_column(2), 2 + TREE_ICON_GAP);
        // Icons disabled: no column at all, so layout is unchanged.
        assert_eq!(tree_list_icon_column(0), 0);
        assert_eq!(tree_list_label_offset(1, 2, 0), 4);
    }

    /// A narrow glyph is padded out to the column, so the label lands at the
    /// same screen column as a row whose glyph fills the column.
    #[test]
    fn narrow_glyphs_are_padded_to_the_icon_column() {
        let column = tree_list_icon_column(2);
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 20, 2));
        let narrow = TreeListIcon::new("x", Style::default());
        let wide = TreeListIcon::new("ab", Style::default());
        let items = [
            TreeListItem::new("one").icon(Some(narrow)),
            TreeListItem::new("two").icon(Some(wide)),
        ];

        tree_list_scrolled(
            &mut surface,
            Rect::new(0, 0, 20, 2),
            &items,
            TreeListStyles::default(),
            None,
            0,
            column,
        );

        assert_eq!(surface[(column, 0)].symbol(), "o");
        assert_eq!(surface[(column, 1)].symbol(), "t");
    }

    /// The cue for a ranged row has to survive the common theme where
    /// `ui.selection` carries a background and no foreground. The old
    /// `selected` accent recoloured the tree connector's *foreground*, so
    /// under such a theme it resolved back to the guide style and painted
    /// nothing at all.
    #[test]
    fn ranged_rows_are_visible_under_a_background_only_selection_theme() {
        let selection = Style::default().bg(Color::Rgb(20, 40, 80));
        let styles = TreeListStyles {
            selection,
            ..TreeListStyles::default()
        };
        let paint = |ranged: bool| {
            let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 12, 1));
            let item = TreeListItem::new("alpha").selected(true).ranged(ranged);
            tree_list_scrolled(
                &mut surface,
                Rect::new(0, 0, 12, 1),
                &[item],
                styles,
                None,
                0,
                0,
            );
            surface
        };

        let plain = paint(false);
        let ranged = paint(true);
        assert_ne!(plain, ranged, "a ranged row must paint differently");
        let fill = tui::ratatui::to_ratatui_style(selection)
            .bg
            .expect("selection background");
        for x in 0..12 {
            assert_eq!(ranged[(x, 0)].bg, fill, "column {x} should carry the fill");
        }
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
