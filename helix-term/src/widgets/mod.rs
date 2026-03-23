//! Stateless rendering widgets for terminal UI.
//!
//! Widgets are functions that take data + area + surface and draw. They don't
//! own state, don't handle events, and don't implement `Component`. This makes
//! them trivially composable — components (and plugins) build UI by calling
//! widget functions with the right data.
//!
//! Layout combinators live in `helix_view::layout` (shared across frontends).
//! These widgets are terminal-specific — they render to `tui::buffer::Buffer`.
//!
//! Trait-based capabilities (Focusable, Scrollable, Bounded, etc.) live in
//! `helix_view::traits` — the single canonical source for all frontends.

mod box_shadow;
mod divider;
mod header;
mod item_list;
mod message;
mod message_list;
mod scroll_region;
mod scrollbar;
mod spinner;
mod style;
mod text_input;

pub use box_shadow::BoxShadow;
pub use divider::{hdivider, vdivider};
pub use header::{header, header_with_counts};
pub use item_list::{item_list, ListState, ListStyles};
pub use message::{message, MessageAlign, MessageCorners, MessageState, MessageStyle};
pub use message_list::{
    message_list, Message, MessageAccessory, MessageAccessoryAlign, MessageAccessoryVisibility,
    MessageCursor, MessageDecoration, MessageDetailsVisibility, MessageKind, MessageLayout,
    MessageListState,
};
pub use scroll_region::{scroll_region, ScrollState, ScrollStyles};
pub use scrollbar::Scrollbar;
pub use spinner::Spinner;
pub use style::WidgetStyle;
pub use text_input::{text_input, TextInputState};
