//! Terminal-local UI host types and implementation.

use std::time::Duration;

use helix_view::graphics::Rect;

use crate::compositor::Compositor;

/// Mark a region, or the whole surface, as needing redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalidation {
    Full,
    Rect(Rect),
}

/// Opaque timer identifier for frontend-side scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u64);

/// Host-side UI effects needed by the terminal compositor.
pub trait UiHost {
    fn invalidate(&mut self, area: Invalidation);
    fn request_timer(&mut self, id: TimerId, after: Duration);
}

/// Terminal host for platform effects.
///
/// Invalidation triggers a compositor full-redraw + async redraw request so the
/// application's render loop picks it up. Timers spawn a tokio task that sleeps
/// for the requested duration, then fires a redraw.
pub struct TermHost<'a> {
    pub compositor: &'a mut Compositor,
}

impl<'a> TermHost<'a> {
    pub fn new(compositor: &'a mut Compositor) -> Self {
        Self { compositor }
    }
}

impl UiHost for TermHost<'_> {
    fn invalidate(&mut self, area: Invalidation) {
        match area {
            Invalidation::Full => {
                self.compositor.need_full_redraw();
                helix_event::request_redraw();
            }
            Invalidation::Rect(_rect) => {
                // Terminal backend redraws the whole screen; treat rect as full.
                // A future GPU/partial-damage backend could use the rect.
                self.compositor.need_full_redraw();
                helix_event::request_redraw();
            }
        }
    }

    fn request_timer(&mut self, _id: TimerId, after: Duration) {
        // Spawn a task that sleeps for the duration then requests a redraw.
        // The editor's event loop sees the redraw request via `helix_event::redraw_requested()`
        // and re-renders, which naturally picks up any state changes the timer was meant to trigger.
        //
        // TimerId is currently unused at the terminal level — the redraw is unconditional.
        // When component-level timer dispatch is needed (e.g. cursor blink, notification
        // auto-dismiss), the application event loop can be extended to deliver TimerId-specific
        // events. The infrastructure (id + spawn + redraw) is in place for that.
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            helix_event::request_redraw();
        });
    }
}
