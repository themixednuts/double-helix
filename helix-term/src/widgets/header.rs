//! Header bar widget — renders a title line with optional counts.

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};
use tui::buffer::Buffer as Surface;

/// Render a simple header/title bar spanning the full width of `area`.
///
/// The title is left-aligned. The area is cleared with `style` first.
pub fn header(surface: &mut Surface, area: Rect, title: &str, style: Style) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    surface.clear_with(Rect::new(area.x, area.y, area.width, 1), style);
    let max_width = area.width as usize;
    surface.set_stringn(area.x, area.y, title, max_width, style);
}

/// Render a header with a right-aligned count indicator (e.g. "Files  3/120").
///
/// `title` is left-aligned, `current/total` is right-aligned.
pub fn header_with_counts(
    surface: &mut Surface,
    area: Rect,
    title: &str,
    current: usize,
    total: usize,
    style: Style,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    surface.clear_with(Rect::new(area.x, area.y, area.width, 1), style);

    let max_width = area.width as usize;
    surface.set_stringn(area.x, area.y, title, max_width, style);

    let count = format!("{}/{}", current, total);
    let count_width = count.width();
    if count_width < max_width {
        let x = area.right().saturating_sub(count_width as u16 + 1);
        surface.set_stringn(x, area.y, &count, count_width, style);
    }
}
