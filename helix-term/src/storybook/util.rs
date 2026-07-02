//! Drawing primitives shared by storybook stories.
//!
//! These are small helpers on top of the Ratatui buffer: filling a rect,
//! clipping a single row of text, splitting a rect into halves or thirds,
//! drawing a framed panel, and other primitives the catalog stories reach
//! for. They're independent of any story's content — story modules import
//! them to compose their layouts.

use std::borrow::Cow;

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};
use tui::ratatui::{
    buffer::Buffer,
    widgets::{Clear, Widget},
};
use tui::text::{Span as HSpan, Spans};

use super::model::UiStyleGuide;

pub(super) fn fill_rect(surface: &mut Buffer, area: Rect, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let area = tui::ratatui::to_ratatui_rect(area);
    Clear.render(area, surface);
    surface.set_style(area, tui::ratatui::to_ratatui_style(style));
}

pub(super) fn render_panel(
    surface: &mut Buffer,
    area: Rect,
    title: &'static str,
    styles: UiStyleGuide,
) -> Rect {
    render_panel_with_corners(surface, area, title, styles, true)
}

pub(super) fn render_panel_with_corners(
    surface: &mut Buffer,
    area: Rect,
    title: &'static str,
    styles: UiStyleGuide,
    rounded: bool,
) -> Rect {
    crate::widgets::Panel::framed(
        crate::widgets::PanelStyle::new(styles.popup, styles.popup_border, styles.accent),
        rounded,
    )
    .title(title)
    .render(surface, area)
}

pub(super) fn inset(area: Rect, x: u16, y: u16) -> Rect {
    let x = x.min(area.width);
    let y = y.min(area.height);
    Rect::new(
        area.x.saturating_add(x),
        area.y.saturating_add(y),
        area.width.saturating_sub(x.saturating_mul(2)),
        area.height.saturating_sub(y.saturating_mul(2)),
    )
}

pub(super) fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub(super) fn set_clipped(surface: &mut Buffer, area: Rect, row: u16, text: &str, style: Style) {
    if row >= area.height || area.width == 0 {
        return;
    }
    surface.set_stringn(
        area.x,
        area.y.saturating_add(row),
        text,
        area.width as usize,
        tui::ratatui::to_ratatui_style(style),
    );
}

pub(super) fn set_right_clipped(surface: &mut Buffer, area: Rect, text: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let width = text.width().min(area.width as usize) as u16;
    surface.set_stringn(
        area.x + area.width.saturating_sub(width),
        area.y,
        text,
        width as usize,
        tui::ratatui::to_ratatui_style(style),
    );
}

pub(super) fn hline(text: impl Into<Cow<'static, str>>, style: Style) -> Spans<'static> {
    Spans::from(HSpan::styled(text, style))
}

pub(super) fn split_horizontal(area: Rect, left_width: u16) -> [Rect; 2] {
    let left_width = left_width.min(area.width);
    [
        Rect::new(area.x, area.y, left_width, area.height),
        Rect::new(
            area.x.saturating_add(left_width),
            area.y,
            area.width.saturating_sub(left_width),
            area.height,
        ),
    ]
}

pub(super) fn split_vertical(area: Rect, heights: [u16; 3]) -> [Rect; 3] {
    let first = heights[0].min(area.height);
    let second = heights[1].min(area.height.saturating_sub(first));
    let third = heights[2].min(area.height.saturating_sub(first + second));
    [
        Rect::new(area.x, area.y, area.width, first),
        Rect::new(area.x, area.y.saturating_add(first), area.width, second),
        Rect::new(
            area.x,
            area.y.saturating_add(first + second),
            area.width,
            third,
        ),
    ]
}

pub(super) fn buffer_to_string(surface: &Buffer, width: u16, height: u16) -> String {
    let mut output = String::new();
    for y in 0..height {
        let mut line = String::new();
        for x in 0..width {
            line.push_str(surface[(x, y)].symbol());
        }
        output.push_str(line.trim_end());
        output.push('\n');
    }
    output
}
