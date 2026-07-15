use helix_plugin_api::requests::{UiRect, UiRenderNode};
use helix_view::graphics::Rect;

pub(crate) fn render_retained_nodes(
    surface: &mut crate::render::CellSurface,
    container: Rect,
    nodes: &[UiRenderNode],
    style_for: impl Fn(&str) -> helix_view::graphics::Style,
) {
    for node in nodes {
        match node {
            UiRenderNode::Text {
                x,
                y,
                text,
                style,
                max_width,
            } => {
                if *x >= container.width || *y >= container.height {
                    continue;
                }
                let available = container.width.saturating_sub(*x) as usize;
                let width = max_width.map_or(available, |width| usize::from(width).min(available));
                if width == 0 {
                    continue;
                }
                surface.set_stringn(
                    container.x.saturating_add(*x),
                    container.y.saturating_add(*y),
                    text,
                    width,
                    tui::ratatui::to_ratatui_style(style_for(style)),
                );
            }
            UiRenderNode::Fill { area, style } => {
                let Some(area) = resolve_area(container, area) else {
                    continue;
                };
                let ratatui_area = tui::ratatui::to_ratatui_rect(area);
                tui::ratatui::widgets::Widget::render(
                    tui::ratatui::widgets::Clear,
                    ratatui_area,
                    surface,
                );
                surface.set_style(
                    ratatui_area,
                    tui::ratatui::to_ratatui_style(style_for(style)),
                );
            }
            UiRenderNode::Header {
                area,
                title,
                current,
                total,
                style,
            } => {
                let Some(area) = resolve_area(container, area) else {
                    continue;
                };
                let style = style_for(style);
                match (*current, *total) {
                    (Some(current), Some(total)) => crate::widgets::header_with_counts(
                        surface, area, title, current, total, style,
                    ),
                    _ => crate::widgets::header(surface, area, title, style),
                }
            }
            UiRenderNode::HorizontalDivider { area, style } => {
                if let Some(area) = resolve_area(container, area) {
                    crate::widgets::hdivider(surface, area, style_for(style));
                }
            }
            UiRenderNode::VerticalDivider { area, style } => {
                if let Some(area) = resolve_area(container, area) {
                    crate::widgets::vdivider(surface, area, style_for(style));
                }
            }
            UiRenderNode::TextInput {
                area,
                text,
                cursor,
                style,
                cursor_style,
            } => {
                if let Some(area) = resolve_area(container, area) {
                    crate::widgets::text_input(
                        surface,
                        area,
                        text,
                        *cursor,
                        style_for(style),
                        style_for(cursor_style),
                    );
                }
            }
            UiRenderNode::Scrollbar {
                area,
                total,
                offset,
                visible,
                thumb_style,
                track_symbol,
                track_style,
            } => {
                let Some(area) = resolve_area(container, area) else {
                    continue;
                };
                crate::widgets::Scrollbar::new(*total, *offset, *visible)
                    .thumb_style(style_for(thumb_style))
                    .track(
                        track_symbol.as_deref().unwrap_or(" "),
                        style_for(track_style),
                    )
                    .render(area, surface);
            }
        }
    }
}

fn resolve_area(container: Rect, relative: &UiRect) -> Option<Rect> {
    if relative.x >= container.width
        || relative.y >= container.height
        || relative.width == 0
        || relative.height == 0
    {
        return None;
    }
    Some(Rect::new(
        container.x.saturating_add(relative.x),
        container.y.saturating_add(relative.y),
        relative
            .width
            .min(container.width.saturating_sub(relative.x)),
        relative
            .height
            .min(container.height.saturating_sub(relative.y)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_plugin_api::requests::UiRenderNode;

    #[test]
    fn retained_rects_are_clipped_to_the_container() {
        let area = resolve_area(
            Rect::new(10, 5, 20, 8),
            &UiRect {
                x: 18,
                y: 6,
                width: 10,
                height: 10,
            },
        )
        .unwrap();
        assert_eq!(area, Rect::new(28, 11, 2, 2));
    }

    #[test]
    fn retained_rects_outside_the_container_are_ignored() {
        assert!(resolve_area(
            Rect::new(10, 5, 20, 8),
            &UiRect {
                x: 20,
                y: 0,
                width: 1,
                height: 1,
            },
        )
        .is_none());
    }

    #[test]
    fn text_constructor_uses_origin_defaults() {
        assert!(matches!(
            UiRenderNode::text("hello"),
            UiRenderNode::Text { x: 0, y: 0, .. }
        ));
    }
}
