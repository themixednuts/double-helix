//! Scrollable styled text region with scrollbar.
//!
//! Renders pre-styled lines into a scrollable viewport. Used for chat panels,
//! preview panes, documentation popups, or any multi-line styled content.

use helix_view::graphics::{Rect, Style};
use tui::text::Spans;

/// Styles for the scroll region widget.
#[derive(Default)]
pub struct ScrollStyles {
    /// Style for the scrollbar thumb.
    pub thumb: Style,
    /// Style for the scrollbar track.
    pub track: Style,
}

/// Computed state returned by `scroll_region`.
pub struct ScrollState {
    /// Maximum scroll value (total_lines - visible_height), or 0 if content fits.
    pub max_scroll: u16,
    /// Number of lines that are actually visible.
    pub visible_lines: u16,
}

/// Render scrollable styled text with an optional scrollbar.
///
/// `lines` are pre-styled `Spans` (from tui). `scroll` is the first visible
/// line index. If `show_scrollbar` is true and content exceeds the area, a
/// proportional scrollbar is drawn on the right edge.
///
/// Returns `ScrollState` with max_scroll and visible line count.
pub fn scroll_region(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    lines: &[Spans],
    scroll: u16,
    show_scrollbar: bool,
    styles: &ScrollStyles,
) -> ScrollState {
    if area.height == 0 || area.width == 0 {
        return ScrollState {
            max_scroll: 0,
            visible_lines: 0,
        };
    }

    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(area.height);
    let scroll = scroll.min(max_scroll);
    let visible_height = area.height;

    let needs_scrollbar = show_scrollbar && total > visible_height;
    let content_area = if needs_scrollbar {
        area.clip_right(1)
    } else {
        area
    };

    // Render visible lines.
    let start = scroll as usize;
    let end = (start + visible_height as usize).min(lines.len());

    for (row, line) in lines[start..end].iter().enumerate() {
        let y = content_area.y + row as u16;
        let mut x = content_area.x;

        for span in &line.0 {
            let remaining = content_area.right().saturating_sub(x) as usize;
            if remaining == 0 {
                break;
            }
            let text: &str = &span.content;
            let width = text.len().min(remaining);
            surface.set_stringn(
                x,
                y,
                text,
                width,
                tui::ratatui::to_ratatui_style(span.style),
            );
            x += width as u16;
        }
    }

    if needs_scrollbar {
        super::scrollbar::Scrollbar::new(total as usize, scroll as usize, visible_height as usize)
            .thumb_style(styles.thumb)
            .track(" ", styles.track)
            .render(Rect::new(area.right() - 1, area.y, 1, area.height), surface);
    }

    ScrollState {
        max_scroll,
        visible_lines: (end - start) as u16,
    }
}
