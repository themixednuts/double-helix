//! Anchored text drawing that Ratatui does not provide directly.

use tui::ratatui::{buffer::Buffer, style::Style};

/// Request object for drawing horizontally anchored text.
pub struct AnchoredText<'a> {
    x: u16,
    y: u16,
    truncate_start: bool,
    truncate_end: bool,
    text: &'a str,
    width: usize,
}

impl<'a> AnchoredText<'a> {
    /// Create an anchored text request.
    pub fn new(x: u16, y: u16, text: &'a str, width: usize) -> Self {
        Self {
            x,
            y,
            truncate_start: false,
            truncate_end: false,
            text,
            width,
        }
    }

    /// Enable or disable a leading truncation marker.
    pub fn truncate_start(mut self, truncate: bool) -> Self {
        self.truncate_start = truncate;
        self
    }

    /// Enable or disable a trailing truncation marker.
    pub fn truncate_end(mut self, truncate: bool) -> Self {
        self.truncate_end = truncate;
        self
    }
}

/// Draw a horizontally anchored string with optional edge truncation markers.
pub fn draw_string_anchored(
    surface: &mut Buffer,
    text: AnchoredText<'_>,
    style: &mut dyn FnMut(usize) -> Style,
) -> (u16, u16) {
    let AnchoredText {
        x,
        y,
        truncate_start,
        truncate_end,
        text,
        width,
    } = text;
    let area = *surface.area();
    if width == 0 || !area.contains(tui::ratatui::layout::Position::new(x, y)) {
        return (x, y);
    }

    let right = area
        .right()
        .min(x.saturating_add(width.try_into().unwrap_or(u16::MAX)));
    let available = right.saturating_sub(x) as usize;
    if available == 0 {
        return (x, y);
    }

    let mut next_x = x;
    let mut used = 0usize;
    if truncate_start {
        if let Some(cell) = surface.cell_mut((next_x, y)) {
            cell.set_symbol("…");
            cell.set_style(style(0));
        }
        next_x = next_x.saturating_add(1);
        used += 1;
    }

    let reserve_end = usize::from(truncate_end && used < available);
    let content_limit = available.saturating_sub(used + reserve_end);
    for grapheme in crate::ui::text_layout::visible_graphemes(text, content_limit) {
        if let Some(cell) = surface.cell_mut((next_x, y)) {
            cell.set_symbol(grapheme.text);
            cell.set_style(style(grapheme.byte));
        }
        for dx in 1..grapheme.width {
            if let Some(cell) = surface.cell_mut((next_x.saturating_add(dx as u16), y)) {
                cell.reset();
            }
        }
        next_x = next_x.saturating_add(grapheme.width as u16);
        used += grapheme.width;
    }

    if truncate_end && used < available {
        if let Some(cell) = surface.cell_mut((next_x, y)) {
            cell.set_symbol("…");
            cell.set_style(style(text.len()));
        }
        next_x = next_x.saturating_add(1);
    }

    (next_x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::ratatui::style::{Color as RatatuiColor, Style as RatatuiStyle};

    #[test]
    fn anchored_text_draws_with_ratatui_style() {
        let mut surface = Buffer::empty(tui::ratatui::layout::Rect::new(0, 0, 4, 1));
        let mut style = |_| RatatuiStyle::default().fg(RatatuiColor::LightBlue);

        draw_string_anchored(
            &mut surface,
            AnchoredText::new(0, 0, "abcdef", 4).truncate_end(true),
            &mut style,
        );

        assert_eq!(surface[(0, 0)].symbol(), "a");
        assert_eq!(surface[(0, 0)].fg, tui::ratatui::style::Color::LightBlue);
        assert_eq!(surface[(3, 0)].symbol(), "…");
    }
}
