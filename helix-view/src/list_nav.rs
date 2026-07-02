//! Cursor + scroll state for any list-shaped UI surface — picker rows,
//! menu items, file explorer rows, future symbol / diagnostic browsers.
//!
//! Same shape as [`crate::edit_region::EditRegion`] and
//! [`crate::jump_labels::JumpSession`]: a shared state machine in
//! `helix-view`, hosts embed it and supply their own key dispatch +
//! rendering.
//!
//! # What this owns
//!
//! - The currently focused row (`selection`)
//! - The scroll offset (`scroll`) so a selection outside the viewport
//!   pulls the view to follow it
//! - The total number of items (`item_count`) and the visible height
//!   (`viewport_height`), both pushed in by the host whenever they
//!   change
//! - The boundary semantics (wrap vs. clamp), the page-size policy
//!   (half / full viewport / fixed N), and the bounds-clamp on
//!   item-count drops
//!
//! # What hosts own
//!
//! - The keymap (which physical keys bind to `move_by`, `page_by`, etc.).
//!   The file explorer wires through the modal engine; the picker
//!   reads raw `KeyEvent`s; the menu cycles via Tab. They all converge
//!   on the same set of state-mutation helpers, so the behavior is
//!   identical even though the keymaps aren't.
//! - The side effects (refresh preview, fire callback, re-render).
//!   `NavOutcome` tells the host whether anything actually changed —
//!   it can compare `selection()` across the call or just look at the
//!   outcome enum.
//! - What an "item" means. `selection` is just a `usize` index into
//!   the host's own item list.
//!
//! # Why not a key-dispatch method?
//!
//! Because the three current hosts each bind nav to different keys.
//! Picker uses Ctrl-N/Ctrl-P/Tab; menu cycles via Tab; file explorer
//! goes through the modal engine's `move_line_up` / `move_line_down`
//! tokens. Forcing a single key dispatch into `ListNav` would either
//! steal keys from one of those hosts or force them all to a common
//! denominator. State + helpers keeps each host in control of its
//! keymap while sharing the actual cursor math.

use std::ops::Range;

/// Pure viewport math for list-shaped UI surfaces.
///
/// `ListNav` owns mutable navigation state. This helper is the canonical
/// stateless projection used by widgets that are handed selection and scroll
/// values directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListViewport {
    total: usize,
    selected: Option<usize>,
    visible: usize,
    scroll: usize,
}

impl ListViewport {
    pub const fn new(total: usize, selected: Option<usize>, visible: usize, scroll: usize) -> Self {
        Self {
            total,
            selected,
            visible,
            scroll,
        }
    }

    pub fn max_scroll(self) -> usize {
        self.total.saturating_sub(self.visible)
    }

    pub fn clamped_scroll(self) -> usize {
        self.scroll.min(self.max_scroll())
    }

    pub fn scroll_to_selected(self) -> usize {
        let Some(selected) = self.selected else {
            return self.clamped_scroll();
        };
        if self.visible == 0 || self.total == 0 {
            return 0;
        }

        let selected = selected.min(self.total.saturating_sub(1));
        let scroll = self.clamped_scroll();
        if selected < scroll {
            selected
        } else if selected >= scroll.saturating_add(self.visible) {
            selected.saturating_add(1).saturating_sub(self.visible)
        } else {
            scroll
        }
    }

    pub fn visible_range(self) -> Range<usize> {
        let start = self.clamped_scroll();
        start..start.saturating_add(self.visible).min(self.total)
    }

    pub fn selected_visible_range(self) -> Range<usize> {
        let start = self.scroll_to_selected();
        start..start.saturating_add(self.visible).min(self.total)
    }
}

/// What the host should do after a navigation call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavOutcome {
    /// Selection changed. Host should refresh any selection-dependent
    /// UI (preview, callback, etc.).
    Moved,
    /// Selection didn't change — the user hit a boundary with `Clamp`
    /// wrap behavior, or the list was empty. Host can skip refresh.
    AtBoundary,
}

/// Whether boundary navigation wraps to the other end or stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapBehavior {
    /// `Down` at the last item wraps to the first; `Up` at the first
    /// wraps to the last. Menu and picker want this.
    Wrap,
    /// `Down` at the last item stays at the last; `Up` at the first
    /// stays at the first. File explorer wants this (a wrap would
    /// teleport across the whole tree).
    Clamp,
}

/// How many rows a page-up / page-down moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageSize {
    /// `viewport_height / 2` (rounded down, minimum 1). Menu and
    /// editor `Ctrl-D`/`Ctrl-U` use this — keeps half the previous
    /// page visible for context.
    HalfViewport,
    /// `viewport_height` (minimum 1). Picker `PgUp`/`PgDn` use this.
    FullViewport,
    /// Caller-supplied fixed count.
    Fixed(usize),
}

impl PageSize {
    fn rows(self, viewport_height: usize) -> usize {
        let v = viewport_height.max(1);
        match self {
            PageSize::HalfViewport => (v / 2).max(1),
            PageSize::FullViewport => v,
            PageSize::Fixed(n) => n.max(1),
        }
    }
}

/// Cursor + scroll state for a list-shaped UI surface.
///
/// Construct with [`ListNav::new`], push state in via [`set_item_count`]
/// / [`set_viewport_height`] whenever the list or visible area
/// changes, and call the move helpers from the host's key dispatch.
///
/// [`set_item_count`]: ListNav::set_item_count
/// [`set_viewport_height`]: ListNav::set_viewport_height
#[derive(Clone, Debug, Default)]
pub struct ListNav {
    selection: usize,
    scroll: usize,
    item_count: usize,
    viewport_height: usize,
}

impl ListNav {
    pub fn new() -> Self {
        Self::default()
    }

    // ---------- Read accessors --------------------------------------

    /// Currently selected item index. Meaningful only when
    /// `item_count > 0`; on an empty list this is `0` (a placeholder)
    /// — hosts should guard with `is_empty()` before indexing.
    pub fn selection(&self) -> usize {
        self.selection
    }

    /// First visible row. Use together with `viewport_height` to
    /// compute the visible window: `scroll..scroll + viewport_height`.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn item_count(&self) -> usize {
        self.item_count
    }

    pub fn viewport_height(&self) -> usize {
        self.viewport_height
    }

    pub fn is_empty(&self) -> bool {
        self.item_count == 0
    }

    // ---------- State pushes ---------------------------------------

    /// Tell the nav how many items the host's list contains right now.
    /// Clamps the current selection to the new range so we never
    /// point at a vanished item. Doesn't move scroll — call
    /// [`Self::ensure_visible`] after if the host wants the (possibly
    /// clamped) selection re-centred in the viewport.
    ///
    /// Called by hosts on every filter update / list rebuild — for the
    /// picker that's every keystroke, for the file explorer that's
    /// every refresh, for the menu that's on `update_matches`.
    pub fn set_item_count(&mut self, count: usize) {
        self.item_count = count;
        if count == 0 {
            self.selection = 0;
            self.scroll = 0;
            return;
        }
        if self.selection >= count {
            self.selection = count - 1;
        }
        // After clamping selection, scroll might point past the new
        // end of the list. Re-pull to keep the visible window
        // anchored on the selection rather than at a stale offset —
        // otherwise hosts see "selection visible, but scroll says
        // we're scrolled past it" inconsistencies.
        self.ensure_visible();
    }

    /// Tell the nav how many rows of the list are currently visible on
    /// screen. Called whenever the panel is laid out — the host
    /// computes `list_area.height` and pushes it in here. The scroll
    /// is re-anchored on the selection so a resize that shrinks the
    /// viewport doesn't leave the cursor off-screen.
    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height;
        self.ensure_visible();
    }

    /// Set the scroll offset directly, clamped to a sensible range.
    /// Used by mouse-wheel scroll handlers ([`crate::traits::Scrollable`])
    /// that move the viewport without changing the selection — the
    /// host wants pure scroll, not cursor movement.
    ///
    /// Doesn't touch `selection`. If the new scroll pushes the
    /// selection out of view, that's the caller's intent (cursor can
    /// be off-screen during a pure scroll); a subsequent
    /// [`Self::ensure_visible`] would pull it back.
    pub fn set_scroll(&mut self, offset: usize) {
        let max_scroll = self.item_count.saturating_sub(self.viewport_height.max(1));
        self.scroll = offset.min(max_scroll);
    }

    /// Direct jump to `index`. Clamps to `0..item_count`. No-op on
    /// empty lists. Returns whether the selection actually changed.
    pub fn set_selection(&mut self, index: usize) -> NavOutcome {
        if self.item_count == 0 {
            return NavOutcome::AtBoundary;
        }
        let target = index.min(self.item_count - 1);
        if target == self.selection {
            return NavOutcome::AtBoundary;
        }
        self.selection = target;
        self.ensure_visible();
        NavOutcome::Moved
    }

    // ---------- Movement helpers -----------------------------------

    /// Move the cursor by `delta` rows. Positive = down, negative = up.
    /// Honors `wrap` at boundaries.
    pub fn move_by(&mut self, delta: isize, wrap: WrapBehavior) -> NavOutcome {
        if self.item_count == 0 {
            return NavOutcome::AtBoundary;
        }
        let count = self.item_count as isize;
        let current = self.selection as isize;
        let target = match wrap {
            WrapBehavior::Wrap => current.saturating_add(delta).rem_euclid(count) as usize,
            WrapBehavior::Clamp => current.saturating_add(delta).clamp(0, count - 1) as usize,
        };
        if target == self.selection {
            return NavOutcome::AtBoundary;
        }
        self.selection = target;
        self.ensure_visible();
        NavOutcome::Moved
    }

    /// Page down by `pages` pages (positive) or up (negative). Page
    /// size is computed from `policy` against the current viewport.
    pub fn page_by(&mut self, pages: isize, page_size: PageSize, wrap: WrapBehavior) -> NavOutcome {
        let rows = page_size.rows(self.viewport_height);
        // Use `i64` to give saturating_mul plenty of headroom — a
        // viewport of 100 × pages of 1000 still fits.
        let delta = (pages as i64).saturating_mul(rows as i64);
        let delta = delta.clamp(isize::MIN as i64, isize::MAX as i64) as isize;
        self.move_by(delta, wrap)
    }

    /// Jump to the first item. No-op on empty list.
    pub fn to_first(&mut self) -> NavOutcome {
        self.set_selection(0)
    }

    /// Jump to the last item. No-op on empty list.
    pub fn to_last(&mut self) -> NavOutcome {
        if self.item_count == 0 {
            return NavOutcome::AtBoundary;
        }
        self.set_selection(self.item_count - 1)
    }

    /// Slide the scroll offset so the selection is on-screen. Idempotent.
    /// Hosts that change item-count or viewport-height should call
    /// this afterwards to keep the cursor visible. Movement helpers
    /// call it automatically — this is just exposed for hosts that
    /// reorder the list or otherwise jump the selection through paths
    /// other than the helpers.
    pub fn ensure_visible(&mut self) {
        if self.item_count == 0 || self.viewport_height == 0 {
            self.scroll = 0;
            return;
        }
        self.scroll = ListViewport::new(
            self.item_count,
            Some(self.selection),
            self.viewport_height,
            self.scroll,
        )
        .scroll_to_selected();
    }

    pub fn viewport(&self) -> ListViewport {
        ListViewport::new(
            self.item_count,
            (!self.is_empty()).then_some(self.selection),
            self.viewport_height,
            self.scroll,
        )
    }

    pub fn visible_range(&self) -> Range<usize> {
        self.viewport().visible_range()
    }
}

#[cfg(test)]
mod tests {
    //! These tests pin the contract every host inherits. If you change
    //! the semantics of one host (e.g. file explorer should wrap now),
    //! you change the `WrapBehavior` passed in — not the impl — so the
    //! other hosts stay where they were.
    use super::*;

    fn nav(item_count: usize, viewport: usize) -> ListNav {
        let mut n = ListNav::new();
        n.set_item_count(item_count);
        n.set_viewport_height(viewport);
        n
    }

    // ---------- Empty list -----------------------------------------

    #[test]
    fn empty_list_move_is_at_boundary() {
        let mut n = nav(0, 10);
        assert_eq!(n.move_by(1, WrapBehavior::Wrap), NavOutcome::AtBoundary);
        assert_eq!(n.move_by(-1, WrapBehavior::Clamp), NavOutcome::AtBoundary);
        assert_eq!(n.to_first(), NavOutcome::AtBoundary);
        assert_eq!(n.to_last(), NavOutcome::AtBoundary);
        assert_eq!(n.selection(), 0);
        assert_eq!(n.scroll(), 0);
    }

    #[test]
    fn empty_list_after_set_item_count_zero_resets_state() {
        let mut n = nav(10, 5);
        n.move_by(7, WrapBehavior::Clamp);
        assert_eq!(n.selection(), 7);
        n.set_item_count(0);
        assert_eq!(n.selection(), 0);
        assert_eq!(n.scroll(), 0);
    }

    // ---------- Clamp behavior -------------------------------------

    #[test]
    fn clamp_at_first_stays_at_first() {
        let mut n = nav(10, 5);
        // selection starts at 0
        assert_eq!(n.move_by(-1, WrapBehavior::Clamp), NavOutcome::AtBoundary);
        assert_eq!(n.selection(), 0);
    }

    #[test]
    fn clamp_at_last_stays_at_last() {
        let mut n = nav(10, 5);
        n.to_last();
        assert_eq!(n.selection(), 9);
        assert_eq!(n.move_by(1, WrapBehavior::Clamp), NavOutcome::AtBoundary);
        assert_eq!(n.selection(), 9);
    }

    #[test]
    fn clamp_large_jump_clips_to_last() {
        let mut n = nav(10, 5);
        assert_eq!(n.move_by(100, WrapBehavior::Clamp), NavOutcome::Moved);
        assert_eq!(n.selection(), 9);
    }

    // ---------- Wrap behavior --------------------------------------

    #[test]
    fn wrap_at_last_wraps_to_first() {
        let mut n = nav(10, 5);
        n.to_last();
        assert_eq!(n.move_by(1, WrapBehavior::Wrap), NavOutcome::Moved);
        assert_eq!(n.selection(), 0);
    }

    #[test]
    fn wrap_at_first_wraps_to_last() {
        let mut n = nav(10, 5);
        assert_eq!(n.move_by(-1, WrapBehavior::Wrap), NavOutcome::Moved);
        assert_eq!(n.selection(), 9);
    }

    #[test]
    fn wrap_large_negative_wraps_consistently() {
        // -12 from selection 0 on a list of 10 should land at... 8.
        // -12 mod 10 (Euclidean) = 8 (since -12 = -2*10 + 8).
        let mut n = nav(10, 5);
        n.move_by(-12, WrapBehavior::Wrap);
        assert_eq!(n.selection(), 8);
    }

    // ---------- Page sizes -----------------------------------------

    #[test]
    fn full_viewport_page_jumps_by_viewport_height() {
        let mut n = nav(100, 10);
        n.page_by(1, PageSize::FullViewport, WrapBehavior::Clamp);
        assert_eq!(n.selection(), 10);
    }

    #[test]
    fn half_viewport_page_jumps_by_half() {
        let mut n = nav(100, 10);
        n.page_by(1, PageSize::HalfViewport, WrapBehavior::Clamp);
        assert_eq!(n.selection(), 5);
    }

    #[test]
    fn half_viewport_page_with_odd_height_rounds_down() {
        let mut n = nav(100, 7);
        n.page_by(1, PageSize::HalfViewport, WrapBehavior::Clamp);
        assert_eq!(n.selection(), 3, "7/2 = 3 (floor)");
    }

    #[test]
    fn fixed_page_size_honors_count() {
        let mut n = nav(100, 10);
        n.page_by(1, PageSize::Fixed(3), WrapBehavior::Clamp);
        assert_eq!(n.selection(), 3);
    }

    #[test]
    fn page_size_minimum_one() {
        // Even with viewport 0 or fixed 0, page advances by at least one.
        let mut n = nav(10, 0);
        n.page_by(1, PageSize::HalfViewport, WrapBehavior::Clamp);
        assert_eq!(n.selection(), 1);

        let mut n = nav(10, 5);
        n.page_by(1, PageSize::Fixed(0), WrapBehavior::Clamp);
        assert_eq!(n.selection(), 1);
    }

    // ---------- Scroll tracking ------------------------------------

    #[test]
    fn scroll_pulls_when_selection_moves_off_bottom() {
        let mut n = nav(20, 5);
        n.move_by(10, WrapBehavior::Clamp);
        assert_eq!(n.selection(), 10);
        // selection 10 with viewport 5 → scroll should put 10 at bottom:
        // scroll = 10 - (5 - 1) = 6
        assert_eq!(n.scroll(), 6);
    }

    #[test]
    fn scroll_pulls_when_selection_moves_off_top() {
        let mut n = nav(20, 5);
        n.move_by(10, WrapBehavior::Clamp);
        // Now scrolled to 6, selection 10.
        n.move_by(-9, WrapBehavior::Clamp);
        // selection 1 < scroll 6, so scroll should snap up to 1.
        assert_eq!(n.selection(), 1);
        assert_eq!(n.scroll(), 1);
    }

    #[test]
    fn scroll_stays_when_selection_inside_viewport() {
        let mut n = nav(20, 5);
        n.move_by(3, WrapBehavior::Clamp);
        // selection 3, viewport 5, scroll 0 — selection visible, scroll stays.
        assert_eq!(n.scroll(), 0);
    }

    #[test]
    fn set_item_count_clamps_selection_into_range() {
        let mut n = nav(100, 5);
        n.move_by(50, WrapBehavior::Clamp);
        n.set_item_count(10);
        assert_eq!(n.selection(), 9);
    }

    #[test]
    fn ensure_visible_clamps_scroll_when_list_shrinks_below_viewport() {
        let mut n = nav(20, 5);
        n.move_by(15, WrapBehavior::Clamp);
        n.set_item_count(3);
        n.ensure_visible();
        // 3 items, viewport 5 → can show everything starting at scroll 0.
        assert_eq!(n.scroll(), 0);
        assert_eq!(n.selection(), 2);
    }

    #[test]
    fn list_viewport_scrolls_down_to_keep_selection_visible() {
        let viewport = ListViewport::new(20, Some(9), 5, 0);

        assert_eq!(viewport.scroll_to_selected(), 5);
        assert_eq!(viewport.selected_visible_range(), 5..10);
    }

    #[test]
    fn list_viewport_clamps_overscroll_to_last_page() {
        let viewport = ListViewport::new(8, None, 3, 99);

        assert_eq!(viewport.clamped_scroll(), 5);
        assert_eq!(viewport.visible_range(), 5..8);
    }

    // ---------- to_first / to_last ---------------------------------

    #[test]
    fn to_first_from_middle_moves_to_zero() {
        let mut n = nav(10, 5);
        n.move_by(5, WrapBehavior::Clamp);
        assert_eq!(n.to_first(), NavOutcome::Moved);
        assert_eq!(n.selection(), 0);
        assert_eq!(n.scroll(), 0);
    }

    #[test]
    fn to_last_from_middle_moves_to_end() {
        let mut n = nav(10, 5);
        assert_eq!(n.to_last(), NavOutcome::Moved);
        assert_eq!(n.selection(), 9);
    }

    #[test]
    fn to_first_when_already_first_is_at_boundary() {
        let mut n = nav(10, 5);
        assert_eq!(n.to_first(), NavOutcome::AtBoundary);
    }

    #[test]
    fn to_last_when_already_last_is_at_boundary() {
        let mut n = nav(10, 5);
        n.to_last();
        assert_eq!(n.to_last(), NavOutcome::AtBoundary);
    }

    // ---------- set_selection --------------------------------------

    #[test]
    fn set_selection_clamps_to_range() {
        let mut n = nav(10, 5);
        n.set_selection(100);
        assert_eq!(n.selection(), 9);
    }

    #[test]
    fn set_selection_to_same_index_reports_at_boundary() {
        let mut n = nav(10, 5);
        n.set_selection(3);
        assert_eq!(n.set_selection(3), NavOutcome::AtBoundary);
    }

    // ---------- Property: state invariants -------------------------

    /// Whatever the host does to a `ListNav`, the selection is always
    /// in range and the visible window contains the selection. This
    /// is the property that makes the abstraction safe to embed.
    #[test]
    fn invariants_hold_through_random_operations() {
        let mut n = nav(100, 7);
        let operations: &[fn(&mut ListNav)] = &[
            |n| {
                n.move_by(3, WrapBehavior::Clamp);
            },
            |n| {
                n.move_by(-5, WrapBehavior::Wrap);
            },
            |n| {
                n.page_by(1, PageSize::FullViewport, WrapBehavior::Clamp);
            },
            |n| {
                n.page_by(-1, PageSize::HalfViewport, WrapBehavior::Wrap);
            },
            |n| {
                n.to_first();
            },
            |n| {
                n.to_last();
            },
            |n| {
                n.set_item_count(50);
            },
            |n| {
                n.set_item_count(200);
            },
            |n| {
                n.set_viewport_height(3);
            },
            |n| {
                n.set_viewport_height(20);
            },
        ];

        // Iterate operations in a deterministic order, asserting
        // invariants after each.
        for (i, op) in operations.iter().cycle().take(40).enumerate() {
            op(&mut n);
            if n.item_count() > 0 {
                assert!(
                    n.selection() < n.item_count(),
                    "iteration {i}: selection {} >= item_count {}",
                    n.selection(),
                    n.item_count(),
                );
                let viewport_bottom = n.scroll().saturating_add(n.viewport_height().max(1));
                if n.viewport_height() > 0 {
                    assert!(
                        n.selection() >= n.scroll() && n.selection() < viewport_bottom,
                        "iteration {i}: selection {} outside viewport {}..{}",
                        n.selection(),
                        n.scroll(),
                        viewport_bottom,
                    );
                }
            } else {
                assert_eq!(n.selection(), 0, "iteration {i}: empty list selection");
                assert_eq!(n.scroll(), 0, "iteration {i}: empty list scroll");
            }
        }
    }
}
