//! Single-line text input widget.
//!
//! Renders a text string with a visible cursor position. Handles truncation
//! when text is wider than the area, showing ellipsis indicators.

use helix_view::graphics::Rect;
use tui::buffer::Buffer as Surface;

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::Style;

/// Computed state returned by `text_input` for cursor positioning.
pub struct TextInputState {
    /// Screen position of the cursor (absolute x, y).
    pub cursor_x: u16,
    pub cursor_y: u16,
    /// Whether the text was truncated on the left.
    pub truncated_start: bool,
    /// Whether the text was truncated on the right.
    pub truncated_end: bool,
}

/// Render a single-line text input with cursor.
///
/// `text` is the full input string, `cursor` is the byte offset of the cursor
/// within `text`. The widget handles horizontal scrolling when the text is
/// wider than `area`.
///
/// Returns `TextInputState` with the computed cursor screen position.
pub fn text_input(
    surface: &mut Surface,
    area: Rect,
    text: &str,
    cursor: usize,
    style: Style,
    cursor_style: Style,
) -> TextInputState {
    if area.width == 0 || area.height == 0 {
        return TextInputState {
            cursor_x: area.x,
            cursor_y: area.y,
            truncated_start: false,
            truncated_end: false,
        };
    }

    let line_width = area.width as usize;

    // Simple case: text fits entirely.
    if text.width() <= line_width {
        surface.set_string(area.x, area.y, text, style);

        // Compute cursor screen position.
        let cursor_col = text[..cursor.min(text.len())].width() as u16;
        let cx = area.x + cursor_col;

        // Draw cursor cell.
        if cx < area.right() {
            let cell = &mut surface[(cx, area.y)];
            cell.set_style(cursor_style);
        }

        return TextInputState {
            cursor_x: cx.min(area.right().saturating_sub(1)),
            cursor_y: area.y,
            truncated_start: false,
            truncated_end: false,
        };
    }

    // Text is wider than area — need to scroll.
    // Find an anchor (byte offset) such that the cursor is visible.
    let anchor = compute_anchor(text, cursor, line_width);
    let truncated_start = anchor > 0;
    let truncated_end = text[anchor..].width() > line_width;

    // Render with truncation indicators.
    surface.set_string_anchored(
        area.x,
        area.y,
        truncated_start,
        truncated_end,
        &text[anchor..],
        line_width,
        |_| style,
    );

    // Compute cursor screen position relative to anchor.
    let cursor_col = text[anchor..cursor.min(text.len())].width() as u16;
    let cx = area.x + cursor_col;

    // Draw cursor cell.
    if cx < area.right() {
        let cell = &mut surface[(cx, area.y)];
        cell.set_style(cursor_style);
    }

    TextInputState {
        cursor_x: cx.min(area.right().saturating_sub(1)),
        cursor_y: area.y,
        truncated_start,
        truncated_end,
    }
}

/// Compute the byte offset anchor so that the cursor is visible within
/// `line_width` characters.
fn compute_anchor(text: &str, cursor: usize, line_width: usize) -> usize {
    use helix_core::unicode::segmentation::UnicodeSegmentation;

    if text[..cursor].width() <= line_width {
        return 0;
    }

    // Walk backwards from cursor to find an anchor that fits.
    let mut width = 0;
    text[..cursor]
        .grapheme_indices(true)
        .rev()
        .find_map(|(idx, g)| {
            width += g.width();
            if width > line_width {
                Some(idx + g.len())
            } else {
                None
            }
        })
        .unwrap_or(0)
}
