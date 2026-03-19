//! Chat bubble widget for message display.
//!
//! Renders a bordered, background-filled message bubble with styled text
//! content. Supports left-aligned (agent) and right-aligned (user) bubbles
//! with configurable width, border style, and background color.
//!
//! Background fill is applied only to the interior cells (between borders),
//! so it never bleeds outside the border glyphs — rounded or squared.

use helix_view::graphics::{Rect, Style};
use tui::buffer::Buffer as Surface;
use tui::text::Spans;

/// Alignment of the bubble within the available width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BubbleAlign {
    Left,
    Right,
}

/// Corner style for bubble borders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BubbleCorners {
    /// Rounded corners: ╭╮╰╯
    Rounded,
    /// Squared corners: ┌┐└┘
    Squared,
}

impl Default for BubbleCorners {
    fn default() -> Self {
        Self::Rounded
    }
}

/// Resolved styles for a chat bubble.
#[derive(Debug, Clone, Copy)]
pub struct BubbleStyle {
    /// Border characters style.
    pub border: Style,
    /// Background fill style (applied to interior cells within the borders).
    pub background: Style,
    /// Corner style (rounded or squared).
    pub corners: BubbleCorners,
}

/// Computed state returned by `chat_bubble`.
pub struct BubbleState {
    /// Total rows consumed (including borders + content lines).
    pub height: u16,
}

/// Render a bordered chat bubble with background fill.
///
/// `lines` are pre-styled content lines (already wrapped to fit `inner_width`).
/// `skip_top` is the number of logical rows clipped from the top (0 = full
/// bubble, 1 = top border hidden, 2+ = top border + content lines hidden).
///
/// Returns `BubbleState` with the total height consumed.
pub fn chat_bubble(
    surface: &mut Surface,
    area: Rect,
    lines: &[Spans],
    bubble_width: u16,
    align: BubbleAlign,
    style: BubbleStyle,
    skip_top: usize,
) -> BubbleState {
    let bubble_w = bubble_width.min(area.width) as usize;

    if bubble_w < 4 || area.height == 0 {
        return BubbleState { height: 0 };
    }

    let draw_top_border = skip_top == 0;
    // Content lines to skip (after skipping the top border row).
    let content_skip = skip_top.saturating_sub(1);
    let visible_lines: Vec<_> = lines.iter().skip(content_skip).cloned().collect();

    // Rows needed: optional top border + visible content + bottom border.
    let rows_needed = (if draw_top_border { 1u16 } else { 0 }) + visible_lines.len() as u16 + 1; // bottom border
    let visible_rows = rows_needed.min(area.height);
    let has_bottom_border = rows_needed <= area.height;

    // Horizontal position based on alignment.
    let x_offset = match align {
        BubbleAlign::Left => area.x,
        BubbleAlign::Right => area.x + area.width.saturating_sub(bubble_w as u16),
    };

    let bw = bubble_w as u16;
    let surf_area = surface.area();
    let surf_right = surf_area.right();
    let surf_bottom = surf_area.bottom();

    // Track current y position.
    let mut y = area.y;

    // Select corner characters.
    let (tl, tr, bl, br) = match style.corners {
        BubbleCorners::Rounded => ("╭", "╮", "╰", "╯"),
        BubbleCorners::Squared => ("┌", "┐", "└", "┘"),
    };

    // Top border.
    if draw_top_border {
        let top = format!("{tl}{}{tr}", "─".repeat(bubble_w.saturating_sub(2)));
        surface.set_stringn(x_offset, y, &top, bubble_w, style.border);
        y += 1;
    }

    // Fill interior with background (between borders).
    let interior_x = x_offset + 1;
    let interior_w = bw.saturating_sub(2);
    let content_rows = if has_bottom_border {
        visible_rows - (if draw_top_border { 2 } else { 1 }) // minus top+bottom or just bottom
    } else {
        visible_rows - (if draw_top_border { 1 } else { 0 }) // minus top or nothing
    };

    for row in 0..content_rows {
        let fy = y + row;
        for x in interior_x..(interior_x + interior_w) {
            if x < surf_right && fy < surf_bottom {
                surface[(x, fy)].set_style(style.background).set_symbol(" ");
            }
        }
    }

    // Determine the last row we can render content on.
    let content_limit = if has_bottom_border {
        area.y + visible_rows - 1
    } else {
        area.y + visible_rows
    };

    // Content lines with side borders.
    for line in &visible_lines {
        if y >= content_limit {
            break;
        }

        // Left border + padding.
        surface.set_stringn(x_offset, y, "│ ", 2, style.border);

        // Render spans.
        let mut x = x_offset + 2;
        let right_limit = x_offset + bw - 2;
        for span in &line.0 {
            let remaining = right_limit.saturating_sub(x) as usize;
            if remaining == 0 {
                break;
            }
            let text: &str = &span.content;
            let width = text.len().min(remaining);
            surface.set_stringn(x, y, text, width, span.style);
            x += width as u16;
        }

        // Right border.
        let right_x = x_offset + bw - 2;
        if right_x + 2 <= area.right() {
            surface.set_stringn(right_x, y, " │", 2, style.border);
        }

        y += 1;
    }

    // Bottom border.
    if has_bottom_border && y < area.y + area.height {
        let bot = format!("{bl}{}{br}", "─".repeat(bubble_w.saturating_sub(2)));
        surface.set_stringn(x_offset, y, &bot, bubble_w, style.border);
    }

    BubbleState {
        height: visible_rows,
    }
}
