use std::{collections::HashMap, time::Instant};

use crate::{PulseGate, PulseHandle, PulseReceiver, PulseRequest, TryRecvError};

/// Stable identity for a producer that can invalidate a frame.
///
/// Sources make scheduled animation frames replaceable and cancellable. Immediate
/// invalidations from the same source still coalesce into a single pending frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameSource {
    name: &'static str,
}

impl FrameSource {
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }

    pub const fn name(self) -> &'static str {
        self.name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameGeneration(u64);

/// Application-owned frame invalidation state.
///
/// This type deliberately contains no task or timer. The host polls
/// [`next_deadline`](Self::next_deadline) with one timer and brackets rendering with
/// [`begin_frame`](Self::begin_frame) / [`end_frame`](Self::end_frame). That keeps
/// rendering single-owner while preserving invalidations raised during a frame.
#[derive(Debug)]
pub struct FrameScheduler {
    sequence: u64,
    dirty: HashMap<FrameSource, u64>,
    deadlines: HashMap<FrameSource, Instant>,
    in_flight: Option<FrameGeneration>,
}

impl Default for FrameScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameScheduler {
    pub fn new() -> Self {
        Self {
            sequence: 0,
            dirty: HashMap::new(),
            deadlines: HashMap::new(),
            in_flight: None,
        }
    }

    pub fn invalidate(&mut self, source: FrameSource) {
        let generation = self.advance_sequence();
        self.dirty.insert(source, generation);
    }

    /// Replace the next requested frame for `source`.
    pub fn invalidate_at(&mut self, source: FrameSource, deadline: Instant) {
        self.deadlines.insert(source, deadline);
    }

    pub fn cancel(&mut self, source: FrameSource) {
        self.dirty.remove(&source);
        self.deadlines.remove(&source);
    }

    /// Replace render-driven deadlines as one snapshot.
    ///
    /// Components report active animations on every render. Replacing the complete
    /// set automatically cancels deadlines from components that are no longer active
    /// or have been removed.
    pub fn replace_deadlines(
        &mut self,
        deadlines: impl IntoIterator<Item = (FrameSource, Instant)>,
    ) {
        self.deadlines.clear();
        for (source, deadline) in deadlines {
            self.deadlines
                .entry(source)
                .and_modify(|current| *current = (*current).min(deadline))
                .or_insert(deadline);
        }
    }

    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let has_due_work =
            !self.dirty.is_empty() || self.deadlines.values().any(|deadline| *deadline <= now);

        if has_due_work {
            return Some(now);
        }

        self.deadlines.values().copied().min()
    }

    pub fn begin_frame(&mut self, now: Instant) -> Option<FrameGeneration> {
        if self.in_flight.is_some() {
            return None;
        }

        self.promote_due_deadlines(now);
        if self.dirty.is_empty() {
            return None;
        }

        let generation = FrameGeneration(self.sequence);
        self.in_flight = Some(generation);
        Some(generation)
    }

    pub fn end_frame(&mut self, generation: FrameGeneration) {
        let Some(in_flight_generation) = self.in_flight.take() else {
            panic!("frame generation must be completed by its owner");
        };
        assert_eq!(in_flight_generation, generation);
        self.dirty
            .retain(|_, invalidated_at| *invalidated_at > generation.0);
    }

    pub fn has_pending_frame(&self, now: Instant) -> bool {
        !self.dirty.is_empty() || self.deadlines.values().any(|deadline| *deadline <= now)
    }

    fn advance_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1);
        if self.sequence == 0 {
            self.sequence = 1;
        }
        self.sequence
    }

    fn promote_due_deadlines(&mut self, now: Instant) {
        if !self.deadlines.iter().any(|(_, deadline)| *deadline <= now) {
            return;
        }

        let generation = self.advance_sequence();
        let dirty = &mut self.dirty;
        self.deadlines.retain(|source, deadline| {
            let keep = *deadline > now;
            if !keep {
                dirty.insert(*source, generation);
            }
            keep
        });
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FramePulse {}

#[derive(Debug, PartialEq, Eq)]
pub struct FrameRequest(PulseRequest<FramePulse>);

#[derive(Debug)]
pub struct FrameGate(PulseGate<FramePulse>);

#[derive(Clone, Debug)]
pub struct FrameHandle(PulseHandle<FramePulse>);

#[derive(Debug)]
pub struct FrameReceiver(PulseReceiver<FramePulse>);

impl FrameGate {
    pub fn new() -> Self {
        Self(PulseGate::new())
    }

    pub fn handle(&self) -> FrameHandle {
        FrameHandle(self.0.handle())
    }

    pub fn request_redraw(&self) {
        self.0.request();
    }

    pub fn take_receiver(&mut self) -> FrameReceiver {
        FrameReceiver(self.0.take_receiver())
    }
}

impl FrameHandle {
    pub fn request_redraw(&self) {
        self.0.request();
    }

    pub async fn request_redraw_async(&self) -> Result<(), crate::Closed<()>> {
        self.0.request_async().await
    }
}

impl FrameReceiver {
    pub async fn recv(&mut self) -> Option<FrameRequest> {
        self.0.recv().await.map(FrameRequest)
    }

    pub fn try_recv(&mut self) -> Result<FrameRequest, TryRecvError> {
        self.0.try_recv().map(FrameRequest)
    }
}

impl futures_util::Stream for FrameReceiver {
    type Item = FrameRequest;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match std::pin::Pin::new(&mut self.0).poll_next(cx) {
            std::task::Poll::Ready(Some(request)) => {
                std::task::Poll::Ready(Some(FrameRequest(request)))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::RuntimeTest;
    use std::time::Duration;

    #[test]
    fn frame_gate_delivers_requests() {
        let rt = RuntimeTest::default();
        let mut gate = FrameGate::new();
        let mut rx = gate.take_receiver();

        gate.request_redraw();

        rt.block_on(async {
            assert!(rx.recv().await.is_some());
        });
    }

    #[test]
    fn frame_gate_coalesces_pending_requests() {
        let mut gate = FrameGate::new();
        let mut rx = gate.take_receiver();

        gate.request_redraw();
        gate.request_redraw();

        assert!(rx.try_recv().is_ok());
        assert!(matches!(rx.try_recv(), Err(crate::TryRecvError::Empty)));
    }

    #[test]
    fn frame_gate_accepts_new_request_after_consumption() {
        let mut gate = FrameGate::new();
        let mut rx = gate.take_receiver();

        gate.request_redraw();
        assert!(rx.try_recv().is_ok());

        gate.request_redraw();
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn frame_handle_async_request_delivers() {
        let rt = RuntimeTest::default();
        let mut gate = FrameGate::new();
        let handle = gate.handle();
        let mut rx = gate.take_receiver();

        rt.block_on(async {
            handle.request_redraw_async().await.unwrap();
            assert!(rx.recv().await.is_some());
        });
    }

    const INPUT: FrameSource = FrameSource::new("input");
    const ANIMATION: FrameSource = FrameSource::new("animation");
    const PICKER: FrameSource = FrameSource::new("picker");

    #[test]
    fn scheduler_coalesces_many_invalidations_into_one_frame() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::new();
        for _ in 0..10_000 {
            scheduler.invalidate(INPUT);
        }

        let generation = scheduler.begin_frame(now).unwrap();
        scheduler.end_frame(generation);

        assert!(!scheduler.has_pending_frame(now));
        assert_eq!(scheduler.begin_frame(now), None);
    }

    #[test]
    fn scheduler_preserves_invalidation_raised_during_frame() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::new();
        scheduler.invalidate(INPUT);

        let first = scheduler.begin_frame(now).unwrap();
        scheduler.invalidate(PICKER);
        scheduler.end_frame(first);

        let second = scheduler.begin_frame(now).unwrap();
        scheduler.end_frame(second);
        assert_eq!(scheduler.begin_frame(now), None);
    }

    #[test]
    fn scheduler_uses_earliest_animation_deadline() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.invalidate_at(INPUT, now + Duration::from_millis(80));
        scheduler.invalidate_at(ANIMATION, now + Duration::from_millis(20));

        assert_eq!(
            scheduler.next_deadline(now),
            Some(now + Duration::from_millis(20))
        );
    }

    #[test]
    fn scheduler_cancel_removes_immediate_and_scheduled_work() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.invalidate(PICKER);
        scheduler.invalidate_at(PICKER, now);
        scheduler.cancel(PICKER);

        assert_eq!(scheduler.next_deadline(now), None);
        assert_eq!(scheduler.begin_frame(now), None);
    }

    #[test]
    fn replacing_render_deadlines_cancels_stale_animation() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.replace_deadlines([(ANIMATION, now + Duration::from_millis(20))]);
        scheduler.replace_deadlines([]);

        assert_eq!(scheduler.next_deadline(now), None);
    }

    #[test]
    fn scheduler_never_delays_new_invalidations() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::new();
        scheduler.invalidate(INPUT);
        let generation = scheduler.begin_frame(now).unwrap();
        scheduler.end_frame(generation);

        scheduler.invalidate(PICKER);
        assert_eq!(scheduler.next_deadline(now), Some(now));
        assert!(scheduler.begin_frame(now).is_some());
    }
}
