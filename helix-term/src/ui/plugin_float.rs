//! Plugin/model floats rendered from the frontend-agnostic UI model.

use crate::compositor::RenderContext;
use crate::ui::plugin_panel::TermDrawSurface;
use helix_core::Position;
use helix_plugin::mlua;
use helix_view::graphics::{Margin, Rect, Style};
use helix_view::layout::{anchor_near, center};
use helix_view::model::{
    DocumentFloatModel, FloatEntry, Placement, PluginFloatModel, RenderBlock, TextFloatModel,
};
use helix_view::Editor;
use tui::buffer::Buffer as Surface;
use tui::text::{Span, Spans, Text};
use tui::widgets::{Block, BorderType, Paragraph, Widget};

pub(crate) fn render_model_floats(viewport: Rect, surface: &mut Surface, cx: &RenderContext) {
    for (_, entry) in &cx.editor.model.floats {
        let Some(area) = resolve_float_area(cx.editor, viewport, &entry.placement) else {
            continue;
        };
        if area.width == 0 || area.height == 0 {
            continue;
        }

        render_float_entry(entry, area, surface, cx);
    }
}

fn render_float_entry(entry: &FloatEntry, area: Rect, surface: &mut Surface, cx: &RenderContext) {
    if let Some(model) = entry.content.downcast_ref::<TextFloatModel>() {
        render_text_float(entry.title.as_deref(), model, area, surface, cx.editor);
    } else if let Some(model) = entry.content.downcast_ref::<DocumentFloatModel>() {
        render_document_float(entry.title.as_deref(), model, area, surface, cx.editor);
    } else if let Some(model) = entry.content.downcast_ref::<PluginFloatModel>() {
        render_plugin_float(entry.title.as_deref(), model, area, surface, cx);
    }
}

fn render_text_float(
    title: Option<&str>,
    model: &TextFloatModel,
    area: Rect,
    surface: &mut Surface,
    editor: &Editor,
) {
    let inner = render_float_frame(title, area, surface, editor);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let text_style = editor.theme.get("ui.text");
    let lines = model
        .blocks
        .iter()
        .flat_map(|block| block_to_spans(block, text_style))
        .collect::<Vec<_>>();
    let text = Text::from(lines);
    Paragraph::new(&text)
        .style(text_style)
        .render(inner, surface);
}

fn render_document_float(
    title: Option<&str>,
    model: &DocumentFloatModel,
    area: Rect,
    surface: &mut Surface,
    editor: &Editor,
) {
    let inner = render_float_frame(title, area, surface, editor);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let text_style = editor.theme.get("ui.text");
    let Some(doc) = editor.documents.get(&model.document) else {
        surface.set_stringn(
            inner.x,
            inner.y,
            "Document unavailable",
            inner.width as usize,
            editor.theme.get("error"),
        );
        return;
    };

    let text = doc.text();
    let lines = (0..text.len_lines())
        .take(inner.height as usize)
        .map(|line| {
            let text = text.line(line).to_string();
            let text = text.trim_end_matches(&['\r', '\n'][..]);
            Spans::from(Span::styled(text.to_string(), text_style))
        })
        .collect::<Vec<_>>();
    let text = Text::from(lines);
    Paragraph::new(&text)
        .style(text_style)
        .render(inner, surface);
}

fn render_plugin_float(
    title: Option<&str>,
    model: &PluginFloatModel,
    area: Rect,
    surface: &mut Surface,
    cx: &RenderContext,
) {
    let inner = render_float_frame(title, area, surface, cx.editor);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(ref pm) = cx.plugin_manager else {
        return;
    };

    let engine = pm.engine();
    let engine = engine.read();
    let lua = engine.lua();

    let ui_callbacks = engine.ui_callbacks();
    let ui_callbacks = ui_callbacks.read();
    let Some(callback_id) = helix_plugin::types::UiCallbackId::new(model.render_callback_id) else {
        render_plugin_error(inner, surface, cx.editor);
        return;
    };
    let key = helix_plugin::types::PluginCallbackKey::new(model.plugin_name.clone(), callback_id);
    let Some(callback_ref) = ui_callbacks.get(&key) else {
        render_plugin_error(inner, surface, cx.editor);
        return;
    };

    let Ok(callback) = lua.registry_value::<mlua::Function>(callback_ref) else {
        return;
    };
    let Ok(area_table) = helix_plugin::lua::api::facade::rect_to_table(lua, inner) else {
        return;
    };
    let Ok(lua_surface) = lua.create_userdata(helix_plugin::lua::api::facade::LuaSurface) else {
        return;
    };

    // The render context stores the concrete surface in thread-local state for
    // LuaSurface methods. Rendering is single-threaded for plugin callbacks.
    let theme_ptr = &cx.editor.theme as *const helix_view::Theme;
    // Safety: the theme is borrowed from the immutable editor for this render
    // frame, and plugin callback rendering is executed synchronously.
    let theme = unsafe { &*theme_ptr };
    let mut wrapper = TermDrawSurface { surface };
    helix_plugin::lua::with_render_context(&mut wrapper, theme, || {
        helix_plugin::lua::with_editor_context_ref(cx.editor, || {
            if let Err(err) =
                helix_plugin::lua::with_current_plugin_name(lua, &model.plugin_name, || {
                    callback.call::<()>((area_table, lua_surface))
                })
            {
                log::error!(
                    "Plugin float render error ({}:{}): {}",
                    model.plugin_name,
                    model.render_callback_id,
                    err
                );
            }
        });
    });
}

fn render_float_frame(
    title: Option<&str>,
    area: Rect,
    surface: &mut Surface,
    editor: &Editor,
) -> Rect {
    let popup_style = editor.theme.get("ui.popup");
    surface.clear_with(area, popup_style);

    let border_type = BorderType::new(editor.config().rounded_corners);
    let mut block = Block::bordered()
        .border_style(popup_style)
        .border_type(border_type);
    if let Some(title) = title {
        block = block.title(title);
    }

    let inner = block.inner(area).inner(Margin::horizontal(1));
    block.render(area, surface);
    inner
}

fn render_plugin_error(area: Rect, surface: &mut Surface, editor: &Editor) {
    surface.set_stringn(
        area.x,
        area.y,
        "Plugin render error",
        area.width as usize,
        editor.theme.get("error"),
    );
}

fn block_to_spans(block: &RenderBlock, fallback_style: Style) -> impl Iterator<Item = Spans<'_>> {
    let style = block.style.unwrap_or(fallback_style);
    block
        .text
        .lines()
        .map(move |line| Spans::from(Span::styled(line, style)))
}

pub(crate) fn resolve_float_area(
    editor: &Editor,
    viewport: Rect,
    placement: &Placement,
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
            let view = editor.tree.try_get(view)?;
            let doc = editor.documents.get(&view.doc)?;
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
    use std::sync::Arc;

    fn test_editor(width: u16, height: u16) -> Editor {
        let theme_loader = helix_view::theme::Loader::new(helix_loader::runtime_dirs());
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
        let area = resolve_float_area(
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
        let area = resolve_float_area(
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

        let area = resolve_float_area(
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
        let mut surface = Surface::empty(Rect::new(0, 0, 40, 10));
        let model = TextFloatModel {
            blocks: vec![RenderBlock {
                text: "hello".into(),
                style: None,
            }],
        };

        render_text_float(
            Some("Note"),
            &model,
            Rect::new(2, 2, 20, 5),
            &mut surface,
            &editor,
        );

        assert_eq!(surface[(4, 3)].symbol.as_ref(), "h");
        assert_eq!(surface[(5, 3)].symbol.as_ref(), "e");
        assert_eq!(surface[(6, 3)].symbol.as_ref(), "l");
    }

    #[tokio::test]
    async fn document_float_renderer_draws_document_text_inside_frame() {
        let mut editor = test_editor(40, 10);
        let doc = editor.open_markdown_scratch(
            helix_view::editor::Action::VerticalSplit,
            "alpha\nbeta".into(),
        );
        let mut surface = Surface::empty(Rect::new(0, 0, 40, 10));
        let model = DocumentFloatModel { document: doc };

        render_document_float(
            Some("Doc"),
            &model,
            Rect::new(2, 2, 20, 5),
            &mut surface,
            &editor,
        );

        assert_eq!(surface[(4, 3)].symbol.as_ref(), "a");
        assert_eq!(surface[(5, 3)].symbol.as_ref(), "l");
        assert_eq!(surface[(4, 4)].symbol.as_ref(), "b");
    }
}
