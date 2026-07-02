//! Divider widget — horizontal or vertical line separator.

use helix_view::graphics::{Rect, Style};

/// Render a horizontal divider ("─") across the top row of `area`.
pub fn hdivider(surface: &mut crate::render::CellSurface, area: Rect, style: Style) {
    for x in area.left()..area.right() {
        {
            if let Some(cell) = surface.cell_mut((x, area.y)) {
                cell.set_symbol("─");
                cell.set_style(tui::ratatui::to_ratatui_style(style));
            }
        };
    }
}

/// Render a vertical divider ("│") down the left column of `area`.
pub fn vdivider(surface: &mut crate::render::CellSurface, area: Rect, style: Style) {
    for y in area.top()..area.bottom() {
        {
            if let Some(cell) = surface.cell_mut((area.x, y)) {
                cell.set_symbol("│");
                cell.set_style(tui::ratatui::to_ratatui_style(style));
            }
        };
    }
}

/// Render a one-cell border around `area`.
pub fn border(surface: &mut crate::render::CellSurface, area: Rect, style: Style, rounded: bool) {
    if area.width < 2 || area.height < 2 {
        return;
    }

    let (horizontal, vertical, top_left, top_right, bottom_left, bottom_right) = if rounded {
        ("─", "│", "╭", "╮", "╰", "╯")
    } else {
        ("─", "│", "┌", "┐", "└", "┘")
    };

    let left = area.left();
    let right = area.right() - 1;
    let top = area.top();
    let bottom = area.bottom() - 1;

    for x in left..=right {
        {
            if let Some(cell) = surface.cell_mut((x, top)) {
                cell.set_symbol(horizontal);
                cell.set_style(tui::ratatui::to_ratatui_style(style));
            }
        };
        {
            if let Some(cell) = surface.cell_mut((x, bottom)) {
                cell.set_symbol(horizontal);
                cell.set_style(tui::ratatui::to_ratatui_style(style));
            }
        };
    }
    for y in top..=bottom {
        {
            if let Some(cell) = surface.cell_mut((left, y)) {
                cell.set_symbol(vertical);
                cell.set_style(tui::ratatui::to_ratatui_style(style));
            }
        };
        {
            if let Some(cell) = surface.cell_mut((right, y)) {
                cell.set_symbol(vertical);
                cell.set_style(tui::ratatui::to_ratatui_style(style));
            }
        };
    }

    {
        if let Some(cell) = surface.cell_mut((left, top)) {
            cell.set_symbol(top_left);
            cell.set_style(tui::ratatui::to_ratatui_style(style));
        }
    };
    {
        if let Some(cell) = surface.cell_mut((right, top)) {
            cell.set_symbol(top_right);
            cell.set_style(tui::ratatui::to_ratatui_style(style));
        }
    };
    {
        if let Some(cell) = surface.cell_mut((left, bottom)) {
            cell.set_symbol(bottom_left);
            cell.set_style(tui::ratatui::to_ratatui_style(style));
        }
    };
    {
        if let Some(cell) = surface.cell_mut((right, bottom)) {
            cell.set_symbol(bottom_right);
            cell.set_style(tui::ratatui::to_ratatui_style(style));
        }
    };
}
