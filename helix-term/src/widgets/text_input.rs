//! Single-line text input widget.
//!
//! Renders a text string with a visible cursor position. Handles truncation
//! when text is wider than the area, showing ellipsis indicators.

use helix_view::graphics::Rect;

use helix_view::graphics::Style;

use super::{draw_string_anchored, AnchoredText};

pub use helix_view::layout::TextInputLayout as TextInputState;

/// Render a single-line text input with cursor.
///
/// `text` is the full input string, `cursor` is the byte offset of the cursor
/// within `text`. The widget handles horizontal scrolling when the text is
/// wider than `area`.
///
/// Returns `TextInputState` with the computed cursor screen position.
pub fn text_input(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    text: &str,
    cursor: usize,
    style: Style,
    cursor_style: Style,
) -> TextInputState {
    let state = helix_view::layout::text_input_layout(area, text, cursor);

    if area.width == 0 || area.height == 0 {
        return state;
    }

    // Simple case: text fits entirely.
    if !state.truncated_start && !state.truncated_end {
        surface.set_string(area.x, area.y, text, tui::ratatui::to_ratatui_style(style));

        // Draw cursor cell.
        if state.cursor_in_area {
            {
                if let Some(cell) = surface.cell_mut((state.cursor_x, area.y)) {
                    cell.set_style(tui::ratatui::to_ratatui_style(cursor_style));
                }
            };
        }

        return state;
    }

    // Render with truncation indicators.
    let mut style_for_offset = |_| tui::ratatui::to_ratatui_style(style);
    draw_string_anchored(
        surface,
        AnchoredText::new(area.x, area.y, &text[state.anchor..], area.width as usize)
            .truncate_start(state.truncated_start)
            .truncate_end(state.truncated_end),
        &mut style_for_offset,
    );

    // Draw cursor cell.
    if state.cursor_in_area {
        {
            if let Some(cell) = surface.cell_mut((state.cursor_x, area.y)) {
                cell.set_style(tui::ratatui::to_ratatui_style(cursor_style));
            }
        };
    }

    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::ratatui::{buffer::Buffer as Surface, layout::Rect as SurfaceRect};

    #[test]
    fn truncated_input_keeps_first_visible_grapheme() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 4, 1));

        let state = text_input(
            &mut surface,
            Rect::new(0, 0, 4, 1),
            "abcdef",
            6,
            Style::default(),
            Style::default(),
        );

        assert!(state.truncated_start);
        assert!(!state.truncated_end);
        assert_eq!(surface[(0, 0)].symbol(), "…");
        assert_eq!(surface[(1, 0)].symbol(), "c");
        assert_eq!(surface[(2, 0)].symbol(), "d");
        assert_eq!(surface[(3, 0)].symbol(), "e");
    }

    #[test]
    fn ratatui_surface_renders_same_truncated_input() {
        let mut surface =
            crate::render::CellSurface::empty(tui::ratatui::layout::Rect::new(0, 0, 4, 1));

        let state = text_input(
            &mut surface,
            Rect::new(0, 0, 4, 1),
            "abcdef",
            6,
            Style::default(),
            Style::default(),
        );

        assert!(state.truncated_start);
        assert!(!state.truncated_end);
        assert_eq!(surface[(0, 0)].symbol(), "…");
        assert_eq!(surface[(1, 0)].symbol(), "c");
        assert_eq!(surface[(2, 0)].symbol(), "d");
        assert_eq!(surface[(3, 0)].symbol(), "e");
    }
}
