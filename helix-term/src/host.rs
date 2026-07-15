//! Terminal-local UI host types and implementation.

use std::time::Duration;

use helix_runtime::{FrameHandle, Runtime};
use helix_view::graphics::Rect;

use crate::compositor::Compositor;

/// Mark a region, or the whole surface, as needing redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalidation {
    Full,
    Rect(Rect),
}

/// Timer id for UI scheduling; shared with [`helix_runtime::Clock`].
pub type TimerId = helix_runtime::TimerId;

/// Host-side UI effects needed by the terminal compositor.
pub trait UiHost {
    fn invalidate(&mut self, area: Invalidation);
    fn request_timer(&mut self, id: TimerId, after: Duration);
}

/// Terminal host for platform effects.
///
/// Invalidation triggers a compositor full-redraw + async redraw request so the
/// application's render loop picks it up. Timers use [`helix_runtime::Clock`]
/// and deliver expiry through the typed application ingress.
pub struct TermHost<'a> {
    pub compositor: &'a mut Compositor,
    runtime: &'a Runtime,
    ingress: crate::runtime::RuntimeIngress,
    redraw: FrameHandle,
    timers: std::collections::HashMap<TimerId, helix_runtime::Task<()>>,
}

impl<'a> TermHost<'a> {
    pub fn new(
        compositor: &'a mut Compositor,
        runtime: &'a Runtime,
        ingress: crate::runtime::RuntimeIngress,
        redraw: FrameHandle,
    ) -> Self {
        Self {
            compositor,
            runtime,
            ingress,
            redraw,
            timers: std::collections::HashMap::new(),
        }
    }
}

impl UiHost for TermHost<'_> {
    fn invalidate(&mut self, area: Invalidation) {
        match area {
            Invalidation::Full => {
                self.compositor.need_full_redraw();
                self.redraw.request_redraw();
            }
            Invalidation::Rect(_rect) => {
                // Terminal backend redraws the whole screen; treat rect as full.
                // A future GPU/partial-damage backend could use the rect.
                self.compositor.need_full_redraw();
                self.redraw.request_redraw();
            }
        }
    }

    fn request_timer(&mut self, id: TimerId, after: Duration) {
        let ingress = self.ingress.clone();
        let timer_task = self.runtime.clock().timer(after);
        let task = self.runtime.work().spawn(async move {
            if timer_task.await.is_ok() {
                ingress.send_timer(id).await;
            }
        });
        self.timers.insert(id, task);
    }
}
