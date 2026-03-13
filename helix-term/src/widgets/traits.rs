//! Trait interfaces and style types for composable widgets and components.
//!
//! Small, focused traits that capture shared behavioral contracts.
//! Compose them for generic code — e.g. a panel that works with any
//! `TextBuffer + TextCursor` for its input, or layout code that works
//! with any `Scrollable`.
//!
//! # Traits
//!
//! ```text
//! TextContent   — read-only text access
//! TextCursor    — byte-offset cursor within content
//! TextBuffer    — mutable: clear, take
//! Scrollable    — vertical scroll state
//! Focusable     — focus management
//! ```
//!
//! # Styles
//!
//! [`WidgetStyle`] bundles the resolved styles a text widget needs.
//! Build one from a theme + focus state, then pass it to `render()`.

use helix_view::theme::{Modifier, Style};

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// Read-only access to text content.
pub trait TextContent {
    /// The full text content.
    fn text(&self) -> &str;

    /// Whether the content is empty.
    fn is_empty(&self) -> bool {
        self.text().is_empty()
    }
}

/// A cursor position within text content (byte offset).
///
/// Separate from [`TextContent`] because read-only text displays
/// (labels, status bars) have content but no cursor.
pub trait TextCursor {
    /// Byte offset of the cursor within the text.
    fn cursor(&self) -> usize;
}

/// A mutable text buffer that can be cleared or consumed.
///
/// Separate from [`TextCursor`] because some buffers may not expose
/// a cursor (e.g. an append-only log buffer).
pub trait TextBuffer: TextContent {
    /// Clear all content and reset cursor/scroll state.
    fn clear(&mut self);

    /// Extract the content and reset the buffer.
    fn take(&mut self) -> String;
}

// ---------------------------------------------------------------------------
// Scroll
// ---------------------------------------------------------------------------

/// Vertical scroll state for any scrollable widget or component.
///
/// Implementors: [`TextBox`](super::TextBox), panel chat views,
/// list views, etc. Stateless render functions (`scroll_region`,
/// `item_list`) take scroll as a parameter instead.
pub trait Scrollable {
    /// Current scroll offset (first visible row/item).
    fn scroll(&self) -> usize;

    /// Total number of rows/items in the content.
    fn len(&self) -> usize;

    /// Scroll to the given offset (clamped internally).
    fn scroll_to(&mut self, offset: usize);

    /// Whether there are no rows/items.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum valid scroll offset.
    fn max_scroll(&self, viewport_height: usize) -> usize {
        self.len().saturating_sub(viewport_height)
    }
}

// ---------------------------------------------------------------------------
// Focus
// ---------------------------------------------------------------------------

/// Focus management for interactive components.
///
/// Any component that can receive or lose keyboard focus. The compositor
/// routes key events only to focused components.
pub trait Focusable {
    /// Whether this component currently has focus.
    fn is_focused(&self) -> bool;

    /// Set the focus state.
    fn set_focused(&mut self, focused: bool);

    /// Toggle focus.
    fn toggle_focus(&mut self) {
        let current = self.is_focused();
        self.set_focused(!current);
    }
}

// ---------------------------------------------------------------------------
// Widget styles
// ---------------------------------------------------------------------------

/// Resolved styles for text widgets.
///
/// Constructed from a theme + focus state. Passed to `render()` so widgets
/// don't need theme access or focus-branching logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WidgetStyle {
    /// Base text style.
    pub text: Style,
    /// Cursor cell style (typically reversed text).
    pub cursor: Style,
    /// Placeholder/hint text style (dimmed).
    pub placeholder: Style,
    /// Selection highlight style.
    pub selection: Style,
    /// Border/frame style.
    pub border: Style,
}

impl WidgetStyle {
    /// Resolve styles from a theme, adjusting for focus state.
    ///
    /// Focused widgets get full-intensity styles. Unfocused widgets get
    /// dimmed cursor and selection.
    pub fn from_theme(theme: &helix_view::Theme, focused: bool) -> Self {
        let text = theme.get("ui.text");
        let cursor = if focused {
            text.patch(Style::default().add_modifier(Modifier::REVERSED))
        } else {
            text
        };
        let placeholder = theme.get("ui.text.inactive");
        let selection = if focused {
            theme.get("ui.selection")
        } else {
            theme.get("ui.selection.inactive")
        };
        let border = if focused {
            theme.get("ui.window")
        } else {
            theme.get("ui.window.inactive")
        };

        Self {
            text,
            cursor,
            placeholder,
            selection,
            border,
        }
    }

    /// Override the text style.
    pub fn with_text(mut self, text: Style) -> Self {
        self.text = text;
        self
    }

    /// Override the cursor style.
    pub fn with_cursor(mut self, cursor: Style) -> Self {
        self.cursor = cursor;
        self
    }

    /// Override the placeholder style.
    pub fn with_placeholder(mut self, placeholder: Style) -> Self {
        self.placeholder = placeholder;
        self
    }

    /// Override the selection style.
    pub fn with_selection(mut self, selection: Style) -> Self {
        self.selection = selection;
        self
    }

    /// Override the border style.
    pub fn with_border(mut self, border: Style) -> Self {
        self.border = border;
        self
    }

    /// Apply a theme patch to all styles (e.g. a background color).
    pub fn patch(mut self, patch: Style) -> Self {
        self.text = self.text.patch(patch);
        self.cursor = self.cursor.patch(patch);
        self.placeholder = self.placeholder.patch(patch);
        self.selection = self.selection.patch(patch);
        self.border = self.border.patch(patch);
        self
    }
}
