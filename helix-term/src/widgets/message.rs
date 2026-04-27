//! Message widget for list-style message display.
//!
//! Renders a directional message frame with styled text content. Supports
//! left-aligned (agent) and right-aligned (user) messages with configurable
//! width and subtle animated border accents.

use helix_view::graphics::{Rect, Style};
use tui::buffer::Buffer as Surface;
use tui::text::Spans;

/// Alignment of the bubble within the available width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAlign {
    Left,
    Right,
}

/// Corner style for bubble borders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageCorners {
    #[default]
    Rounded,
    Squared,
}

/// Resolved styles for a chat bubble.
#[derive(Debug, Clone, Copy)]
pub struct MessageStyle {
    pub border: Style,
    pub corners: MessageCorners,
    pub accent: Option<Style>,
    pub accent_progress: f32,
}

/// Computed state returned by `message`.
pub struct MessageState {
    pub height: u16,
}

pub fn message(
    surface: &mut Surface,
    area: Rect,
    lines: &[Spans],
    bubble_width: u16,
    align: MessageAlign,
    style: MessageStyle,
    skip_top: usize,
) -> MessageState {
    let bubble_w = bubble_width.min(area.width) as usize;

    if bubble_w < 4 || area.height == 0 {
        return MessageState { height: 0 };
    }

    let content_skip = skip_top.min(lines.len());
    let visible_lines: Vec<_> = lines.iter().skip(content_skip).cloned().collect();
    let remaining_rows = lines.len().saturating_sub(content_skip) as u16 + 1;
    let visible_rows = remaining_rows.min(area.height);
    let has_bottom_border = remaining_rows <= area.height;

    let x_offset = match align {
        MessageAlign::Left => area.x,
        MessageAlign::Right => area.x + area.width.saturating_sub(bubble_w as u16),
    };

    let bw = bubble_w as u16;
    let surf_area = surface.area();
    let surf_right = surf_area.right();
    let surf_bottom = surf_area.bottom();
    let mut y = area.y;

    let border_cells = directional_border_cells(align, style.corners);
    let accent_style = style.accent.unwrap_or(style.border);
    let accent_progress = style.accent_progress.clamp(0.0, 1.0);

    let content_rows = if has_bottom_border {
        visible_rows.saturating_sub(1)
    } else {
        visible_rows
    };
    let accent = BorderAccent {
        horizontal_span: bubble_w,
        vertical_span: content_rows as usize,
        align,
        base: style.border,
        accent: accent_style,
        progress: accent_progress,
    };

    for row in 0..content_rows {
        let fy = y + row;
        if fy >= surf_bottom {
            continue;
        }

        for dx in 0..bubble_w {
            let x = x_offset + dx as u16;
            if x >= surf_right {
                continue;
            }
            surface[(x, fy)].set_symbol(" ");
        }

        if border_cells.left_vertical != " " {
            surface[(x_offset, fy)]
                .set_symbol(border_cells.left_vertical)
                .set_style(border_style_for_edge(
                    BorderEdge::Left,
                    row as usize,
                    accent,
                ));
        }

        let right_x = x_offset + bw.saturating_sub(1);
        if right_x < surf_right && border_cells.right_vertical != " " {
            surface[(right_x, fy)]
                .set_symbol(border_cells.right_vertical)
                .set_style(border_style_for_edge(
                    BorderEdge::Right,
                    row as usize,
                    accent,
                ));
        }
    }

    let content_limit = area.y + content_rows;

    for line in &visible_lines {
        if y >= content_limit {
            break;
        }

        let mut x = x_offset + 1;
        let right_limit = x_offset + bw - 1;
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

        y += 1;
    }

    if has_bottom_border && y < area.y + area.height {
        for dx in 0..bubble_w {
            let x = x_offset + dx as u16;
            if x >= surf_right || y >= surf_bottom {
                continue;
            }
            surface[(x, y)]
                .set_symbol(border_cells.bottom(dx, bubble_w, align))
                .set_style(border_style_for_edge(BorderEdge::Bottom, dx, accent));
        }
    }

    MessageState {
        height: visible_rows,
    }
}

#[derive(Clone, Copy)]
struct BorderCells {
    anchored_corner: &'static str,
    open_cap: &'static str,
    bottom_horizontal: &'static str,
    left_vertical: &'static str,
    right_vertical: &'static str,
}

impl BorderCells {
    fn bottom(&self, dx: usize, width: usize, align: MessageAlign) -> &'static str {
        match align {
            MessageAlign::Left if dx == 0 => self.anchored_corner,
            MessageAlign::Left if dx + 1 == width => self.open_cap,
            MessageAlign::Right if dx == 0 => self.open_cap,
            MessageAlign::Right if dx + 1 == width => self.anchored_corner,
            _ => self.bottom_horizontal,
        }
    }
}

#[derive(Clone, Copy)]
enum BorderEdge {
    Left,
    Bottom,
    Right,
}

#[derive(Clone, Copy)]
struct BorderAccent {
    horizontal_span: usize,
    vertical_span: usize,
    align: MessageAlign,
    base: Style,
    accent: Style,
    progress: f32,
}

fn directional_border_cells(align: MessageAlign, corners: MessageCorners) -> BorderCells {
    let (left_corner, right_corner, left_cap, right_cap) = match corners {
        MessageCorners::Rounded => ("╰", "╯", "╴", "╶"),
        MessageCorners::Squared => ("└", "┘", "╴", "╶"),
    };

    match align {
        MessageAlign::Left => BorderCells {
            anchored_corner: left_corner,
            open_cap: left_cap,
            bottom_horizontal: "─",
            left_vertical: "│",
            right_vertical: " ",
        },
        MessageAlign::Right => BorderCells {
            anchored_corner: right_corner,
            open_cap: right_cap,
            bottom_horizontal: "─",
            left_vertical: " ",
            right_vertical: "│",
        },
    }
}

fn border_style_for_edge(edge: BorderEdge, offset: usize, accent: BorderAccent) -> Style {
    if accent.progress <= 0.0 {
        return accent.base;
    }

    let Some(perimeter) = perimeter_position(
        edge,
        offset,
        accent.horizontal_span,
        accent.vertical_span,
        accent.align,
    ) else {
        return accent.base;
    };
    let lit = ((perimeter_total(accent.horizontal_span, accent.vertical_span) as f32)
        * accent.progress)
        .ceil()
        .max(1.0) as usize;
    if perimeter < lit {
        merge_border_style(accent.base, accent.accent)
    } else {
        accent.base
    }
}

fn merge_border_style(base: Style, accent: Style) -> Style {
    let mut merged = base;
    if let Some(fg) = accent.fg {
        merged = merged.fg(fg);
    }
    if let Some(bg) = accent.bg {
        merged = merged.bg(bg);
    }
    if let Some(underline_color) = accent.underline_color {
        merged = merged.underline_color(underline_color);
    }
    if let Some(underline_style) = accent.underline_style {
        merged = merged.underline_style(underline_style);
    }
    merged.add_modifier(accent.add_modifier)
}

fn perimeter_total(horizontal_span: usize, vertical_span: usize) -> usize {
    horizontal_span.max(1) + vertical_span.max(1)
}

fn perimeter_position(
    edge: BorderEdge,
    offset: usize,
    horizontal_span: usize,
    vertical_span: usize,
    align: MessageAlign,
) -> Option<usize> {
    let horizontal_span = horizontal_span.max(1);
    let vertical_span = vertical_span.max(1);
    match align {
        MessageAlign::Left => match edge {
            BorderEdge::Left => Some(offset),
            BorderEdge::Bottom => Some(vertical_span + offset),
            BorderEdge::Right => None,
        },
        MessageAlign::Right => match edge {
            BorderEdge::Right => Some(offset),
            BorderEdge::Bottom => {
                Some(vertical_span + horizontal_span.saturating_sub(1).saturating_sub(offset))
            }
            BorderEdge::Left => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::graphics::Color;

    #[test]
    fn corners_stay_normal_for_left_messages() {
        let cells = directional_border_cells(MessageAlign::Left, MessageCorners::Squared);
        assert_eq!(cells.left_vertical, "│");
        assert_eq!(cells.bottom(0, 8, MessageAlign::Left), "└");
        assert_eq!(cells.bottom(1, 8, MessageAlign::Left), "─");
        assert_eq!(cells.bottom(7, 8, MessageAlign::Left), "╴");
    }

    #[test]
    fn corners_stay_normal_for_right_messages() {
        let cells = directional_border_cells(MessageAlign::Right, MessageCorners::Squared);
        assert_eq!(cells.right_vertical, "│");
        assert_eq!(cells.bottom(0, 8, MessageAlign::Right), "╶");
        assert_eq!(cells.bottom(6, 8, MessageAlign::Right), "─");
        assert_eq!(cells.bottom(7, 8, MessageAlign::Right), "┘");
    }

    #[test]
    fn accent_starts_at_bottom_left_for_agent_border() {
        let base = Style::default();
        let accent = Style::default().fg(Color::Blue);
        let border = BorderAccent {
            horizontal_span: 10,
            vertical_span: 2,
            align: MessageAlign::Left,
            base,
            accent,
            progress: 0.2,
        };
        assert_eq!(
            border_style_for_edge(BorderEdge::Bottom, 0, border).fg,
            accent.fg
        );
        assert_eq!(
            border_style_for_edge(BorderEdge::Bottom, 9, border).fg,
            base.fg
        );
    }

    #[test]
    fn left_message_renders_open_frame() {
        let mut surface = Surface::empty(Rect::new(0, 0, 12, 4));
        let _ = message(
            &mut surface,
            Rect::new(0, 0, 12, 4),
            &[Spans::from("hi")],
            8,
            MessageAlign::Left,
            MessageStyle {
                border: Style::default(),
                corners: MessageCorners::Squared,
                accent: None,
                accent_progress: 0.0,
            },
            0,
        );

        assert_eq!(surface[(0, 0)].symbol.as_ref(), "│");
        assert_eq!(surface[(7, 0)].symbol.as_ref(), " ");
        assert_eq!(surface[(1, 0)].symbol.as_ref(), "h");
        assert_eq!(surface[(2, 0)].symbol.as_ref(), "i");
        assert_eq!(surface[(0, 1)].symbol.as_ref(), "└");
        assert_eq!(surface[(1, 1)].symbol.as_ref(), "─");
        assert_eq!(surface[(2, 1)].symbol.as_ref(), "─");
        assert_eq!(surface[(7, 1)].symbol.as_ref(), "╴");
    }

    #[test]
    fn right_message_renders_open_frame() {
        let mut surface = Surface::empty(Rect::new(0, 0, 12, 4));
        let _ = message(
            &mut surface,
            Rect::new(0, 0, 12, 4),
            &[Spans::from("hi")],
            8,
            MessageAlign::Right,
            MessageStyle {
                border: Style::default(),
                corners: MessageCorners::Squared,
                accent: None,
                accent_progress: 0.0,
            },
            0,
        );

        assert_eq!(surface[(5, 0)].symbol.as_ref(), "h");
        assert_eq!(surface[(6, 0)].symbol.as_ref(), "i");
        assert_eq!(surface[(11, 0)].symbol.as_ref(), "│");
        assert_eq!(surface[(4, 1)].symbol.as_ref(), "╶");
        assert_eq!(surface[(10, 1)].symbol.as_ref(), "─");
        assert_eq!(surface[(11, 1)].symbol.as_ref(), "┘");
    }
}
