//! Resolved style bundles for text widgets.
//!
//! [`WidgetStyle`] bundles the resolved styles a text widget needs.
//! Build one from a theme + focus state, then pass it to `render()`.

use helix_view::theme::{Modifier, Style};

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
