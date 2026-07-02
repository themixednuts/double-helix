//! Shared panel chrome.
//!
//! A panel is just chrome around caller-owned content. Stateful components
//! compose this widget with headers, lists, inputs, and other widgets instead of
//! hand-rendering separate panel shapes.

use helix_view::graphics::{Rect, Style};
use tui::ratatui::widgets::Widget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelEdge {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelVariant {
    Surface,
    Framed { rounded: bool },
    Edge { edge: PanelEdge },
}

/// Resolved styles for a framed panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PanelStyle {
    /// Fill style for the full panel area.
    pub background: Style,
    /// Border style.
    pub border: Style,
    /// Optional title style.
    pub title: Style,
}

impl PanelStyle {
    pub fn new(background: Style, border: Style, title: Style) -> Self {
        Self {
            background,
            border,
            title,
        }
    }

    pub fn plain(background: Style) -> Self {
        Self::new(background, background, background)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panel<'a> {
    variant: PanelVariant,
    style: PanelStyle,
    title: Option<&'a str>,
}

impl<'a> Panel<'a> {
    pub const fn new(variant: PanelVariant, style: PanelStyle) -> Self {
        Self {
            variant,
            style,
            title: None,
        }
    }

    pub const fn surface(style: PanelStyle) -> Self {
        Self::new(PanelVariant::Surface, style)
    }

    pub const fn framed(style: PanelStyle, rounded: bool) -> Self {
        Self::new(PanelVariant::Framed { rounded }, style)
    }

    pub const fn edge(style: PanelStyle, edge: PanelEdge) -> Self {
        Self::new(PanelVariant::Edge { edge }, style)
    }

    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    pub fn render(self, surface: &mut crate::render::CellSurface, area: Rect) -> Rect {
        fill(surface, area, self.style.background);

        let inner = self.content_area(area);
        match self.variant {
            PanelVariant::Surface => {}
            PanelVariant::Framed { rounded } => {
                super::border(surface, area, self.style.border, rounded);
            }
            PanelVariant::Edge {
                edge: PanelEdge::Left,
            } => {
                super::vdivider(
                    surface,
                    Rect::new(area.x, area.y, 1, area.height),
                    self.style.border,
                );
            }
            PanelVariant::Edge {
                edge: PanelEdge::Right,
            } => {
                let divider = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
                super::vdivider(surface, divider, self.style.border);
            }
        }

        if let Some(title) = self.title {
            self.render_title(surface, area, inner, title);
        }

        inner
    }

    pub fn content_area(self, area: Rect) -> Rect {
        match self.variant {
            PanelVariant::Surface => area,
            PanelVariant::Framed { .. } => inset(area, 1, 1),
            PanelVariant::Edge {
                edge: PanelEdge::Left,
            } => area.clip_left(1),
            PanelVariant::Edge {
                edge: PanelEdge::Right,
            } => area.clip_right(1),
        }
    }

    fn render_title(
        self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        inner: Rect,
        title: &str,
    ) {
        if area.height == 0 {
            return;
        }

        let (x, width) = match self.variant {
            PanelVariant::Framed { .. } => (area.x.saturating_add(2), area.width.saturating_sub(4)),
            PanelVariant::Surface | PanelVariant::Edge { .. } => (inner.x, inner.width),
        };

        if width == 0 {
            return;
        }

        surface.set_stringn(
            x,
            area.y,
            title,
            width as usize,
            tui::ratatui::to_ratatui_style(self.style.title),
        );
    }
}

pub fn inset(area: Rect, x: u16, y: u16) -> Rect {
    let x = x.min(area.width);
    let y = y.min(area.height);
    Rect::new(
        area.x.saturating_add(x),
        area.y.saturating_add(y),
        area.width.saturating_sub(x.saturating_mul(2)),
        area.height.saturating_sub(y.saturating_mul(2)),
    )
}

fn fill(surface: &mut crate::render::CellSurface, area: Rect, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let area = tui::ratatui::to_ratatui_rect(area);
    tui::ratatui::widgets::Clear.render(area, surface);
    surface.set_style(area, tui::ratatui::to_ratatui_style(style));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::ratatui::{buffer::Buffer, layout::Rect as RatatuiRect};

    #[test]
    fn panel_renders_title_and_returns_inner_area() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 12, 5));

        let inner = Panel::framed(PanelStyle::default(), true)
            .title("title")
            .render(&mut surface, Rect::new(0, 0, 12, 5));

        assert_eq!(inner, Rect::new(1, 1, 10, 3));
        assert_eq!(surface[(0, 0)].symbol(), "╭");
        assert_eq!(surface[(2, 0)].symbol(), "t");
    }

    #[test]
    fn edge_panel_draws_one_sided_chrome() {
        let mut surface = Buffer::empty(RatatuiRect::new(0, 0, 8, 3));

        let inner = Panel::edge(PanelStyle::default(), PanelEdge::Right)
            .render(&mut surface, Rect::new(0, 0, 8, 3));

        assert_eq!(inner, Rect::new(0, 0, 7, 3));
        assert_eq!(surface[(7, 0)].symbol(), "│");
        assert_eq!(surface[(0, 0)].symbol(), " ");
    }
}
