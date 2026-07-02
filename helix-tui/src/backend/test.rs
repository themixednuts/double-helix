use crate::{backend::Backend, terminal::Config};
use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{CursorKind, Rect};
use ratatui::{
    buffer::{Buffer, Cell},
    layout::{Position, Rect as RatatuiRect, Size},
};
use std::{fmt::Write, io};

/// A backend used for the integration tests.
#[derive(Debug)]
pub struct TestBackend {
    width: u16,
    buffer: Buffer,
    height: u16,
    cursor: bool,
    pos: (u16, u16),
}

/// Returns a string representation of the given buffer for debugging purpose.
fn buffer_view(buffer: &Buffer) -> String {
    let mut view =
        String::with_capacity(buffer.content().len() + buffer.area().height as usize * 3);
    for cells in buffer.content().chunks(buffer.area().width as usize) {
        let mut overwritten = vec![];
        let mut skip: usize = 0;
        view.push('"');
        for (x, c) in cells.iter().enumerate() {
            if skip == 0 {
                view.push_str(c.symbol());
            } else {
                overwritten.push((x, c.symbol()))
            }
            skip = std::cmp::max(skip, c.symbol().width()).saturating_sub(1);
        }
        view.push('"');
        if !overwritten.is_empty() {
            write!(
                &mut view,
                " Hidden by multi-width symbols: {:?}",
                overwritten
            )
            .unwrap();
        }
        view.push('\n');
    }
    view
}

impl TestBackend {
    pub fn new(width: u16, height: u16) -> TestBackend {
        TestBackend {
            width,
            height,
            buffer: Buffer::empty(RatatuiRect::new(0, 0, width, height)),
            cursor: false,
            pos: (0, 0),
        }
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.buffer.resize(RatatuiRect::new(0, 0, width, height));
        self.width = width;
        self.height = height;
    }

    pub fn assert_buffer(&self, expected: &Buffer) {
        assert_eq!(expected.area(), self.buffer.area());
        let diff = expected.diff(&self.buffer);
        if diff.is_empty() {
            return;
        }

        let mut debug_info = String::from("Buffers are not equal");
        debug_info.push('\n');
        debug_info.push_str("Expected:");
        debug_info.push('\n');
        let expected_view = buffer_view(expected);
        debug_info.push_str(&expected_view);
        debug_info.push('\n');
        debug_info.push_str("Got:");
        debug_info.push('\n');
        let view = buffer_view(&self.buffer);
        debug_info.push_str(&view);
        debug_info.push('\n');

        debug_info.push_str("Diff:");
        debug_info.push('\n');
        let nice_diff = diff
            .iter()
            .enumerate()
            .map(|(i, (x, y, cell))| {
                let expected_cell = &expected[(*x, *y)];
                format!(
                    "{}: at ({}, {}) expected {:?} got {:?}",
                    i, x, y, expected_cell, cell
                )
            })
            .collect::<Vec<String>>()
            .join("\n");
        debug_info.push_str(&nice_diff);
        panic!("{}", debug_info);
    }

    fn draw_cells<'a, I>(&mut self, content: I) -> Result<(), io::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, c) in content {
            if let Some(cell) = self.buffer.cell_mut((x, y)) {
                *cell = c.clone();
            }
        }
        Ok(())
    }
}

impl Backend for TestBackend {
    fn claim(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    fn reconfigure(&mut self, _config: Config) -> Result<(), io::Error> {
        Ok(())
    }

    fn restore(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    fn draw<'a, I>(&mut self, content: I) -> Result<(), io::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.draw_cells(content)
    }

    fn hide_cursor(&mut self) -> Result<(), io::Error> {
        self.cursor = false;
        Ok(())
    }

    fn show_cursor(&mut self, _kind: CursorKind) -> Result<(), io::Error> {
        self.cursor = true;
        Ok(())
    }

    fn set_cursor(&mut self, x: u16, y: u16) -> Result<(), io::Error> {
        self.pos = (x, y);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), io::Error> {
        self.buffer.reset();
        Ok(())
    }

    fn size(&self) -> Result<Rect, io::Error> {
        Ok(Rect::new(0, 0, self.width, self.height))
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    fn supports_true_color(&self) -> bool {
        false
    }

    fn get_theme_mode(&self) -> Option<helix_view::theme::Mode> {
        None
    }
}

impl ratatui::backend::Backend for TestBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.draw_cells(content)
    }

    fn append_lines(&mut self, _n: u16) -> Result<(), Self::Error> {
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.cursor = false;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.cursor = true;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        Ok(Position::new(self.pos.0, self.pos.1))
    }

    fn set_cursor_position<P>(&mut self, position: P) -> Result<(), Self::Error>
    where
        P: Into<Position>,
    {
        let position = position.into();
        self.pos = (position.x, position.y);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.buffer.reset();
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> Result<(), Self::Error> {
        match clear_type {
            ratatui::backend::ClearType::All => self.buffer.reset(),
            _ if self.width == 0 || self.height == 0 => {}
            ratatui::backend::ClearType::AfterCursor => {
                let RatatuiRect { width, height, .. } = *self.buffer.area();
                for y in self.pos.1..height {
                    let start = if y == self.pos.1 { self.pos.0 } else { 0 };
                    for x in start..width {
                        self.buffer[(x, y)].reset();
                    }
                }
            }
            ratatui::backend::ClearType::BeforeCursor => {
                for y in 0..=self.pos.1.min(self.height.saturating_sub(1)) {
                    let end = if y == self.pos.1 {
                        self.pos.0.min(self.width.saturating_sub(1))
                    } else {
                        self.width.saturating_sub(1)
                    };
                    for x in 0..=end {
                        self.buffer[(x, y)].reset();
                    }
                }
            }
            ratatui::backend::ClearType::CurrentLine => {
                if self.pos.1 < self.height {
                    for x in 0..self.width {
                        self.buffer[(x, self.pos.1)].reset();
                    }
                }
            }
            ratatui::backend::ClearType::UntilNewLine => {
                if self.pos.1 < self.height {
                    for x in self.pos.0..self.width {
                        self.buffer[(x, self.pos.1)].reset();
                    }
                }
            }
        }
        Ok(())
    }

    fn size(&self) -> Result<Size, Self::Error> {
        Ok(Size::new(self.width, self.height))
    }

    fn window_size(&mut self) -> Result<ratatui::backend::WindowSize, Self::Error> {
        Ok(ratatui::backend::WindowSize {
            columns_rows: Size::new(self.width, self.height),
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn scroll_region_up(
        &mut self,
        _region: std::ops::Range<u16>,
        _line_count: u16,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn scroll_region_down(
        &mut self,
        _region: std::ops::Range<u16>,
        _line_count: u16,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}
