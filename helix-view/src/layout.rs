//! Layout combinators — pure geometry functions for splitting and positioning areas.
//!
//! These operate on `Rect` and are frontend-agnostic. Both terminal and GUI
//! frontends can use these to compute sub-areas for widget rendering.

use crate::graphics::Rect;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_core::Position;
use std::num::NonZeroU16;

/// How much space a region should take in a layout split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// Exactly N cells. `None` = collapsed (0 cells).
    Fixed(Option<NonZeroU16>),
    /// Percentage of parent (0..=100).
    Percent(u8),
    /// Take all remaining space after fixed/percent regions are allocated.
    Fill,
    /// Take remaining space, clamped to [min, max].
    Constrained { min: u16, max: NonZeroU16 },
}

impl Size {
    /// Create a fixed-size slot. 0 produces a collapsed slot (`Fixed(None)`).
    pub fn fixed(n: u16) -> Self {
        Size::Fixed(NonZeroU16::new(n))
    }

    /// Create a constrained slot. Max saturates to 1 if given 0.
    pub fn constrained(min: u16, max: u16) -> Self {
        Size::Constrained {
            min,
            max: NonZeroU16::new(max).unwrap_or(NonZeroU16::MIN),
        }
    }
}

/// Which direction a divider or split runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Horizontal,
    Vertical,
}

/// Preferred direction when positioning a popup near an anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorBias {
    Above,
    Below,
    Left,
    Right,
}

/// Split an area vertically (top to bottom) according to the given sizes.
///
/// Fixed and Percent sizes are allocated first. Fill/Constrained sizes share
/// the remaining space equally. Returns one `Rect` per size entry.
///
/// If the sizes exceed the available height, later regions get zero height.
pub fn split_vertical(area: Rect, sizes: &[Size]) -> Box<[Rect]> {
    let rects = allocate(area.height, sizes);
    let mut y = area.y;
    rects
        .iter()
        .map(|&h| {
            let r = Rect::new(area.x, y, area.width, h);
            y = y.saturating_add(h);
            r
        })
        .collect()
}

/// Split an area horizontally (left to right) according to the given sizes.
///
/// Same allocation logic as `split_vertical` but along the width axis.
pub fn split_horizontal(area: Rect, sizes: &[Size]) -> Box<[Rect]> {
    let rects = allocate(area.width, sizes);
    let mut x = area.x;
    rects
        .iter()
        .map(|&w| {
            let r = Rect::new(x, area.y, w, area.height);
            x = x.saturating_add(w);
            r
        })
        .collect()
}

/// Compute a centered sub-area within `area`. If the requested size exceeds
/// the area, the result is clamped to fit.
pub fn center(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Compute a popup position anchored near a point, preferring the given direction.
///
/// If the popup doesn't fit in the preferred direction, it flips to the opposite
/// side. If it still doesn't fit, it's clamped to the viewport edges.
pub fn anchor_near(viewport: Rect, anchor: Position, size: (u16, u16), bias: AnchorBias) -> Rect {
    let (w, h) = (size.0.min(viewport.width), size.1.min(viewport.height));
    let anchor_x = anchor.col as u16 + viewport.x;
    let anchor_y = anchor.row as u16 + viewport.y;

    let (x, y) = match bias {
        AnchorBias::Below => {
            let y = if anchor_y + 1 + h <= viewport.bottom() {
                anchor_y + 1
            } else if anchor_y >= viewport.y + h {
                anchor_y.saturating_sub(h)
            } else {
                viewport.bottom().saturating_sub(h)
            };
            let x = anchor_x.min(viewport.right().saturating_sub(w));
            (x, y)
        }
        AnchorBias::Above => {
            let y = if anchor_y >= viewport.y + h {
                anchor_y.saturating_sub(h)
            } else if anchor_y + 1 + h <= viewport.bottom() {
                anchor_y + 1
            } else {
                viewport.y
            };
            let x = anchor_x.min(viewport.right().saturating_sub(w));
            (x, y)
        }
        AnchorBias::Right => {
            let x = if anchor_x + 1 + w <= viewport.right() {
                anchor_x + 1
            } else if anchor_x >= viewport.x + w {
                anchor_x.saturating_sub(w)
            } else {
                viewport.right().saturating_sub(w)
            };
            let y = anchor_y.min(viewport.bottom().saturating_sub(h));
            (x, y)
        }
        AnchorBias::Left => {
            let x = if anchor_x >= viewport.x + w {
                anchor_x.saturating_sub(w)
            } else if anchor_x + 1 + w <= viewport.right() {
                anchor_x + 1
            } else {
                viewport.x
            };
            let y = anchor_y.min(viewport.bottom().saturating_sub(h));
            (x, y)
        }
    };

    Rect::new(x.max(viewport.x), y.max(viewport.y), w, h)
}

/// Frontend-agnostic layout state for a single-line text input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextInputLayout {
    /// Byte offset where visible text should begin.
    pub anchor: usize,
    /// Screen position of the cursor.
    pub cursor_x: u16,
    /// Screen position of the cursor.
    pub cursor_y: u16,
    /// Whether the cursor cell is inside the input area and can be styled.
    pub cursor_in_area: bool,
    /// Whether the text is clipped before `anchor`.
    pub truncated_start: bool,
    /// Whether the text extends past the visible area.
    pub truncated_end: bool,
}

/// Compute the visible anchor and cursor position for a single-line text input.
pub fn text_input_layout(area: Rect, text: &str, cursor: usize) -> TextInputLayout {
    let cursor = clamp_to_char_boundary(text, cursor);

    if area.width == 0 || area.height == 0 {
        return TextInputLayout {
            anchor: 0,
            cursor_x: area.x,
            cursor_y: area.y,
            cursor_in_area: false,
            truncated_start: false,
            truncated_end: false,
        };
    }

    let line_width = area.width as usize;

    if text.width() <= line_width {
        let cursor_col = text[..cursor].width() as u16;
        let cursor_x = area.x.saturating_add(cursor_col);
        return TextInputLayout {
            anchor: 0,
            cursor_x: cursor_x.min(area.right().saturating_sub(1)),
            cursor_y: area.y,
            cursor_in_area: cursor_x < area.right(),
            truncated_start: false,
            truncated_end: false,
        };
    }

    let anchor = text_input_anchor(text, cursor, line_width);
    let truncated_start = anchor > 0;
    let truncated_end = text[anchor..].width() > line_width;
    let cursor_col = if cursor >= anchor {
        text[anchor..cursor].width() as u16
    } else {
        0
    };
    let cursor_x = area.x.saturating_add(cursor_col);

    TextInputLayout {
        anchor,
        cursor_x: cursor_x.min(area.right().saturating_sub(1)),
        cursor_y: area.y,
        cursor_in_area: cursor_x < area.right(),
        truncated_start,
        truncated_end,
    }
}

fn text_input_anchor(text: &str, cursor: usize, line_width: usize) -> usize {
    use helix_core::unicode::segmentation::UnicodeSegmentation;

    if text[..cursor].width() <= line_width {
        return 0;
    }

    let mut width = 0;
    text[..cursor]
        .grapheme_indices(true)
        .rev()
        .find_map(|(idx, grapheme)| {
            width += grapheme.width();
            if width > line_width {
                Some(idx + grapheme.len())
            } else {
                None
            }
        })
        .unwrap_or(0)
}

fn clamp_to_char_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// Internal: allocate sizes along a single axis, returning the resolved
/// cell count for each entry.
fn allocate(total: u16, sizes: &[Size]) -> Box<[u16]> {
    let mut results = vec![0u16; sizes.len()];
    let mut remaining = total;
    let mut fill_count = 0u16;

    // First pass: allocate fixed and percent sizes.
    for (i, size) in sizes.iter().enumerate() {
        match *size {
            Size::Fixed(Some(px)) => {
                let alloc = px.get().min(remaining);
                results[i] = alloc;
                remaining = remaining.saturating_sub(alloc);
            }
            Size::Fixed(None) => {
                // Collapsed slot — 0 cells.
            }
            Size::Percent(pct) => {
                let alloc = ((total as u32 * pct as u32) / 100) as u16;
                let alloc = alloc.min(remaining);
                results[i] = alloc;
                remaining = remaining.saturating_sub(alloc);
            }
            Size::Fill | Size::Constrained { .. } => {
                fill_count += 1;
            }
        }
    }

    // Second pass: distribute remaining space to Fill/Constrained.
    if let Some(per_fill) = remaining.checked_div(fill_count) {
        let mut extra = remaining % fill_count;

        for (i, size) in sizes.iter().enumerate() {
            match *size {
                Size::Fill => {
                    let bonus = if extra > 0 {
                        extra = extra.saturating_sub(1);
                        1
                    } else {
                        0
                    };
                    results[i] = per_fill + bonus;
                }
                Size::Constrained { min, max } => {
                    let bonus = if extra > 0 {
                        extra = extra.saturating_sub(1);
                        1
                    } else {
                        0
                    };
                    results[i] = (per_fill + bonus).clamp(min, max.get());
                }
                _ => {}
            }
        }
    }

    results.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    // ─── split_vertical ─────────────────────────────────────────────

    #[test]
    fn split_vertical_fixed() {
        let rects = split_vertical(area(), &[Size::fixed(1), Size::fixed(3)]);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], Rect::new(0, 0, 80, 1));
        assert_eq!(rects[1], Rect::new(0, 1, 80, 3));
    }

    #[test]
    fn split_vertical_fill_takes_remaining() {
        let rects = split_vertical(area(), &[Size::fixed(4), Size::Fill]);
        assert_eq!(rects[0].height, 4);
        assert_eq!(rects[1].height, 20);
        assert_eq!(rects[1].y, 4);
    }

    #[test]
    fn split_vertical_percent() {
        let rects = split_vertical(area(), &[Size::Percent(50), Size::Fill]);
        assert_eq!(rects[0].height, 12);
        assert_eq!(rects[1].height, 12);
    }

    #[test]
    fn split_vertical_multiple_fills_share_equally() {
        let rects = split_vertical(area(), &[Size::fixed(4), Size::Fill, Size::Fill]);
        assert_eq!(rects[0].height, 4);
        assert_eq!(rects[1].height, 10);
        assert_eq!(rects[2].height, 10);
    }

    #[test]
    fn split_vertical_constrained_clamps() {
        let rects = split_vertical(area(), &[Size::constrained(5, 8), Size::Fill]);
        // 24 total, per_fill = 12, clamped to max=8
        assert_eq!(rects[0].height, 8);
        assert_eq!(rects[1].height, 12);
    }

    #[test]
    fn split_vertical_overflow_clamps() {
        let rects = split_vertical(area(), &[Size::fixed(20), Size::fixed(20)]);
        assert_eq!(rects[0].height, 20);
        assert_eq!(rects[1].height, 4); // only 4 remaining
    }

    #[test]
    fn split_vertical_preserves_x_and_width() {
        let a = Rect::new(10, 5, 60, 20);
        let rects = split_vertical(a, &[Size::fixed(5), Size::Fill]);
        assert_eq!(rects[0].x, 10);
        assert_eq!(rects[0].width, 60);
        assert_eq!(rects[1].x, 10);
        assert_eq!(rects[1].width, 60);
        assert_eq!(rects[0].y, 5);
        assert_eq!(rects[1].y, 10);
    }

    // ─── split_horizontal ───────────────────────────────────────────

    #[test]
    fn split_horizontal_fixed_and_fill() {
        let rects = split_horizontal(area(), &[Size::fixed(20), Size::Fill]);
        assert_eq!(rects[0], Rect::new(0, 0, 20, 24));
        assert_eq!(rects[1], Rect::new(20, 0, 60, 24));
    }

    #[test]
    fn split_horizontal_preserves_y_and_height() {
        let a = Rect::new(5, 10, 100, 30);
        let rects = split_horizontal(a, &[Size::Percent(30), Size::Fill]);
        assert_eq!(rects[0].y, 10);
        assert_eq!(rects[0].height, 30);
        assert_eq!(rects[1].y, 10);
        assert_eq!(rects[1].height, 30);
    }

    // ─── center ─────────────────────────────────────────────────────

    #[test]
    fn center_fits() {
        let r = center(area(), 40, 10);
        assert_eq!(r, Rect::new(20, 7, 40, 10));
    }

    #[test]
    fn center_clamps_when_too_large() {
        let r = center(area(), 200, 200);
        assert_eq!(r, Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn center_with_offset_area() {
        let a = Rect::new(10, 10, 80, 24);
        let r = center(a, 40, 10);
        assert_eq!(r, Rect::new(30, 17, 40, 10));
    }

    // ─── anchor_near ────────────────────────────────────────────────

    #[test]
    fn anchor_below_fits() {
        let r = anchor_near(
            area(),
            Position { row: 5, col: 10 },
            (20, 5),
            AnchorBias::Below,
        );
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 6); // anchor_y + 1
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 5);
    }

    #[test]
    fn anchor_below_flips_to_above_when_no_room() {
        let r = anchor_near(
            area(),
            Position { row: 22, col: 10 },
            (20, 5),
            AnchorBias::Below,
        );
        assert!(r.y < 22); // should flip above
        assert_eq!(r.height, 5);
    }

    #[test]
    fn anchor_right_fits() {
        let r = anchor_near(
            area(),
            Position { row: 5, col: 10 },
            (20, 5),
            AnchorBias::Right,
        );
        assert_eq!(r.x, 11); // anchor_x + 1
        assert_eq!(r.y, 5);
    }

    #[test]
    fn anchor_clamps_to_viewport() {
        let r = anchor_near(
            area(),
            Position { row: 0, col: 70 },
            (20, 5),
            AnchorBias::Right,
        );
        assert!(r.right() <= 80);
    }

    // ─── text_input_layout ─────────────────────────────────────────

    #[test]
    fn text_input_layout_fits_text() {
        let state = text_input_layout(Rect::new(2, 3, 10, 1), "hello", 3);

        assert_eq!(state.anchor, 0);
        assert_eq!(state.cursor_x, 5);
        assert_eq!(state.cursor_y, 3);
        assert!(state.cursor_in_area);
        assert!(!state.truncated_start);
        assert!(!state.truncated_end);
    }

    #[test]
    fn text_input_layout_scrolls_to_cursor() {
        let state = text_input_layout(Rect::new(0, 0, 4, 1), "abcdef", 6);

        assert_eq!(state.anchor, 2);
        assert_eq!(state.cursor_x, 3);
        assert!(!state.cursor_in_area);
        assert!(state.truncated_start);
        assert!(!state.truncated_end);
    }

    #[test]
    fn text_input_layout_clamps_invalid_cursor_boundary() {
        let state = text_input_layout(Rect::new(0, 0, 5, 1), "aé", 2);

        assert_eq!(state.anchor, 0);
        assert_eq!(state.cursor_x, 1);
        assert!(state.cursor_in_area);
    }

    #[test]
    fn text_input_layout_reports_full_width_cursor_outside_area() {
        let state = text_input_layout(Rect::new(0, 0, 4, 1), "abcd", 4);

        assert_eq!(state.cursor_x, 3);
        assert!(!state.cursor_in_area);
    }

    // ─── empty input ────────────────────────────────────────────────

    #[test]
    fn split_empty_sizes_returns_empty() {
        let rects = split_vertical(area(), &[]);
        assert!(rects.is_empty());
    }

    #[test]
    fn split_zero_area() {
        let a = Rect::new(0, 0, 0, 0);
        let rects = split_vertical(a, &[Size::fixed(5), Size::Fill]);
        assert_eq!(rects[0].height, 0);
        assert_eq!(rects[1].height, 0);
    }
}
