//! Scrollable item list widget with selection highlight and scrollbar.
//!
//! Renders a vertical list of items with one selected. Handles scroll offset
//! and draws a proportional scrollbar when items exceed the visible area.

use helix_view::graphics::{Rect, Style};
use tui::buffer::Buffer as Surface;

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

/// Render a scrollable item list with selection highlight.
///
/// `render_item` is called for each visible item with `(index, area, surface, is_selected)`.
/// The caller decides how to draw each item — this widget just handles layout,
/// selection, scrolling, and the scrollbar.
///
/// Returns `ListState` with the computed scroll and visible range.
pub fn item_list(
    surface: &mut Surface,
    area: Rect,
    item_count: usize,
    selected: Option<usize>,
    scroll: usize,
    styles: &ListStyles,
    render_item: impl Fn(usize, Rect, &mut Surface, bool),
) -> ListState {
    if area.height == 0 || area.width == 0 {
        return ListState {
            scroll: 0,
            visible_start: 0,
            visible_end: 0,
        };
    }

    let win_height = area.height as usize;
    let scroll = scroll.min(item_count.saturating_sub(win_height));
    let visible_end = (scroll + win_height).min(item_count);

    surface.clear_with(area, styles.normal);

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
            surface.clear_with(item_area, styles.selected);
        }

        render_item(item_idx, item_area, surface, is_selected);
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
