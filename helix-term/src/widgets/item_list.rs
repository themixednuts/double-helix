//! Scrollable item list widget with selection highlight and scrollbar.
//!
//! Renders a vertical list of items with one selected. Handles scroll offset
//! and draws a proportional scrollbar when items exceed the visible area.

use helix_view::graphics::{Rect, Style};

use helix_view::list_nav::ListViewport;

/// Styles for the item list widget.
#[derive(Default)]
pub struct ListStyles {
    /// Style for unselected items (background).
    pub normal: Style,
    /// Style for the selected item.
    pub selected: Style,
    /// Style for the scrollbar thumb.
    pub scrollbar_thumb: Style,
    /// Style for the scrollbar track.
    pub scrollbar_track: Style,
}

/// Computed state returned by `item_list`.
pub struct ListState {
    /// The scroll offset that was actually used (may differ from input if clamped).
    pub scroll: usize,
    /// Range of item indices that are visible.
    pub visible_start: usize,
    pub visible_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkedItems<'a> {
    marked: &'a [usize],
    glyph: &'a str,
}

impl<'a> MarkedItems<'a> {
    pub const fn new(marked: &'a [usize], glyph: &'a str) -> Self {
        Self { marked, glyph }
    }

    pub fn is_marked(self, index: usize) -> bool {
        self.marked.contains(&index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StickyRows<'a> {
    rows: &'a [usize],
}

impl<'a> StickyRows<'a> {
    pub const fn new(rows: &'a [usize]) -> Self {
        Self { rows }
    }

    pub fn pinned_row(self, scroll: usize) -> Option<usize> {
        pinned_sticky_row(self.rows, scroll)
    }
}

pub fn pinned_sticky_row(rows: &[usize], scroll: usize) -> Option<usize> {
    rows.iter().copied().take_while(|row| *row <= scroll).last()
}

/// Render a scrollable item list with selection highlight.
///
/// `render_item` is called for each visible item with `(index, area, surface, is_selected)`.
/// The caller decides how to draw each item — this widget just handles layout,
/// selection, scrolling, and the scrollbar.
///
/// Returns `ListState` with the computed scroll and visible range.
pub fn item_list<F>(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    item_count: usize,
    selected: Option<usize>,
    scroll: usize,
    styles: &ListStyles,
    render_item: F,
) -> ListState
where
    F: Fn(usize, Rect, &mut crate::render::CellSurface, bool),
{
    item_list_with_marks(
        surface,
        area,
        item_count,
        selected,
        scroll,
        None,
        styles,
        |index, area, surface, is_selected, _is_marked| {
            render_item(index, area, surface, is_selected);
        },
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "widget renderer keeps state, styles, and row callback explicit for call-site clarity"
)]
pub fn item_list_with_marks<F>(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    item_count: usize,
    selected: Option<usize>,
    scroll: usize,
    marks: Option<MarkedItems<'_>>,
    styles: &ListStyles,
    render_item: F,
) -> ListState
where
    F: Fn(usize, Rect, &mut crate::render::CellSurface, bool, bool),
{
    item_list_with_marks_and_sticky(
        surface,
        area,
        item_count,
        selected,
        scroll,
        marks,
        None,
        styles,
        render_item,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "widget renderer keeps state, styles, and row callback explicit for call-site clarity"
)]
pub fn item_list_with_marks_and_sticky<F>(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    item_count: usize,
    selected: Option<usize>,
    scroll: usize,
    marks: Option<MarkedItems<'_>>,
    sticky_rows: Option<StickyRows<'_>>,
    styles: &ListStyles,
    render_item: F,
) -> ListState
where
    F: Fn(usize, Rect, &mut crate::render::CellSurface, bool, bool),
{
    if area.height == 0 || area.width == 0 {
        return ListState {
            scroll: 0,
            visible_start: 0,
            visible_end: 0,
        };
    }

    let win_height = area.height as usize;
    let viewport = ListViewport::new(item_count, selected, win_height, scroll);
    let visible_range = viewport.selected_visible_range();
    let scroll = visible_range.start;
    let visible_end = visible_range.end;

    {
        let area = tui::ratatui::to_ratatui_rect(area);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
        surface.set_style(area, tui::ratatui::to_ratatui_style(styles.normal));
    };

    let needs_scrollbar = item_count > win_height;
    let content_area = if needs_scrollbar {
        area.clip_right(1)
    } else {
        area
    };

    for (row_idx, item_idx) in (scroll..visible_end).enumerate() {
        let is_selected = selected == Some(item_idx);
        let item_area = Rect::new(
            content_area.x,
            content_area.y + row_idx as u16,
            content_area.width,
            1,
        );

        if is_selected {
            {
                let area = tui::ratatui::to_ratatui_rect(item_area);
                tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
                surface.set_style(area, tui::ratatui::to_ratatui_style(styles.selected));
            };
        }

        let is_marked = marks.is_some_and(|marks| marks.is_marked(item_idx));
        let item_area = if let Some(marks) = marks.filter(|_| content_area.width > 1) {
            let glyph_style = if is_selected {
                styles.selected
            } else {
                styles.normal
            };
            surface.set_stringn(
                item_area.x,
                item_area.y,
                if is_marked { marks.glyph } else { " " },
                1,
                tui::ratatui::to_ratatui_style(glyph_style),
            );
            item_area.clip_left(1)
        } else {
            item_area
        };

        render_item(item_idx, item_area, surface, is_selected, is_marked);
    }

    if let Some(sticky_idx) = sticky_rows.and_then(|rows| rows.pinned_row(scroll)) {
        if sticky_idx < item_count {
            let item_area = Rect::new(content_area.x, content_area.y, content_area.width, 1);
            {
                let area = tui::ratatui::to_ratatui_rect(item_area);
                tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
                surface.set_style(area, tui::ratatui::to_ratatui_style(styles.normal));
            };
            render_item(
                sticky_idx,
                item_area,
                surface,
                selected == Some(sticky_idx),
                false,
            );
        }
    }

    if needs_scrollbar {
        super::scrollbar::Scrollbar::new(item_count, scroll, win_height)
            .thumb_style(styles.scrollbar_thumb)
            .track(" ", styles.scrollbar_track)
            .render(Rect::new(area.right() - 1, area.y, 1, area.height), surface);
    }

    ListState {
        scroll,
        visible_start: scroll,
        visible_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marked_items_reports_marked_indices() {
        let marks = MarkedItems::new(&[1, 3], "✓");
        assert!(marks.is_marked(1));
        assert!(!marks.is_marked(2));
    }

    #[test]
    fn sticky_header_pins_last_header_at_or_before_scroll() {
        let headers = [0, 4, 9];
        assert_eq!(pinned_sticky_row(&headers, 0), Some(0));
        assert_eq!(pinned_sticky_row(&headers, 3), Some(0));
        assert_eq!(pinned_sticky_row(&headers, 4), Some(4));
        assert_eq!(pinned_sticky_row(&headers, 8), Some(4));
        assert_eq!(pinned_sticky_row(&headers, 9), Some(9));
        assert_eq!(pinned_sticky_row(&headers, 99), Some(9));
    }

    #[test]
    fn sticky_header_is_none_before_first_group() {
        assert_eq!(pinned_sticky_row(&[2, 5], 0), None);
    }

    #[test]
    fn item_list_scrolls_selected_row_into_view() {
        let mut surface =
            crate::render::CellSurface::empty(tui::ratatui::layout::Rect::new(0, 0, 10, 3));
        let state = item_list(
            &mut surface,
            Rect::new(0, 0, 10, 3),
            10,
            Some(7),
            0,
            &ListStyles::default(),
            |_index, _area, _surface, _selected| {},
        );

        assert_eq!(state.scroll, 5);
        assert_eq!(state.visible_start, 5);
        assert_eq!(state.visible_end, 8);
    }
}
