//! Composable component traits.
//!
//! These traits form a hierarchy of capabilities that UI components can
//! implement. Higher-level abstractions (EditRegion, ContentRegion) are
//! compositions of these primitives — not special types with baked-in
//! assumptions.
//!
//! ## Tier 0 — Atomic Traits (no dependencies)
//!
//! [`Bounded`], [`Identified`], [`TextContent`], [`Focusable`], [`Modal`]
//!
//! ## Tier 1 — Spatial Composition (builds on Tier 0)
//!
//! [`Viewport`]: [`Identified`] + [`Bounded`]
//! [`Scrollable`]: [`Bounded`]

use crate::document::Mode;
use crate::graphics::Rect;
use crate::view::ViewPosition;
use crate::ViewId;
use helix_core::doc_formatter::TextFormat;
use helix_core::history::UndoKind;
use helix_core::indent::IndentStyle;
use helix_core::line_ending::LineEnding;
use helix_core::syntax::Syntax;
use helix_core::text_annotations::TextAnnotations;
use helix_core::{Rope, Selection, Transaction};

// ---------------------------------------------------------------------------
// Tier 0 — Atomic traits
// ---------------------------------------------------------------------------

/// Has a screen rectangle.
pub trait Bounded {
    fn area(&self) -> Rect;
    fn set_area(&mut self, area: Rect);
}

/// Has a unique identity for keying per-view state (selections, scroll, etc.)
pub trait Identified {
    fn id(&self) -> ViewId;
}

impl Identified for ViewId {
    fn id(&self) -> ViewId {
        *self
    }
}

/// Provides read access to text content.
pub trait TextContent {
    fn text(&self) -> &Rope;
}

/// Applies text transactions against content and per-viewport state.
pub trait MutableText: TextContent {
    fn apply(&mut self, transaction: &Transaction, view_id: ViewId) -> bool;
}

/// Produces text layout formatting for a viewport width.
pub trait FormattableText: TextContent {
    fn text_format(&self, viewport_width: u16) -> TextFormat;
}

/// Exposes tab width for text-coordinate calculations.
pub trait TextMetrics {
    fn tab_width(&self) -> usize;
}

/// Exposes indentation preferences used by editing transforms.
pub trait Indentation {
    fn indent_style(&self) -> IndentStyle;
    fn indent_width(&self) -> usize;
}

/// Exposes the document line ending used for inserted text.
pub trait LineEndingAware {
    fn line_ending(&self) -> LineEnding;
}

/// Exposes syntax/tree-sitter state for syntax-aware commands.
pub trait SyntaxAware {
    fn syntax(&self) -> Option<&Syntax>;
}

/// Stores per-viewport cursor selections.
pub trait Selectable {
    fn selection(&self, view_id: ViewId) -> &Selection;
    fn set_selection(&mut self, view_id: ViewId, selection: Selection);
}

/// Can receive or lose keyboard focus.
pub trait Focusable {
    fn is_focused(&self) -> bool;
    fn set_focused(&mut self, focused: bool);
    fn toggle_focus(&mut self) {
        let current = self.is_focused();
        self.set_focused(!current);
    }
}

/// Has an editing mode (Normal, Insert, Select).
pub trait Modal {
    fn mode(&self) -> Mode;
    fn set_mode(&mut self, mode: Mode);
}

impl Modal for Mode {
    fn mode(&self) -> Mode {
        *self
    }

    fn set_mode(&mut self, mode: Mode) {
        *self = mode;
    }
}

// ---------------------------------------------------------------------------
// Tier 1 — Spatial composition
// ---------------------------------------------------------------------------

/// A positioned viewport into content: identity + screen area + scroll offset.
/// This is the fundamental "window into something" primitive.
pub trait Viewport: Identified + Bounded {
    fn offset(&self) -> &ViewPosition;
    fn set_offset(&mut self, pos: ViewPosition);
}

/// Provides the viewport-specific text rendering context for movement/navigation.
pub trait NavigableViewport<D>: Identified {
    fn text_area_width(&self, doc: &D) -> u16;
    fn text_annotations<'a>(&self, doc: &'a D) -> TextAnnotations<'a>;
}

/// A text viewport with doc-owned view position state.
pub trait TextViewport<D>: NavigableViewport<D> {
    fn text_area(&self, doc: &D) -> Rect;
    fn view_offset(&self, doc: &D) -> ViewPosition;
    fn set_view_offset(&self, doc: &mut D, pos: ViewPosition);
}

/// Tracks history-side viewport state (jump selections, synced revisions).
pub trait HistoryViewport<D>: Identified {
    fn apply_history_transaction(&mut self, transaction: &Transaction, doc: &mut D);
    fn sync_changes(&mut self, doc: &mut D);
}

/// Supports saving cursor positions for later jump navigation.
pub trait Jumpable<D>: HistoryViewport<D> {
    fn push_jump(&mut self, doc: &mut D);
}

/// Supports undo/redo/history navigation against a viewport's history state.
pub trait Undoable<V>: MutableText {
    fn undo(&mut self, viewport: &mut V) -> bool;
    fn redo(&mut self, viewport: &mut V) -> bool;
    fn earlier(&mut self, viewport: &mut V, kind: UndoKind) -> bool;
    fn later(&mut self, viewport: &mut V, kind: UndoKind) -> bool;
    fn commit_undo_checkpoint(&mut self, viewport: &mut V);
}

/// Can scroll through content that exceeds the viewport.
pub trait Scrollable: Bounded {
    fn scroll(&self) -> usize;
    fn scroll_to(&mut self, offset: usize);
    fn content_height(&self) -> usize;

    fn max_scroll(&self) -> usize {
        self.content_height()
            .saturating_sub(self.area().height as usize)
    }
}
