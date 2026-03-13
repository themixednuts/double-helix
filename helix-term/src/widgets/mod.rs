//! Stateless rendering widgets for terminal UI.
//!
//! Widgets are functions that take data + area + surface and draw. They don't
//! own state, don't handle events, and don't implement `Component`. This makes
//! them trivially composable — components (and plugins) build UI by calling
//! widget functions with the right data.
//!
//! Layout combinators live in `helix_view::layout` (shared across frontends).
//! These widgets are terminal-specific — they render to `tui::buffer::Buffer`.

mod box_shadow;
mod divider;
mod header;
mod input_line;
mod item_list;
mod scroll_region;
mod scrollbar;
mod text_box;
mod text_input;
pub mod traits;

pub use box_shadow::BoxShadow;
pub use divider::{hdivider, vdivider};
pub use header::{header, header_with_counts};
pub use input_line::InputLine;
pub use item_list::{item_list, ListState, ListStyles};
pub use scroll_region::{scroll_region, ScrollState, ScrollStyles};
pub use scrollbar::Scrollbar;
pub use text_box::TextBox;
pub use text_input::{text_input, TextInputState};
pub use traits::{Focusable, Scrollable, TextBuffer, TextContent, TextCursor, WidgetStyle};
