//! Divider widget — horizontal or vertical line separator.

use helix_view::graphics::{Rect, Style};
use tui::buffer::Buffer as Surface;

/// Render a horizontal divider ("─") across the top row of `area`.
pub fn hdivider(surface: &mut Surface, area: Rect, style: Style) {
    for x in area.left()..area.right() {
        let cell = &mut surface[(x, area.y)];
        cell.set_symbol("─");
        cell.set_style(style);
    }
}

/// Render a vertical divider ("│") down the left column of `area`.
pub fn vdivider(surface: &mut Surface, area: Rect, style: Style) {
    for y in area.top()..area.bottom() {
        let cell = &mut surface[(area.x, y)];
        cell.set_symbol("│");
        cell.set_style(style);
    }
}
