//! Multi-line text box widget with cursor and scrolling.
//!
//! A persistent, editable text area for embedding in panels. Handles
//! insert-mode editing (chars, backspace, delete, arrows, home/end) and
//! renders with cursor and optional scrollbar. Mode switching (Esc, i, a)
//! is left to the editing engine — this widget only processes text editing keys.
//!
//! # Sizing
//!
//! The caller controls the `Rect` passed to `render()`. The text box fills
//! the given area and scrolls vertically when content exceeds it.
//! [`TextBox::visible_rows`] returns how many rows the content needs, so
//! callers can implement grow-to-fit by clamping between min/max.

use helix_core::unicode::segmentation::{GraphemeCursor, UnicodeSegmentation};
use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::Rect;
use helix_view::input::{KeyCode, KeyEvent};
use tui::buffer::Buffer as Surface;

/// Multi-line text buffer with 2D cursor.
pub struct TextBox {
    /// Full text content (newlines delimit lines).
    text: String,
    /// Byte offset of the cursor within `text`.
    cursor: usize,
    /// First visible line (vertical scroll offset).
    scroll: usize,
    /// Whether this widget has keyboard focus.
    focused: bool,
}

/// Computed state returned by [`TextBox::render`].
pub struct TextBoxState {
    /// Screen position of the cursor (absolute).
    pub cursor_x: u16,
    pub cursor_y: u16,
    /// Total number of lines in the buffer.
    pub total_lines: usize,
    /// Maximum scroll value.
    pub max_scroll: usize,
}

impl TextBox {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            scroll: 0,
            focused: false,
        }
    }

    // -- Accessors ----------------------------------------------------------

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Number of lines the content occupies (at least 1).
    pub fn line_count(&self) -> usize {
        self.text.lines().count().max(1)
    }

    /// How many visible rows the content needs (for grow-to-fit layouts).
    /// Returns at least 1.
    pub fn visible_rows(&self) -> usize {
        self.line_count()
    }

    /// Take the text content and reset the buffer.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.scroll = 0;
        std::mem::take(&mut self.text)
    }

    /// Clear all text.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.scroll = 0;
    }

    // -- Cursor geometry ----------------------------------------------------

    /// Returns (row, col_byte_offset_in_line) of the cursor.
    fn cursor_pos(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.matches('\n').count();
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        (row, self.cursor - line_start)
    }

    /// Byte offset of the start of the given line (0-indexed).
    fn line_start(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        self.text
            .match_indices('\n')
            .nth(line - 1)
            .map_or(self.text.len(), |(i, _)| i + 1)
    }

    /// Byte offset of the end of the given line (before '\n' or at text end).
    fn line_end(&self, line: usize) -> usize {
        let start = self.line_start(line);
        self.text[start..]
            .find('\n')
            .map_or(self.text.len(), |i| start + i)
    }

    /// Content of the given line (without trailing '\n').
    fn line_content(&self, line: usize) -> &str {
        &self.text[self.line_start(line)..self.line_end(line)]
    }

    /// Ensure the cursor row is visible given the viewport height.
    fn ensure_visible(&mut self, viewport_height: usize) {
        if viewport_height == 0 {
            return;
        }
        let (row, _) = self.cursor_pos();
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + viewport_height {
            self.scroll = row + 1 - viewport_height;
        }
    }

    // -- Editing (insert-mode keys) -----------------------------------------

    /// Handle an insert-mode key event. Returns `true` if consumed.
    pub fn handle_key(&mut self, key: &KeyEvent, viewport_height: usize) -> bool {
        let consumed = match key.code {
            KeyCode::Char(ch) => {
                self.text.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
                true
            }
            KeyCode::Enter => {
                self.text.insert(self.cursor, '\n');
                self.cursor += 1;
                true
            }
            KeyCode::Tab => {
                self.text.insert(self.cursor, '\t');
                self.cursor += 1;
                true
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_grapheme(&self.text, self.cursor);
                    self.text.drain(prev..self.cursor);
                    self.cursor = prev;
                }
                true
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    let next = next_grapheme(&self.text, self.cursor);
                    self.text.drain(self.cursor..next);
                }
                true
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_grapheme(&self.text, self.cursor);
                }
                true
            }
            KeyCode::Right => {
                if self.cursor < self.text.len() {
                    self.cursor = next_grapheme(&self.text, self.cursor);
                }
                true
            }
            KeyCode::Up => {
                let (row, col_offset) = self.cursor_pos();
                if row > 0 {
                    let col_width = self.line_content(row)[..col_offset].width();
                    let prev_line = self.line_content(row - 1);
                    let byte_col = grapheme_offset_at_width(prev_line, col_width);
                    self.cursor = self.line_start(row - 1) + byte_col;
                }
                true
            }
            KeyCode::Down => {
                let (row, col_offset) = self.cursor_pos();
                let total = self.line_count();
                if row + 1 < total {
                    let col_width = self.line_content(row)[..col_offset].width();
                    let next_line = self.line_content(row + 1);
                    let byte_col = grapheme_offset_at_width(next_line, col_width);
                    self.cursor = self.line_start(row + 1) + byte_col;
                }
                true
            }
            KeyCode::Home => {
                let (row, _) = self.cursor_pos();
                self.cursor = self.line_start(row);
                true
            }
            KeyCode::End => {
                let (row, _) = self.cursor_pos();
                self.cursor = self.line_end(row);
                true
            }
            _ => false,
        };

        if consumed {
            self.ensure_visible(viewport_height);
        }
        consumed
    }

    // -- Rendering ----------------------------------------------------------

    /// Render the text box into `area`. Returns computed state for cursor
    /// positioning.
    pub fn render(
        &mut self,
        surface: &mut Surface,
        area: Rect,
        styles: &super::traits::WidgetStyle,
    ) -> TextBoxState {
        let style = styles.text;
        let cursor_style = styles.cursor;
        let height = area.height as usize;
        let width = area.width as usize;

        if height == 0 || width == 0 {
            return TextBoxState {
                cursor_x: area.x,
                cursor_y: area.y,
                total_lines: self.line_count(),
                max_scroll: 0,
            };
        }

        // Ensure scroll is valid.
        let total = self.line_count();
        let max_scroll = total.saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);

        // Render visible lines.
        for row in 0..height {
            let line_idx = self.scroll + row;
            let y = area.y + row as u16;

            if line_idx < total {
                let content = self.line_content(line_idx);
                let truncated = if content.width() > width {
                    &content[..grapheme_offset_at_width(content, width)]
                } else {
                    content
                };
                surface.set_string(area.x, y, truncated, style);
            }
        }

        // Compute cursor screen position.
        let (cursor_row, col_offset) = self.cursor_pos();
        let col_width = self.line_content(cursor_row)[..col_offset].width() as u16;
        let screen_row = cursor_row.saturating_sub(self.scroll) as u16;
        let cx = (area.x + col_width).min(area.right().saturating_sub(1));
        let cy = (area.y + screen_row).min(area.bottom().saturating_sub(1));

        // Draw cursor cell.
        if screen_row < area.height {
            let cell = &mut surface[(cx, cy)];
            cell.set_style(cursor_style);
        }

        TextBoxState {
            cursor_x: cx,
            cursor_y: cy,
            total_lines: total,
            max_scroll,
        }
    }
}

impl Default for TextBox {
    fn default() -> Self {
        Self::new()
    }
}

impl super::traits::TextContent for TextBox {
    fn text(&self) -> &str {
        &self.text
    }
}

impl super::traits::TextCursor for TextBox {
    fn cursor(&self) -> usize {
        self.cursor
    }
}

impl super::traits::TextBuffer for TextBox {
    fn clear(&mut self) {
        self.clear();
    }

    fn take(&mut self) -> String {
        self.take()
    }
}

impl super::traits::Focusable for TextBox {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl super::traits::Scrollable for TextBox {
    fn scroll(&self) -> usize {
        self.scroll
    }

    fn len(&self) -> usize {
        self.line_count()
    }

    fn scroll_to(&mut self, offset: usize) {
        self.scroll = offset.min(self.line_count().saturating_sub(1));
    }
}

// -- Grapheme helpers -------------------------------------------------------

fn prev_grapheme(text: &str, byte_offset: usize) -> usize {
    let mut cursor = GraphemeCursor::new(byte_offset, text.len(), true);
    cursor
        .prev_boundary(text, 0)
        .ok()
        .flatten()
        .unwrap_or(byte_offset)
}

fn next_grapheme(text: &str, byte_offset: usize) -> usize {
    let mut cursor = GraphemeCursor::new(byte_offset, text.len(), true);
    cursor
        .next_boundary(text, 0)
        .ok()
        .flatten()
        .unwrap_or(byte_offset)
}

/// Find the byte offset in `line` that corresponds to the given display width.
/// Stops at the nearest grapheme boundary without exceeding `target_width`.
fn grapheme_offset_at_width(line: &str, target_width: usize) -> usize {
    let mut width = 0;
    for (idx, grapheme) in line.grapheme_indices(true) {
        let gw = grapheme.width();
        if width + gw > target_width {
            return idx;
        }
        width += gw;
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: helix_view::input::KeyModifiers::NONE,
        }
    }

    #[test]
    fn insert_and_cursor_position() {
        let mut tb = TextBox::new();
        tb.handle_key(&key(KeyCode::Char('a')), 10);
        tb.handle_key(&key(KeyCode::Char('b')), 10);
        assert_eq!(tb.text(), "ab");
        assert_eq!(tb.cursor_pos(), (0, 2));
    }

    #[test]
    fn newline_creates_lines() {
        let mut tb = TextBox::new();
        tb.handle_key(&key(KeyCode::Char('a')), 10);
        tb.handle_key(&key(KeyCode::Enter), 10);
        tb.handle_key(&key(KeyCode::Char('b')), 10);
        assert_eq!(tb.text(), "a\nb");
        assert_eq!(tb.line_count(), 2);
        assert_eq!(tb.cursor_pos(), (1, 1));
    }

    #[test]
    fn up_down_navigation() {
        let mut tb = TextBox::new();
        tb.text = "hello\nworld".into();
        tb.cursor = 11; // end of "world"

        tb.handle_key(&key(KeyCode::Up), 10);
        assert_eq!(tb.cursor_pos(), (0, 5)); // end of "hello"

        tb.handle_key(&key(KeyCode::Down), 10);
        assert_eq!(tb.cursor_pos(), (1, 5)); // end of "world"
    }

    #[test]
    fn backspace_across_newline() {
        let mut tb = TextBox::new();
        tb.text = "a\nb".into();
        tb.cursor = 2; // start of "b" line
        tb.handle_key(&key(KeyCode::Backspace), 10);
        assert_eq!(tb.text(), "ab");
        assert_eq!(tb.cursor_pos(), (0, 1));
    }

    #[test]
    fn take_clears_buffer() {
        let mut tb = TextBox::new();
        tb.text = "hello".into();
        tb.cursor = 5;
        let taken = tb.take();
        assert_eq!(taken, "hello");
        assert!(tb.is_empty());
        assert_eq!(tb.cursor(), 0);
    }

    #[test]
    fn scroll_follows_cursor() {
        let mut tb = TextBox::new();
        tb.text = "1\n2\n3\n4\n5\n6".into();
        tb.cursor = tb.text.len(); // line 5 (0-indexed)
        tb.ensure_visible(3); // viewport of 3 lines
        assert_eq!(tb.scroll, 3); // lines 3,4,5 visible
    }
}
