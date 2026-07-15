use crate::compositor::{Component, RenderContext};

use helix_view::graphics::Rect;
use std::sync::Arc;

pub struct Text {
    pub(crate) contents: Arc<tui::text::Text<'static>>,
    size: (u16, u16),
    viewport: (u16, u16),
}

impl Text {
    pub fn new(contents: String) -> Self {
        Self {
            contents: Arc::new(tui::text::Text::from(contents)),
            size: (0, 0),
            viewport: (0, 0),
        }
    }
}

impl From<tui::text::Text<'static>> for Text {
    fn from(contents: tui::text::Text<'static>) -> Self {
        Self {
            contents: Arc::new(contents),
            size: (0, 0),
            viewport: (0, 0),
        }
    }
}

impl Component for Text {
    fn prepare_render(&mut self, area: Rect, _cx: &RenderContext) -> crate::render::PreparedRender {
        let contents = Arc::clone(&self.contents);
        crate::render::PreparedRender::deferred(move |cancellation| {
            let mut output = crate::render::RenderOutput::sparse(area);
            if !cancellation.is_cancelled() {
                paint_text(output.surface_mut(), area, &contents);
            }
            output
        })
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        if viewport != self.viewport {
            let width = std::cmp::min(self.contents.width() as u16, viewport.0);
            let height = std::cmp::min(self.contents.height() as u16, viewport.1);
            self.size = (width, height);
            self.viewport = viewport;
        }
        Some(self.size)
    }
}

pub(crate) fn paint_text(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    contents: &tui::text::Text<'_>,
) {
    use tui::ratatui::widgets::{Paragraph, Widget, Wrap};

    let contents = tui::ratatui::to_ratatui_text(contents);
    Paragraph::new(contents)
        .wrap(Wrap { trim: false })
        .render(tui::ratatui::to_ratatui_rect(area), surface);
}

pub fn required_size(text: &tui::text::Text, max_text_width: u16) -> (u16, u16) {
    let mut text_width = 0;
    let mut height = 0;
    for content in &text.lines {
        height += 1;
        let content_width = content.width() as u16;
        if content_width > max_text_width {
            text_width = max_text_width;
            height += content_width.checked_div(max_text_width).unwrap_or(0);
        } else if content_width > text_width {
            text_width = content_width;
        }
    }
    (text_width, height)
}
