use helix_plugin::types::{SurfaceRenderOp, SurfaceRenderOps};

pub(crate) fn apply_plugin_render_ops(
    surface: &mut crate::render::CellSurface,
    ops: SurfaceRenderOps,
) {
    for op in ops {
        match op {
            SurfaceRenderOp::SetString { x, y, text, style } => {
                surface.set_string(x, y, text, tui::ratatui::to_ratatui_style(style));
            }
            SurfaceRenderOp::SetStringN {
                x,
                y,
                text,
                max_width,
                style,
            } => {
                surface.set_stringn(x, y, text, max_width, tui::ratatui::to_ratatui_style(style));
            }
            SurfaceRenderOp::Clear { area, style } => {
                let area = tui::ratatui::to_ratatui_rect(area);
                tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
                surface.set_style(area, tui::ratatui::to_ratatui_style(style));
            }
            SurfaceRenderOp::SetStyle { area, style } => {
                surface.set_style(
                    tui::ratatui::to_ratatui_rect(area),
                    tui::ratatui::to_ratatui_style(style),
                );
            }
            SurfaceRenderOp::Header { area, title, style } => {
                crate::widgets::header(surface, area, &title, style);
            }
            SurfaceRenderOp::HeaderWithCounts {
                area,
                title,
                current,
                total,
                style,
            } => {
                crate::widgets::header_with_counts(surface, area, &title, current, total, style);
            }
            SurfaceRenderOp::HDivider { area, style } => {
                crate::widgets::hdivider(surface, area, style);
            }
            SurfaceRenderOp::VDivider { area, style } => {
                crate::widgets::vdivider(surface, area, style);
            }
            SurfaceRenderOp::TextInput {
                area,
                text,
                cursor,
                style,
                cursor_style,
            } => {
                crate::widgets::text_input(surface, area, &text, cursor, style, cursor_style);
            }
            SurfaceRenderOp::Scrollbar {
                area,
                total,
                offset,
                visible,
                thumb_style,
                track_symbol,
                track_style,
            } => {
                let scrollbar =
                    crate::widgets::Scrollbar::new(total, offset, visible).thumb_style(thumb_style);
                let scrollbar = match track_symbol {
                    Some(symbol) => scrollbar.track(symbol, track_style),
                    None => scrollbar.track(" ", track_style),
                };
                scrollbar.render(area, surface);
            }
        }
    }
}
