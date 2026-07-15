use std::io;

use helix_view::graphics::{CursorKind, Rect};
use tui::backend::Backend as HelixBackend;
use tui::ratatui::{
    self,
    backend::Backend as RatatuiBackend,
    buffer::Buffer,
    layout::{Position, Rect as RatatuiRect},
};

/// Application-facing terminal facade backed by Ratatui buffers.
///
/// Helix still owns backend lifecycle and terminal extensions through [`HelixBackend`], while
/// frame diffing and the render surface come from Ratatui. Keeping this facade narrow avoids
/// leaking backend lifecycle details through the rest of the application.
pub(super) struct AppTerminal<B>
where
    B: HelixBackend + RatatuiBackend<Error = io::Error>,
{
    inner: ratatui::Terminal<B>,
    viewport_area: Rect,
    cursor_kind: CursorKind,
}

impl<B> AppTerminal<B>
where
    B: HelixBackend + RatatuiBackend<Error = io::Error>,
{
    pub(super) fn new(backend: B) -> io::Result<Self> {
        let inner = ratatui::Terminal::new(backend)?;
        let viewport_area = rect_from_size(inner.size()?);
        Ok(Self {
            inner,
            viewport_area,
            cursor_kind: CursorKind::Block,
        })
    }

    pub(super) fn claim(&mut self) -> io::Result<()> {
        HelixBackend::claim(self.inner.backend_mut())?;
        let area = HelixBackend::size(self.inner.backend()).unwrap_or(self.viewport_area);
        if area != self.viewport_area {
            self.resize(area)?;
        }
        Ok(())
    }

    pub(super) fn reconfigure(&mut self, config: tui::terminal::Config) -> io::Result<()> {
        HelixBackend::reconfigure(self.inner.backend_mut(), config)
    }

    pub(super) fn restore(&mut self) -> io::Result<()> {
        HelixBackend::show_cursor(self.inner.backend_mut(), CursorKind::Block).ok();
        HelixBackend::restore(self.inner.backend_mut())
    }

    pub(super) fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    pub(super) fn size(&self) -> Rect {
        self.viewport_area
    }

    pub(super) fn resize(&mut self, area: Rect) -> io::Result<()> {
        self.inner.resize(tui::ratatui::to_ratatui_rect(area))?;
        self.viewport_area = area;
        Ok(())
    }

    pub(super) fn current_buffer_mut(&mut self) -> &mut Buffer {
        self.inner.current_buffer_mut()
    }

    pub(super) fn backend(&self) -> &B {
        self.inner.backend()
    }

    pub(super) fn draw(
        &mut self,
        cursor_position: Option<(u16, u16)>,
        cursor_kind: CursorKind,
    ) -> io::Result<()> {
        self.inner.flush()?;

        if let Some((x, y)) = cursor_position {
            self.inner.set_cursor_position(Position::new(x, y))?;
        }

        match cursor_kind {
            CursorKind::Hidden => self.inner.hide_cursor()?,
            kind => HelixBackend::show_cursor(self.inner.backend_mut(), kind)?,
        }
        self.cursor_kind = cursor_kind;

        self.inner.swap_buffers();
        HelixBackend::flush(self.inner.backend_mut())
    }

    pub(super) fn present(
        &mut self,
        area: Rect,
        surface: Buffer,
        cursor_position: Option<(u16, u16)>,
        cursor_kind: CursorKind,
        full_redraw: bool,
    ) -> io::Result<Buffer> {
        if area != self.viewport_area {
            self.resize(area)?;
        }
        if full_redraw {
            self.clear()?;
        }
        let retired = std::mem::replace(self.current_buffer_mut(), surface);
        self.draw(cursor_position, cursor_kind)?;
        Ok(retired)
    }
}

fn rect_from_size(size: ratatui::layout::Size) -> Rect {
    tui::ratatui::to_helix_rect(RatatuiRect::new(0, 0, size.width, size.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::backend::TestBackend;

    #[test]
    fn app_terminal_flushes_ratatui_buffer_through_helix_backend() {
        let mut terminal = AppTerminal::new(TestBackend::new(4, 2)).unwrap();
        terminal.claim().unwrap();
        terminal
            .current_buffer_mut()
            .set_string(1, 0, "x", ratatui::style::Style::default());

        terminal.draw(None, CursorKind::Hidden).unwrap();

        assert_eq!(terminal.backend().buffer()[(1, 0)].symbol(), "x");
    }

    #[test]
    fn app_terminal_presents_an_owned_frame() {
        let mut terminal = AppTerminal::new(TestBackend::new(4, 2)).unwrap();
        terminal.claim().unwrap();
        let area = Rect::new(0, 0, 4, 2);
        let mut surface = Buffer::empty(tui::ratatui::to_ratatui_rect(area));
        surface.set_string(2, 1, "y", ratatui::style::Style::default());

        terminal
            .present(area, surface, None, CursorKind::Hidden, false)
            .unwrap();

        assert_eq!(terminal.backend().buffer()[(2, 1)].symbol(), "y");
    }
}
