use crate::{
    compositor::{Component, RenderContext},
    ui::design::{self, InfoPopupStyles},
    widgets::{Panel, PanelStyle},
};
use helix_view::graphics::{Margin, Rect};
use helix_view::info::Info;
use tui::ratatui::widgets::{Paragraph, Widget};

impl Component for Info {
    fn render(
        &mut self,
        viewport: Rect,
        surface: &mut crate::render::CellSurface,
        cx: &RenderContext,
    ) {
        let styles = design::InfoPopupStyles::from_theme(cx.theme());
        let rounded = cx.config().rounded_corners;

        render_info_popup(surface, viewport, self, styles, rounded);
    }
}

pub(crate) fn render_info_popup(
    surface: &mut crate::render::CellSurface,
    viewport: Rect,
    info: &Info,
    styles: InfoPopupStyles,
    rounded_corners: bool,
) -> Rect {
    let area = design::info_popup_area(viewport, info);
    if area.width == 0 || area.height == 0 {
        return area;
    }

    // Use the shared `Panel::framed` widget — same chrome as every
    // other framed surface in the editor (plugin floats, debug
    // overlays). It fills the background, draws the border (with
    // configurable rounded corners), and returns the inner content
    // area. Centralizing here means a future change to border
    // semantics (style key, glyph set, padding) propagates to every
    // popup that draws a frame.
    let panel_style = PanelStyle::new(styles.popup, styles.popup, styles.popup);
    let panel = Panel::framed(panel_style, rounded_corners).title(info.title.as_ref());
    let inner = panel.render(surface, area).inner(Margin::horizontal(1));

    let needs_scrollbar = info.height > inner.height;
    let body = if needs_scrollbar {
        inner.clip_right(1)
    } else {
        inner
    };
    let scroll = info.visible_scroll(body.height);

    Paragraph::new(info.text.as_str())
        .style(tui::ratatui::to_ratatui_style(styles.text))
        .scroll((scroll as u16, 0))
        .render(tui::ratatui::to_ratatui_rect(body), surface);

    if needs_scrollbar && inner.width > 0 {
        crate::widgets::Scrollbar::new(info.height as usize, scroll, inner.height as usize)
            .symbol("▌")
            .thumb_style(styles.scrollbar)
            .render(
                Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height),
                surface,
            );
    }

    area
}
