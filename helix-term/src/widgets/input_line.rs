//! Single-line text input with cursor and editing.
//!
//! A persistent, editable single-line text field for embedding in panels
//! and components. Handles insert-mode editing (chars, backspace, delete,
//! arrows, home/end). Mode switching is left to the editing engine.
//!
//! Renders via the [`text_input`](super::text_input) widget.

use helix_core::unicode::segmentation::GraphemeCursor;
use helix_view::graphics::Rect;
use helix_view::input::{KeyCode, KeyEvent};
use tui::buffer::Buffer as Surface;

/// Single-line text buffer with cursor.
pub struct InputLine {
    text: String,
    cursor: usize, // byte offset
    focused: bool,
}

impl InputLine {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            focused: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Take the text content and reset the buffer.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Clear all text.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Handle an insert-mode key event. Returns `true` if consumed.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(ch) => {
                self.text.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
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
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.text.len();
                true
            }
            _ => false,
        }
    }

    /// Render the input line. Returns the [`super::TextInputState`] with
    /// cursor screen position.
    pub fn render(
        &self,
        surface: &mut Surface,
        area: Rect,
        styles: &super::traits::WidgetStyle,
    ) -> super::TextInputState {
        super::text_input(
            surface,
            area,
            &self.text,
            self.cursor,
            styles.text,
            styles.cursor,
        )
    }
}

impl Default for InputLine {
    fn default() -> Self {
        Self::new()
    }
}

impl super::traits::TextContent for InputLine {
    fn text(&self) -> &str {
        &self.text
    }
}

impl super::traits::TextCursor for InputLine {
    fn cursor(&self) -> usize {
        self.cursor
    }
}

impl super::traits::Focusable for InputLine {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl super::traits::TextBuffer for InputLine {
    fn clear(&mut self) {
        self.clear();
    }

    fn take(&mut self) -> String {
        self.take()
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::input::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn insert_chars() {
        let mut il = InputLine::new();
        il.handle_key(&key(KeyCode::Char('h')));
        il.handle_key(&key(KeyCode::Char('i')));
        assert_eq!(il.text(), "hi");
        assert_eq!(il.cursor(), 2);
    }

    #[test]
    fn backspace_deletes() {
        let mut il = InputLine::new();
        il.handle_key(&key(KeyCode::Char('a')));
        il.handle_key(&key(KeyCode::Char('b')));
        il.handle_key(&key(KeyCode::Backspace));
        assert_eq!(il.text(), "a");
    }

    #[test]
    fn take_returns_and_clears() {
        let mut il = InputLine::new();
        il.handle_key(&key(KeyCode::Char('x')));
        let taken = il.take();
        assert_eq!(taken, "x");
        assert!(il.is_empty());
    }
}
