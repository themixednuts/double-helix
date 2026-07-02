//! Stateless rendering widgets for terminal UI.
//!
//! Widgets are functions that take data + area + surface and draw. They don't
//! own state, don't handle events, and don't implement `Component`. This makes
//! them trivially composable — components (and plugins) build UI by calling
//! widget functions with the right data.
//!
//! Layout combinators live in `helix_view::layout` (shared across frontends).
//! These widgets are terminal-specific — they render to Ratatui buffers.
//!
//! Trait-based capabilities (Focusable, Scrollable, Bounded, etc.) live in
//! `helix_view::traits` — the single canonical source for all frontends.

mod box_shadow;
mod chip_strip;
mod diff_view;
mod divider;
mod header;
mod hint_bar;
mod item_list;
mod marquee;
mod message;
mod message_list;
mod panel;
mod picker_table;
mod progress;
mod scroll_region;
mod scrollbar;
mod selection_viewport;
mod spinner;
mod style;
mod surface;
mod table;
mod tabs;
mod text_input;
mod toast;
mod tree_list;

pub use box_shadow::BoxShadow;
pub use chip_strip::{chip_strip_left, chip_strip_right, Chip};
pub use diff_view::{
    diff_text, diff_view, DiffDocument, DiffLine, DiffLineKind, DiffOptions, DiffSpan, DiffStyles,
    DiffViewState,
};
pub use divider::{border, hdivider, vdivider};
pub use header::{header, header_with_counts};
pub use hint_bar::{hint_bar, hint_bar_layout, Hint, HintBarState, HintBarStyle};
pub use item_list::{item_list, item_list_with_marks, ListState, ListStyles, MarkedItems};
pub use marquee::{
    schedule_redraw_at, Marquee, DEFAULT_HOLD_END, DEFAULT_HOLD_START, DEFAULT_INACTIVITY_TIMEOUT,
    DEFAULT_SCROLL_DURATION,
};
pub use message::{message, MessageAlign, MessageCorners, MessageState, MessageStyle};
pub use message_list::{
    message_list, Message, MessageAccessory, MessageAccessoryAlign, MessageAccessoryVisibility,
    MessageCursor, MessageDecoration, MessageDetailsVisibility, MessageKind, MessageLayout,
    MessageListState,
};
pub use panel::{inset, Panel, PanelEdge, PanelStyle, PanelVariant};
pub use picker_table::PickerTable;
pub use progress::{progress_bar, progress_fill, ProgressState, ProgressStyle};
pub use scroll_region::{scroll_region, ScrollState, ScrollStyles};
pub use scrollbar::Scrollbar;
pub use selection_viewport::SelectionViewport;
pub use spinner::Spinner;
pub use style::WidgetStyle;
pub use surface::{draw_string_anchored, AnchoredText};
pub use table::{TableCell, TableRow};
pub use tabs::{
    tabs, tabs_layout, tabs_layout_with_options, tabs_with_options, Tab, TabCell, TabRange,
    TabsOptions, TabsScrollPolicy, TabsState, TabsStyle,
};
pub use text_input::{text_input, TextInputState};
pub use toast::{
    toast_queue, Toast, ToastId, ToastQueue, ToastQueueState, ToastSeverity, ToastStyle,
};
pub use tree_list::{
    tree_list, tree_list_label_offset, TreeListIcon, TreeListItem, TreeListStatus, TreeListStyles,
};
