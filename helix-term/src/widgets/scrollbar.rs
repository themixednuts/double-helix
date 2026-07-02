//! Proportional scrollbar widget.
//!
//! Draws a 1-wide proportional scrollbar. Used by item_list, scroll_region,
//! menu, popup, and any component that needs a scrollbar.

use helix_view::graphics::{Rect, Style};
use std::borrow::Cow;

/// A proportional scrollbar rendered in a 1-wide column.
///
/// # Examples
///
/// ```ignore
/// // Simple scrollbar with default "▐" symbol:
/// Scrollbar::new(total_items, scroll_offset, visible_count)
///     .thumb_style(thumb)
///     .track(" ", track)
///     .render(area, surface);
///
/// // Thumb-only (no track rendering, e.g. over a border):
/// Scrollbar::new(total, offset, visible)
///     .symbol("▌")
///     .thumb_style(style)
///     .render(area, surface);
/// ```
pub struct Scrollbar {
    total: usize,
    offset: usize,
    visible: usize,
    thumb_symbol: Cow<'static, str>,
    thumb_style: Style,
    track: Option<(Cow<'static, str>, Style)>,
}

impl Scrollbar {
    /// Create a scrollbar for `total` items with `offset` as the first visible
    /// index and `visible` items shown.
    pub fn new(total: usize, offset: usize, visible: usize) -> Self {
        Self {
            total,
            offset,
            visible,
            thumb_symbol: Cow::Borrowed("▐"),
            thumb_style: Style::default(),
            track: None,
        }
    }

    /// Set the thumb symbol (default: "▐").
    pub fn symbol(mut self, symbol: impl Into<Cow<'static, str>>) -> Self {
        self.thumb_symbol = symbol.into();
        self
    }

    /// Set the thumb style.
    pub fn thumb_style(mut self, style: Style) -> Self {
        self.thumb_style = style;
        self
    }

    /// Enable track rendering with the given symbol and style.
    /// If not called, non-thumb cells are left untouched.
    pub fn track(mut self, symbol: impl Into<Cow<'static, str>>, style: Style) -> Self {
        self.track = Some((symbol.into(), style));
        self
    }

    /// Render the scrollbar into `area` on `surface`.
    pub fn render(self, area: Rect, surface: &mut crate::render::CellSurface) {
        let height = area.height as usize;
        if height == 0 || self.total == 0 {
            return;
        }

        let thumb_height = (height * height).div_ceil(self.total).max(1).min(height);
        let max_offset = self.total.saturating_sub(self.visible);
        let offset = self.offset.min(max_offset);
        let thumb_top = ((height - thumb_height) * offset)
            .checked_div(max_offset)
            .unwrap_or(0);

        for i in 0..height {
            if i >= thumb_top && i < thumb_top + thumb_height {
                {
                    if let Some(cell) = surface.cell_mut((area.x, area.y + i as u16)) {
                        cell.set_symbol(self.thumb_symbol.as_ref());
                        cell.set_style(tui::ratatui::to_ratatui_style(self.thumb_style));
                    }
                };
            } else if let Some((track_symbol, track_style)) = self.track.as_ref() {
                {
                    if let Some(cell) = surface.cell_mut((area.x, area.y + i as u16)) {
                        cell.set_symbol(track_symbol.as_ref());
                        cell.set_style(tui::ratatui::to_ratatui_style(*track_style));
                    }
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::ratatui::{buffer::Buffer, layout::Rect as RatatuiRect};

    #[test]
    fn max_offset_places_thumb_on_last_row() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 1, 10));

        Scrollbar::new(100, 90, 10)
            .track(".", Style::default())
            .render(Rect::new(0, 0, 1, 10), &mut surface);

        assert_eq!(surface[(0, 9)].symbol(), "▐");
    }

    #[test]
    fn overscroll_is_clamped_to_last_row() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 1, 10));

        Scrollbar::new(100, 900, 10)
            .track(".", Style::default())
            .render(Rect::new(0, 0, 1, 10), &mut surface);

        assert_eq!(surface[(0, 9)].symbol(), "▐");
    }
}
