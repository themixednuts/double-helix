use helix_view::graphics::{Rect, Style};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProgressStyle {
    pub track: Style,
    pub fill: Style,
    pub label: Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressState {
    pub filled_cells: u16,
    pub partial_eighths: u8,
}

pub fn progress_bar(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    ratio: f32,
    label: Option<&str>,
    style: ProgressStyle,
) -> ProgressState {
    let state = progress_fill(area.width, ratio);
    if area.width == 0 || area.height == 0 {
        return state;
    }

    let y = area.y;
    let track = tui::ratatui::to_ratatui_style(style.track);
    let fill = tui::ratatui::to_ratatui_style(style.fill);
    for i in 0..area.width {
        let glyph = if i < state.filled_cells {
            "█"
        } else if i == state.filled_cells && state.partial_eighths > 0 {
            partial_block(state.partial_eighths)
        } else {
            " "
        };
        let cell_style = if i <= state.filled_cells { fill } else { track };
        surface.set_stringn(area.x + i, y, glyph, 1, cell_style);
    }

    if let Some(label) = label.filter(|label| !label.is_empty()) {
        let width = helix_core::unicode::width::UnicodeWidthStr::width(label) as u16;
        if width <= area.width {
            let x = area.x + area.width.saturating_sub(width) / 2;
            surface.set_stringn(
                x,
                y,
                label,
                area.width.saturating_sub(x.saturating_sub(area.x)) as usize,
                tui::ratatui::to_ratatui_style(style.label),
            );
        }
    }

    state
}

pub fn progress_fill(width: u16, ratio: f32) -> ProgressState {
    if width == 0 {
        return ProgressState {
            filled_cells: 0,
            partial_eighths: 0,
        };
    }
    let total = ratio.clamp(0.0, 1.0) * width as f32;
    let filled_cells = total.floor() as u16;
    let partial_eighths = ((total - filled_cells as f32) * 8.0).round() as u8;
    if partial_eighths >= 8 {
        ProgressState {
            filled_cells: (filled_cells + 1).min(width),
            partial_eighths: 0,
        }
    } else {
        ProgressState {
            filled_cells: filled_cells.min(width),
            partial_eighths,
        }
    }
}

fn partial_block(eighths: u8) -> &'static str {
    match eighths {
        1 => "▏",
        2 => "▎",
        3 => "▍",
        4 => "▌",
        5 => "▋",
        6 => "▊",
        7 => "▉",
        _ => "█",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_fill_handles_width_edges() {
        assert_eq!(progress_fill(0, 0.5).filled_cells, 0);
        assert_eq!(progress_fill(10, -1.0).filled_cells, 0);
        assert_eq!(progress_fill(10, 2.0).filled_cells, 10);
    }

    #[test]
    fn progress_fill_tracks_partial_eighths() {
        let state = progress_fill(10, 0.25);
        assert_eq!(state.filled_cells, 2);
        assert_eq!(state.partial_eighths, 4);
    }
}
