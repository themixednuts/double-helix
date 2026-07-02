//! Runtime picker table rendering.
//!
//! The picker controller builds rows and headers; this widget owns the terminal
//! rendering details for column sizing, selection symbols, and truncation.

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};
use tui::{
    ratatui::layout::{Constraint, Direction as TuiDirection, Layout as TuiLayout},
    text::{Span, Spans},
};

use crate::ui::text_layout::{self, TruncateAt};

use super::{TableCell, TableRow};

const PICKER_COLUMN_SPACING: u16 = 1;

fn picker_column_widths(
    widths: &[Constraint],
    max_width: u16,
    highlight_symbol_width: u16,
) -> Vec<u16> {
    let has_selection = highlight_symbol_width > 0;
    let mut constraints = Vec::with_capacity(widths.len() * 2 + usize::from(has_selection));
    if has_selection {
        constraints.push(Constraint::Length(highlight_symbol_width));
    }
    for constraint in widths {
        constraints.push(*constraint);
        constraints.push(Constraint::Length(PICKER_COLUMN_SPACING));
    }
    if !widths.is_empty() {
        constraints.pop();
    }

    let chunks = TuiLayout::default()
        .direction(TuiDirection::Horizontal)
        .constraints(constraints)
        .split(tui::ratatui::to_ratatui_rect(Rect {
            x: 0,
            y: 0,
            width: max_width,
            height: 1,
        }));

    chunks
        .iter()
        .skip(usize::from(has_selection))
        .step_by(2)
        .map(|chunk| chunk.width)
        .collect()
}

fn set_spans_truncated_start(
    surface: &mut crate::render::CellSurface,
    x: u16,
    y: u16,
    spans: &Spans<'_>,
    width: u16,
    ellipsis_style: Style,
) -> (u16, u16) {
    let surface_area = tui::ratatui::to_helix_rect(*surface.area());
    if width == 0 || !surface_area.contains(x, y) {
        return (x, y);
    }

    let available = width.min(surface_area.right().saturating_sub(x));
    if available == 0 {
        return (x, y);
    }

    if spans.width() <= available as usize {
        return surface.set_line(x, y, &tui::ratatui::to_ratatui_line(spans), available);
    }

    let plain = spans
        .0
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let truncated = text_layout::truncate(&plain, available as usize, TruncateAt::Start);
    let mut styled = Vec::new();
    if let Some(rest) = truncated.strip_prefix('…') {
        styled.push(Span::styled("…".to_string(), ellipsis_style));
        let mut remaining = rest;
        for span in spans.0.iter().rev() {
            let content = span.content.as_ref();
            if remaining.ends_with(content) {
                styled.push(Span::styled(content.to_string(), span.style));
                remaining = &remaining[..remaining.len().saturating_sub(content.len())];
                if remaining.is_empty() {
                    break;
                }
            } else if let Some(start) = content.rfind(remaining) {
                styled.push(Span::styled(content[start..].to_string(), span.style));
                break;
            }
        }
        styled[1..].reverse();
    } else {
        styled.push(Span::styled(truncated, ellipsis_style));
    }
    surface.set_line(
        x,
        y,
        &tui::ratatui::to_ratatui_line(&Spans::from(styled)),
        available,
    )
}

fn render_picker_cell(
    surface: &mut crate::render::CellSurface,
    cell: &TableCell<'_>,
    area: Rect,
    truncate_start: bool,
    ellipsis_style: Style,
) {
    for (i, spans) in cell.content.lines.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let y = area.y + i as u16;
        if truncate_start {
            set_spans_truncated_start(surface, area.x, y, spans, area.width, ellipsis_style);
        } else {
            surface.set_line(area.x, y, &tui::ratatui::to_ratatui_line(spans), area.width);
        }
    }
}

pub struct PickerTable<'a> {
    pub rows: Vec<TableRow<'a>>,
    pub header: Option<TableRow<'a>>,
    pub widths: &'a [Constraint],
    pub text_style: Style,
    pub placeholder_style: Style,
    pub selected_style: Style,
    pub header_style: Style,
    pub highlight_symbol: &'a str,
    pub selected_row: Option<usize>,
    pub truncate_start: bool,
}

impl PickerTable<'_> {
    pub fn render(self, area: Rect, surface: &mut crate::render::CellSurface) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        surface.set_style(
            tui::ratatui::to_ratatui_rect(area),
            tui::ratatui::to_ratatui_style(self.text_style),
        );
        let highlight_symbol_width = self
            .selected_row
            .map(|_| self.highlight_symbol.width() as u16)
            .unwrap_or(0);
        let columns_widths = picker_column_widths(self.widths, area.width, highlight_symbol_width);
        let blank_symbol = " ".repeat(highlight_symbol_width as usize);
        let mut current_height = 0;

        if let Some(header) = self.header {
            let header_height = area.height.min(1);
            let header_area = Rect {
                x: area.left(),
                y: area.top(),
                width: area.width,
                height: header_height,
            };
            surface.set_style(
                tui::ratatui::to_ratatui_rect(header_area),
                tui::ratatui::to_ratatui_style(self.header_style),
            );

            let mut col = area
                .left()
                .saturating_add(highlight_symbol_width.min(area.width));
            for (width, cell) in columns_widths.iter().zip(header.cells.iter()) {
                render_picker_cell(
                    surface,
                    cell,
                    Rect {
                        x: col,
                        y: area.top(),
                        width: *width,
                        height: header_height,
                    },
                    self.truncate_start,
                    self.header_style,
                );
                col = col.saturating_add(*width + PICKER_COLUMN_SPACING);
            }
            current_height += header_height;
        }

        if current_height >= area.height {
            return;
        }

        if self.rows.is_empty() {
            let y = area.top() + current_height;
            let text = "No results";
            let text_width = text.width() as u16;
            let x = area
                .left()
                .saturating_add(area.width.saturating_sub(text_width) / 2);
            surface.set_stringn(
                x,
                y,
                text,
                area.width.saturating_sub(x.saturating_sub(area.left())) as usize,
                tui::ratatui::to_ratatui_style(self.placeholder_style),
            );
            return;
        }

        for (i, row) in self
            .rows
            .into_iter()
            .take(area.height.saturating_sub(current_height) as usize)
            .enumerate()
        {
            let row_y = area.top() + current_height + i as u16;
            let row_area = Rect {
                x: area.left(),
                y: row_y,
                width: area.width,
                height: 1,
            };
            let is_selected = self.selected_row.is_some_and(|selected| selected == i);
            let row_style = if is_selected {
                self.selected_style
            } else {
                self.text_style
            };
            surface.set_style(
                tui::ratatui::to_ratatui_rect(row_area),
                tui::ratatui::to_ratatui_style(row_style),
            );

            let mut col = area.left();
            if self.selected_row.is_some() {
                let symbol = if is_selected {
                    self.highlight_symbol
                } else {
                    blank_symbol.as_str()
                };
                surface.set_stringn(
                    col,
                    row_y,
                    symbol,
                    area.width as usize,
                    tui::ratatui::to_ratatui_style(row_style),
                );
                col = col.saturating_add(highlight_symbol_width.min(area.width));
            }

            for (width, cell) in columns_widths.iter().zip(row.cells.iter()) {
                if is_selected {
                    let mut cell = cell.clone();
                    cell.set_style(self.selected_style);
                    render_picker_cell(
                        surface,
                        &cell,
                        Rect {
                            x: col,
                            y: row_y,
                            width: *width,
                            height: 1,
                        },
                        self.truncate_start,
                        row_style,
                    );
                } else {
                    render_picker_cell(
                        surface,
                        cell,
                        Rect {
                            x: col,
                            y: row_y,
                            width: *width,
                            height: 1,
                        },
                        self.truncate_start,
                        row_style,
                    );
                }
                col = col.saturating_add(*width + PICKER_COLUMN_SPACING);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::ratatui::{buffer::Buffer, layout::Rect as RatatuiRect};

    #[test]
    fn selected_row_uses_picker_symbol() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 20, 3));
        let widths = [Constraint::Length(18)];

        PickerTable {
            rows: vec![TableRow::new(["first"]), TableRow::new(["second"])],
            header: None,
            widths: &widths,
            text_style: Style::default(),
            placeholder_style: Style::default(),
            selected_style: Style::default(),
            header_style: Style::default(),
            highlight_symbol: "> ",
            selected_row: Some(1),
            truncate_start: false,
        }
        .render(Rect::new(0, 0, 20, 3), &mut surface);

        assert_eq!(surface[(0, 1)].symbol(), ">");
        assert_eq!(surface[(2, 1)].symbol(), "s");
    }

    #[test]
    fn empty_table_renders_placeholder() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 20, 3));
        let widths = [Constraint::Length(18)];

        PickerTable {
            rows: Vec::new(),
            header: None,
            widths: &widths,
            text_style: Style::default(),
            placeholder_style: Style::default(),
            selected_style: Style::default(),
            header_style: Style::default(),
            highlight_symbol: "> ",
            selected_row: None,
            truncate_start: false,
        }
        .render(Rect::new(0, 0, 20, 3), &mut surface);

        assert_eq!(surface[(5, 0)].symbol(), "N");
    }

    #[test]
    fn start_truncation_matches_text_layout_for_cjk() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 8, 1));
        let widths = [Constraint::Length(8)];

        PickerTable {
            rows: vec![TableRow::new(["src/界面/文件.rs"])],
            header: None,
            widths: &widths,
            text_style: Style::default(),
            placeholder_style: Style::default(),
            selected_style: Style::default(),
            header_style: Style::default(),
            highlight_symbol: "",
            selected_row: None,
            truncate_start: true,
        }
        .render(Rect::new(0, 0, 8, 1), &mut surface);

        let expected = text_layout::truncate("src/界面/文件.rs", 8, TruncateAt::Start);
        let mut rendered = String::new();
        let mut previous_wide = false;
        for x in 0..8 {
            let symbol = surface[(x, 0)].symbol();
            if symbol == " " && previous_wide {
                previous_wide = false;
                continue;
            }
            previous_wide = symbol.width() > 1;
            rendered.push_str(symbol);
        }
        let rendered = rendered.trim_end().to_string();
        assert_eq!(rendered, expected);
    }
}
