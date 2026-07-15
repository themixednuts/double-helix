//! Plugin/model floats rendered from the frontend-agnostic UI model.

use crate::compositor::RenderContext;
use helix_core::Position;
use helix_view::graphics::{Rect, Style};
use helix_view::layout::{anchor_near, center};
use helix_view::model::{DocumentFloatModel, Placement, RenderBlock, TextFloatModel};
use std::sync::Arc;
use tui::text::{Span, Spans};

enum FloatRenderContent {
    Text(TextFloatModel),
    Document(Option<helix_core::Rope>),
}

struct FloatRenderEntry {
    title: Option<Arc<str>>,
    area: Rect,
    content: FloatRenderContent,
}

pub(crate) struct ModelFloatsRenderSnapshot {
    entries: Box<[FloatRenderEntry]>,
    popup_style: Style,
    text_style: Style,
    error_style: Style,
    rounded_corners: bool,
}

impl ModelFloatsRenderSnapshot {
    pub(crate) fn collect(viewport: Rect, cx: &RenderContext) -> Self {
        let entries = cx
            .model_float_entries()
            .filter_map(|entry| {
                let area = resolve_float_area(cx, viewport, &entry.placement)?;
                if area.width == 0 || area.height == 0 {
                    return None;
                }
                let content = if let Some(model) = entry.content.downcast_ref::<TextFloatModel>() {
                    FloatRenderContent::Text(model.clone())
                } else if let Some(model) = entry.content.downcast_ref::<DocumentFloatModel>() {
                    FloatRenderContent::Document(
                        cx.document(model.document)
                            .map(|document| document.text().clone()),
                    )
                } else {
                    return None;
                };
                Some(FloatRenderEntry {
                    title: entry.title.as_deref().map(Arc::from),
                    area,
                    content,
                })
            })
            .collect();
        Self {
            entries,
            popup_style: cx.style("ui.popup"),
            text_style: cx.style("ui.text"),
            error_style: cx.style("error"),
            rounded_corners: cx.config().rounded_corners,
        }
    }

    pub(crate) fn paint(self, surface: &mut crate::render::CellSurface) {
        for entry in self.entries {
            let inner = render_float_frame(
                entry.title.as_deref(),
                entry.area,
                surface,
                self.popup_style,
                self.rounded_corners,
            );
            if inner.width == 0 || inner.height == 0 {
                continue;
            }
            match entry.content {
                FloatRenderContent::Text(model) => {
                    render_text_float_content(&model, inner, surface, self.text_style)
                }
                FloatRenderContent::Document(Some(text)) => {
                    render_rope_float_content(&text, inner, surface, self.text_style)
                }
                FloatRenderContent::Document(None) => {
                    surface.set_stringn(
                        inner.x,
                        inner.y,
                        "Document unavailable",
                        inner.width as usize,
                        tui::ratatui::to_ratatui_style(self.error_style),
                    );
                }
            }
        }
    }
}

fn render_text_float_content(
    model: &TextFloatModel,
    inner: Rect,
    surface: &mut crate::render::CellSurface,
    text_style: Style,
) {
    let lines = model
        .blocks
        .iter()
        .flat_map(|block| block_to_spans(block, text_style))
        .collect::<Vec<_>>();
    render_spans_lines(surface, inner, &lines);
}

#[cfg(test)]
fn render_document_float_content(
    doc: &helix_view::Document,
    inner: Rect,
    surface: &mut crate::render::CellSurface,
    text_style: Style,
) {
    render_rope_float_content(doc.text(), inner, surface, text_style);
}

fn render_rope_float_content(
    text: &helix_core::Rope,
    inner: Rect,
    surface: &mut crate::render::CellSurface,
    text_style: Style,
) {
    let lines = (0..text.len_lines())
        .take(inner.height as usize)
        .map(|line| {
            let text = text.line(line).to_string();
            let text = text.trim_end_matches(&['\r', '\n'][..]);
            Spans::from(Span::styled(text.to_string(), text_style))
        })
        .collect::<Vec<_>>();
    render_spans_lines(surface, inner, &lines);
}

fn render_float_frame(
    title: Option<&str>,
    area: Rect,
    surface: &mut crate::render::CellSurface,
    popup_style: Style,
    rounded_corners: bool,
) -> Rect {
    let style = crate::widgets::PanelStyle::plain(popup_style);
    let inner = match title {
        Some(title) => crate::widgets::Panel::framed(style, rounded_corners)
            .title(title)
            .render(surface, area),
        None => crate::widgets::Panel::framed(style, rounded_corners).render(surface, area),
    };
    Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    )
}

fn render_spans_lines(surface: &mut crate::render::CellSurface, area: Rect, lines: &[Spans<'_>]) {
    for (row, line) in lines.iter().take(area.height as usize).enumerate() {
        surface.set_line(
            area.x,
            area.y + row as u16,
            &tui::ratatui::to_ratatui_line(line),
            area.width,
        );
    }
}

fn block_to_spans(block: &RenderBlock, fallback_style: Style) -> impl Iterator<Item = Spans<'_>> {
    let style = block.style.unwrap_or(fallback_style);
    block
        .text
        .lines()
        .map(move |line| Spans::from(Span::styled(line, style)))
}

fn resolve_float_area(cx: &RenderContext, viewport: Rect, placement: &Placement) -> Option<Rect> {
    resolve_float_area_with(
        viewport,
        placement,
        |view_id| cx.view(view_id),
        |document_id| cx.document(document_id),
    )
}

fn resolve_float_area_with<'a>(
    viewport: Rect,
    placement: &Placement,
    mut view_for_id: impl FnMut(helix_view::ViewId) -> Option<&'a helix_view::View>,
    mut document_for_id: impl FnMut(helix_view::DocumentId) -> Option<&'a helix_view::Document>,
) -> Option<Rect> {
    match *placement {
        Placement::Fullscreen => Some(viewport),
        Placement::Centered { width, height } => Some(center(viewport, width, height)),
        Placement::Float {
            x,
            y,
            width,
            height,
        } => Some(viewport.intersection(Rect::new(x, y, width, height))),
        Placement::Anchored {
            view,
            anchor,
            width,
            height,
            bias,
        } => {
            let view = view_for_id(view)?;
            let doc = document_for_id(view.doc)?;
            let text = doc.text();
            let char_idx = position_to_char(text, anchor);
            let relative = view.screen_coords_at_pos(doc, text.slice(..), char_idx)?;
            let inner = view.inner_area(doc);
            let absolute = Position {
                row: relative.row + inner.y as usize,
                col: relative.col + inner.x as usize,
            };
            let viewport_relative = Position {
                row: absolute.row.saturating_sub(viewport.y as usize),
                col: absolute.col.saturating_sub(viewport.x as usize),
            };
            Some(anchor_near(
                viewport,
                viewport_relative,
                (width, height),
                bias,
            ))
        }
    }
}

#[cfg(test)]
fn resolve_float_area_for_editor(
    editor: &helix_view::Editor,
    viewport: Rect,
    placement: &Placement,
) -> Option<Rect> {
    resolve_float_area_with(
        viewport,
        placement,
        |view_id| editor.tree.try_get(view_id),
        |document_id| editor.documents.get(&document_id),
    )
}

fn position_to_char(text: &helix_core::Rope, position: Position) -> usize {
    let line = position.row.min(text.len_lines().saturating_sub(1));
    let line_start = text.line_to_char(line);
    let line_end = text.line_to_char((line + 1).min(text.len_lines()));
    let line_len = line_end.saturating_sub(line_start);
    line_start + position.col.min(line_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_view::Editor;
    use std::sync::Arc;
    use tui::ratatui::{buffer::Buffer as Surface, layout::Rect as SurfaceRect};

    fn test_editor(width: u16, height: u16) -> Editor {
        let theme_loader = helix_view::theme::Loader::new(&[]);
        let syn_loader = helix_core::config::default_lang_loader();
        let config = helix_view::editor::Config::default();
        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = helix_view::handlers::Handlers::dummy();
        let mut editor = Editor::new(
            Rect::new(0, 0, width, height),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(
                config,
                |c: &helix_view::editor::Config| c,
            )),
            helix_runtime::test::runtime(),
            handlers,
        );
        editor.new_file(helix_view::editor::Action::VerticalSplit);
        editor
    }

    #[tokio::test]
    async fn centered_float_area_is_clamped_and_centered() {
        let editor = test_editor(80, 24);
        let area = resolve_float_area_for_editor(
            &editor,
            Rect::new(0, 0, 40, 10),
            &Placement::Centered {
                width: 100,
                height: 4,
            },
        )
        .unwrap();

        assert_eq!(area, Rect::new(0, 3, 40, 4));
    }

    #[tokio::test]
    async fn absolute_float_area_clips_to_viewport() {
        let editor = test_editor(80, 24);
        let area = resolve_float_area_for_editor(
            &editor,
            Rect::new(0, 0, 40, 10),
            &Placement::Float {
                x: 35,
                y: 8,
                width: 20,
                height: 6,
            },
        )
        .unwrap();

        assert_eq!(area, Rect::new(35, 8, 5, 2));
    }

    #[tokio::test]
    async fn anchored_float_area_uses_view_coordinates() {
        let editor = test_editor(80, 24);
        let view_id = editor.tree.focus;
        let view = editor.tree.get(view_id);
        let doc = editor.documents.get(&view.doc).unwrap();
        let inner = view.inner_area(doc);

        let area = resolve_float_area_for_editor(
            &editor,
            Rect::new(0, 0, 80, 24),
            &Placement::Anchored {
                view: view_id,
                anchor: Position { row: 0, col: 0 },
                width: 10,
                height: 4,
                bias: helix_view::layout::AnchorBias::Below,
            },
        )
        .unwrap();

        assert_eq!(area.x, inner.x);
        assert_eq!(area.y, inner.y + 1);
        assert_eq!(area.width, 10);
        assert_eq!(area.height, 4);
    }

    #[tokio::test]
    async fn text_float_renderer_draws_content_inside_frame() {
        let editor = test_editor(40, 10);
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 40, 10));
        let model = TextFloatModel {
            blocks: vec![RenderBlock {
                text: "hello".into(),
                style: None,
            }]
            .into(),
        };

        let inner = render_float_frame(
            Some("Note"),
            Rect::new(2, 2, 20, 5),
            &mut surface,
            editor.theme.get("ui.popup"),
            editor.config().rounded_corners,
        );
        render_text_float_content(&model, inner, &mut surface, editor.theme.get("ui.text"));

        assert_eq!(surface[(4, 3)].symbol(), "h");
        assert_eq!(surface[(5, 3)].symbol(), "e");
        assert_eq!(surface[(6, 3)].symbol(), "l");
    }

    #[tokio::test]
    async fn text_float_renderer_draws_to_ratatui_surface() {
        let editor = test_editor(40, 10);
        let mut surface =
            crate::render::CellSurface::empty(tui::ratatui::layout::Rect::new(0, 0, 40, 10));
        let model = TextFloatModel {
            blocks: vec![RenderBlock {
                text: "hello".into(),
                style: None,
            }]
            .into(),
        };

        let inner = render_float_frame(
            Some("Note"),
            Rect::new(2, 2, 20, 5),
            &mut surface,
            editor.theme.get("ui.popup"),
            editor.config().rounded_corners,
        );
        render_text_float_content(&model, inner, &mut surface, editor.theme.get("ui.text"));

        assert_eq!(surface[(4, 3)].symbol(), "h");
        assert_eq!(surface[(5, 3)].symbol(), "e");
        assert_eq!(surface[(6, 3)].symbol(), "l");
    }

    #[tokio::test]
    async fn document_float_renderer_draws_document_text_inside_frame() {
        let mut editor = test_editor(40, 10);
        let doc = editor.open_markdown_scratch(
            helix_view::editor::Action::VerticalSplit,
            "alpha\nbeta".into(),
        );
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 40, 10));
        let model = DocumentFloatModel { document: doc };

        let inner = render_float_frame(
            Some("Doc"),
            Rect::new(2, 2, 20, 5),
            &mut surface,
            editor.theme.get("ui.popup"),
            editor.config().rounded_corners,
        );
        let doc = editor.document(model.document).unwrap();
        render_document_float_content(doc, inner, &mut surface, editor.theme.get("ui.text"));

        assert_eq!(surface[(4, 3)].symbol(), "a");
        assert_eq!(surface[(5, 3)].symbol(), "l");
        assert_eq!(surface[(4, 4)].symbol(), "b");
    }
}
