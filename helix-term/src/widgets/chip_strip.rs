//! A row of styled "chips" — short labelled, styled cells laid out
//! horizontally. Used by the file explorer footer, the assistant
//! statusline, and any future panel chrome that wants the same look.
//!
//! # Why a helper, not a Component
//!
//! The chip-row pattern was previously hand-rolled in two places (file
//! explorer + assistant), each with its own clipping math, cursor
//! advance, and separator handling. The math is small but identical;
//! the helper consolidates it so future visual tweaks (chip padding,
//! width measurement for CJK, separator style) propagate to every
//! consumer.
//!
//! Stays a function rather than a `Component` because the host owns
//! the surrounding chrome (background fill, mode chip, count chip in
//! the file explorer; bar style, dot glyph, leading/trailing
//! positioning in the assistant). The host calls `chip_strip_left` or
//! `chip_strip_right` for the *interior* chip row; the rest of the
//! chrome is the host's responsibility.

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::Style;

/// One chip — a styled label cell. The label string is rendered
/// verbatim (including any padding the caller bakes into it), with
/// `style` applied to the cells it occupies. The caller decides
/// whether to wrap the label in spaces (` foo `) for visual padding
/// or leave it tight (`foo`); the widget doesn't add or strip them.
#[derive(Clone, Debug)]
pub struct Chip<'a> {
    pub label: &'a str,
    pub style: Style,
}

impl<'a> Chip<'a> {
    pub fn new(label: &'a str, style: Style) -> Self {
        Self { label, style }
    }
}

/// Render chips starting at `start_x`, growing right toward `end_x`.
/// Stops drawing if the next chip wouldn't fit. Returns the x position
/// just past the last drawn chip — the caller can use this as the
/// start of any subsequent content (or to detect "did we draw all of
/// them" by comparing chips iterated vs intended).
///
/// `start_x` is inclusive; `end_x` is exclusive (the first column the
/// caller wants left untouched, e.g. where a right-aligned count chip
/// will land). `y` is the row.
///
/// No separator between chips by default — the caller bakes any
/// separator (` ` / `  ` / `·`) into the chip labels themselves. This
/// keeps the helper simple and lets each host control its own
/// inter-chip rhythm.
pub fn chip_strip_left(
    surface: &mut crate::render::CellSurface,
    start_x: u16,
    end_x: u16,
    y: u16,
    chips: &[Chip<'_>],
) -> u16 {
    let mut cursor = start_x;
    for chip in chips {
        let width = UnicodeWidthStr::width(chip.label) as u16;
        if cursor.saturating_add(width) > end_x {
            break;
        }
        surface.set_stringn(
            cursor,
            y,
            chip.label,
            width as usize,
            tui::ratatui::to_ratatui_style(chip.style),
        );
        cursor = cursor.saturating_add(width);
    }
    cursor
}

/// Render chips right-aligned: the last chip ends at `end_x - 1`, and
/// earlier chips stack to its left. Stops drawing if a chip wouldn't
/// fit between `start_x` and the current running anchor. Returns the
/// x position of the leftmost drawn chip — the caller can use this
/// as the right edge of any preceding content (e.g. where to stop
/// drawing middle chips before the right cluster begins).
///
/// Chips are rendered in the order given: `chips[0]` is the leftmost
/// of the right-aligned cluster, `chips.last()` is the rightmost.
/// (This matches the usual mental model of "here's my list of chips,
/// align it to the right edge.")
pub fn chip_strip_right(
    surface: &mut crate::render::CellSurface,
    start_x: u16,
    end_x: u16,
    y: u16,
    chips: &[Chip<'_>],
) -> u16 {
    let total_width: u16 = chips
        .iter()
        .map(|chip| UnicodeWidthStr::width(chip.label) as u16)
        .sum();
    if total_width == 0 || total_width > end_x.saturating_sub(start_x) {
        return end_x;
    }
    let mut cursor = end_x.saturating_sub(total_width);
    let anchor = cursor;
    for chip in chips {
        let width = UnicodeWidthStr::width(chip.label) as u16;
        surface.set_stringn(
            cursor,
            y,
            chip.label,
            width as usize,
            tui::ratatui::to_ratatui_style(chip.style),
        );
        cursor = cursor.saturating_add(width);
    }
    anchor
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::ratatui::{buffer::Buffer, layout::Rect as RatatuiRect};

    fn read_row(surface: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| surface[(x, y)].symbol())
            .collect::<String>()
    }

    #[test]
    fn left_strip_lays_chips_in_order() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 20, 1));
        let style = Style::default();
        let chips = [
            Chip::new("aa", style),
            Chip::new("bb", style),
            Chip::new("cc", style),
        ];
        let end = chip_strip_left(&mut surface, 0, 20, 0, &chips);
        assert_eq!(end, 6);
        assert_eq!(&read_row(&surface, 0, 6), "aabbcc");
    }

    #[test]
    fn left_strip_stops_when_chip_would_overflow() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 20, 1));
        let style = Style::default();
        let chips = [Chip::new("aaa", style), Chip::new("bbb", style)];
        // Only 4 cells of width — second chip (3 cells) won't fit
        // alongside the first.
        let end = chip_strip_left(&mut surface, 0, 4, 0, &chips);
        assert_eq!(end, 3, "only first chip fits");
        assert_eq!(&read_row(&surface, 0, 4), "aaa ");
    }

    #[test]
    fn right_strip_lays_chips_anchored_to_end_x() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 20, 1));
        let style = Style::default();
        let chips = [Chip::new("xx", style), Chip::new("yy", style)];
        let anchor = chip_strip_right(&mut surface, 0, 20, 0, &chips);
        assert_eq!(anchor, 16, "two 2-wide chips land at columns 16-19");
        assert_eq!(&read_row(&surface, 0, 20), "                xxyy");
    }

    #[test]
    fn right_strip_returns_end_x_when_overflowing() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 20, 1));
        let style = Style::default();
        let chips = [Chip::new("xxxxxxxxxxxx", style)]; // 12 cells
                                                        // Only 5 cells of space — chip doesn't fit; nothing drawn.
        let anchor = chip_strip_right(&mut surface, 0, 5, 0, &chips);
        assert_eq!(anchor, 5, "anchor = end_x when no chip drawn");
        assert_eq!(read_row(&surface, 0, 20), " ".repeat(20));
    }

    #[test]
    fn empty_chip_list_is_a_noop() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 20, 1));
        let chips: &[Chip<'_>] = &[];
        let end_left = chip_strip_left(&mut surface, 5, 15, 0, chips);
        let anchor_right = chip_strip_right(&mut surface, 0, 20, 0, chips);
        assert_eq!(end_left, 5, "no chips → cursor stays at start_x");
        assert_eq!(anchor_right, 20, "no chips → anchor stays at end_x");
    }

    #[test]
    fn cjk_width_is_honored() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 10, 1));
        let style = Style::default();
        // "あ" is 2 cells wide; chip_strip should account for that.
        let chips = [Chip::new("あb", style)];
        let end = chip_strip_left(&mut surface, 0, 10, 0, &chips);
        assert_eq!(end, 3, "2-cell CJK + 1-cell latin = 3 cells");
    }
}
