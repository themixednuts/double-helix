use crate::{
    compositor::{Component, RenderContext},
    ui::design::{self, InfoPopupStyles},
    widgets::{Panel, PanelStyle},
};
use helix_view::graphics::{Margin, Rect};
use helix_view::info::Info;
use std::sync::Arc;
use tui::ratatui::widgets::{Paragraph, Widget};

struct InfoRenderModel {
    title: Arc<str>,
    text: Arc<str>,
    width: u16,
    height: u16,
    scroll: usize,
    styles: InfoPopupStyles,
    rounded_corners: bool,
}

impl Component for Info {
    fn prepare_render(
        &mut self,
        viewport: Rect,
        cx: &RenderContext,
    ) -> crate::render::PreparedRender {
        let model = InfoRenderModel {
            title: Arc::from(self.title.as_ref()),
            text: Arc::from(self.text.as_str()),
            width: self.width,
            height: self.height,
            scroll: self.scroll,
            styles: design::InfoPopupStyles::from_theme(cx.theme()),
            rounded_corners: cx.config().rounded_corners,
        };
        crate::render::PreparedRender::deferred(move |cancellation| {
            let mut output = crate::render::RenderOutput::sparse(viewport);
            if !cancellation.is_cancelled() {
                paint_info_popup(output.surface_mut(), viewport, &model);
            }
            output
        })
    }
}

fn paint_info_popup(
    surface: &mut crate::render::CellSurface,
    viewport: Rect,
    model: &InfoRenderModel,
) -> Rect {
    let area = design::info_popup_area(viewport, model.width, model.height);
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
    let panel_style = PanelStyle::new(model.styles.popup, model.styles.popup, model.styles.popup);
    let panel = Panel::framed(panel_style, model.rounded_corners).title(model.title.as_ref());
    let inner = panel.render(surface, area).inner(Margin::horizontal(1));

    let needs_scrollbar = model.height > inner.height;
    let body = if needs_scrollbar {
        inner.clip_right(1)
    } else {
        inner
    };
    let scroll = model
        .scroll
        .min((model.height as usize).saturating_sub(body.height as usize));

    Paragraph::new(model.text.as_ref())
        .style(tui::ratatui::to_ratatui_style(model.styles.text))
        .scroll((scroll as u16, 0))
        .render(tui::ratatui::to_ratatui_rect(body), surface);

    if needs_scrollbar && inner.width > 0 {
        crate::widgets::Scrollbar::new(model.height as usize, scroll, inner.height as usize)
            .symbol("▌")
            .thumb_style(model.styles.scrollbar)
            .render(
                Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height),
                surface,
            );
    }

    area
}
