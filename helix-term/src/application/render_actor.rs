use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use helix_runtime::{LatestAdmissionError, LatestByKeySender};
use helix_view::graphics::{CursorKind, Rect};
use tui::ratatui::buffer::Buffer;

use super::terminal_presenter::PresenterHandle;
use crate::render::{FramePacket, RenderPlan};

pub(super) struct PreparedFrame {
    generation: helix_runtime::FrameGeneration,
    area: Rect,
    cursor: Option<(u16, u16)>,
    cursor_kind: CursorKind,
    full_redraw: bool,
    plan: RenderPlan,
}

impl PreparedFrame {
    pub fn new(
        generation: helix_runtime::FrameGeneration,
        plan: RenderPlan,
        cursor: Option<(u16, u16)>,
        cursor_kind: CursorKind,
        full_redraw: bool,
    ) -> Self {
        let area = plan.area();
        Self {
            generation,
            area,
            cursor,
            cursor_kind,
            full_redraw,
            plan,
        }
    }

    fn preserve_full_redraw(pending: &mut Self, mut newer: Self) {
        newer.full_redraw |= pending.full_redraw;
        *pending = newer;
    }
}

struct QueuedFrame {
    sequence: u64,
    frame: PreparedFrame,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum RenderAdmissionError {
    #[error("render actor is closed")]
    Closed,
    #[error("render actor admission invariant was violated")]
    Full,
}

pub(super) struct RenderActor {
    tx: LatestByKeySender<(), QueuedFrame>,
    next_sequence: AtomicU64,
    latest_sequence: Arc<AtomicU64>,
    occupied: Arc<AtomicBool>,
    presenter: PresenterHandle,
    pipeline_ready: helix_runtime::PulseHandle<super::FramePipelineReady>,
    cancel: helix_runtime::Token,
}

impl RenderActor {
    pub fn spawn(
        work: helix_runtime::Work,
        block: helix_runtime::Block,
        presenter: PresenterHandle,
        pipeline_ready: helix_runtime::PulseHandle<super::FramePipelineReady>,
    ) -> Self {
        let (tx, mut rx) = helix_runtime::latest_by_key::<(), QueuedFrame>(1);
        let latest_sequence = Arc::new(AtomicU64::new(0));
        let actor_latest = latest_sequence.clone();
        let occupied = Arc::new(AtomicBool::new(false));
        let actor_occupied = occupied.clone();
        let actor_presenter = presenter.clone();
        let actor_pipeline_ready = pipeline_ready.clone();
        let cancel = helix_runtime::Token::new();
        let actor_cancel = cancel.clone();

        work.spawn(async move {
            let force_full_redraw = AtomicBool::new(false);
            let mut cache = crate::render::CacheStore::default();
            loop {
                let queued = tokio::select! {
                    biased;
                    _ = actor_cancel.canceled() => break,
                    queued = rx.recv() => match queued {
                        Some(((), queued)) => queued,
                        None => break,
                    },
                };

                let QueuedFrame { sequence, frame } = queued;
                let PreparedFrame {
                    generation,
                    area,
                    cursor,
                    cursor_kind,
                    full_redraw,
                    mut plan,
                } = frame;
                let surface = plan
                    .take_seed()
                    .unwrap_or_else(|| actor_presenter.take_surface(area));
                let cancellation = crate::render::RenderCancellation::for_sequence(
                    Arc::clone(&actor_latest),
                    sequence,
                );
                let result = if plan.is_empty() {
                    plan.execute(surface, &mut cache, &cancellation)
                } else {
                    let mut worker_cache = std::mem::take(&mut cache);
                    match block
                        .spawn(move || {
                            let result = plan.execute(surface, &mut worker_cache, &cancellation);
                            (result, worker_cache)
                        })
                        .await
                    {
                        Ok((result, worker_cache)) => {
                            cache = worker_cache;
                            result
                        }
                        Err(error) => {
                            log::error!("render worker failed sequence={sequence} error={error}");
                            cache = crate::render::CacheStore::default();
                            force_full_redraw.store(true, Ordering::Release);
                            release_if_latest(
                                &actor_latest,
                                &actor_occupied,
                                &actor_pipeline_ready,
                                sequence,
                            );
                            continue;
                        }
                    }
                };

                if !result.complete || actor_latest.load(Ordering::Acquire) != sequence {
                    if full_redraw {
                        force_full_redraw.store(true, Ordering::Release);
                    }
                    actor_presenter.recycle_surface(result.surface);
                    release_if_latest(
                        &actor_latest,
                        &actor_occupied,
                        &actor_pipeline_ready,
                        sequence,
                    );
                    continue;
                }

                let inherited_full_redraw = force_full_redraw.swap(false, Ordering::AcqRel);
                let (cursor, cursor_kind) = result
                    .metadata
                    .cursor_override()
                    .map(|cursor| (cursor.position, cursor.kind))
                    .unwrap_or((cursor, cursor_kind));
                let packet = FramePacket {
                    generation,
                    area,
                    surface: result.surface,
                    cursor,
                    cursor_kind,
                    full_redraw: full_redraw || inherited_full_redraw,
                };
                if let Err(error) = actor_presenter.submit(packet) {
                    log::error!("failed to submit rendered frame: {error}");
                    release_if_latest(
                        &actor_latest,
                        &actor_occupied,
                        &actor_pipeline_ready,
                        sequence,
                    );
                    break;
                }
                release_if_latest(
                    &actor_latest,
                    &actor_occupied,
                    &actor_pipeline_ready,
                    sequence,
                );
            }
        })
        .detach();

        Self {
            tx,
            next_sequence: AtomicU64::new(1),
            latest_sequence,
            occupied,
            presenter,
            pipeline_ready,
            cancel,
        }
    }

    pub fn is_saturated(&self) -> bool {
        self.occupied.load(Ordering::Acquire) || self.presenter.has_pending_frame()
    }

    pub fn take_surface(&self, area: Rect) -> Buffer {
        self.presenter.take_surface(area)
    }

    pub fn submit(&self, frame: PreparedFrame) -> Result<(), RenderAdmissionError> {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let previous = self.latest_sequence.swap(sequence, Ordering::AcqRel);
        let was_occupied = self.occupied.swap(true, Ordering::AcqRel);
        let queued = QueuedFrame { sequence, frame };
        let admitted = self
            .tx
            .try_fold((), queued, |pending, newer| {
                let sequence = newer.sequence;
                PreparedFrame::preserve_full_redraw(&mut pending.frame, newer.frame);
                pending.sequence = sequence;
            })
            .map(|_| ())
            .map_err(|error| match error {
                LatestAdmissionError::Closed((), _) => RenderAdmissionError::Closed,
                LatestAdmissionError::Full((), _) => RenderAdmissionError::Full,
            });
        if admitted.is_err() {
            let _ = self.latest_sequence.compare_exchange(
                sequence,
                previous,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.occupied.store(was_occupied, Ordering::Release);
            if !was_occupied {
                self.pipeline_ready.request();
            }
        }
        admitted
    }
}

fn release_if_latest(
    latest_sequence: &AtomicU64,
    occupied: &AtomicBool,
    pipeline_ready: &helix_runtime::PulseHandle<super::FramePipelineReady>,
    sequence: u64,
) {
    if latest_sequence.load(Ordering::Acquire) == sequence {
        occupied.store(false, Ordering::Release);
        pipeline_ready.request();
    }
}

impl Drop for RenderActor {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_runtime::{FrameScheduler, FrameSource};

    fn generation() -> helix_runtime::FrameGeneration {
        let mut scheduler = FrameScheduler::new();
        scheduler.invalidate(FrameSource::new("render-actor-test"));
        scheduler
            .begin_frame(std::time::Instant::now())
            .expect("test invalidation must produce a frame")
    }

    fn frame(full_redraw: bool) -> PreparedFrame {
        let area = Rect::new(0, 0, 1, 1);
        PreparedFrame::new(
            generation(),
            RenderPlan::seeded(area, Buffer::empty(tui::ratatui::to_ratatui_rect(area))),
            None,
            CursorKind::Hidden,
            full_redraw,
        )
    }

    #[test]
    fn replacement_preserves_full_redraw_intent() {
        let mut pending = frame(true);
        PreparedFrame::preserve_full_redraw(&mut pending, frame(false));
        assert!(pending.full_redraw);
    }

    #[test]
    fn stale_completion_cannot_release_a_newer_render() {
        let latest = AtomicU64::new(2);
        let occupied = AtomicBool::new(true);
        let mut gate = helix_runtime::PulseGate::<super::super::FramePipelineReady>::new();
        let ready = gate.handle();
        let mut ready_rx = gate.take_receiver();

        release_if_latest(&latest, &occupied, &ready, 1);
        assert!(occupied.load(Ordering::Acquire));
        assert!(matches!(
            ready_rx.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));

        release_if_latest(&latest, &occupied, &ready, 2);
        assert!(!occupied.load(Ordering::Acquire));
        assert!(ready_rx.try_recv().is_ok());
    }
}
