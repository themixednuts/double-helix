use std::num::NonZeroUsize;
use std::sync::atomic::{self, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_runtime::{FrameHandle, PulseGate, PulseHandle, Runtime, Work};

use crate::{Document, DocumentId, ViewId};

#[derive(Debug)]
enum DiagnosticEvent {
    CursorLineChanged { generation: usize },
    Refresh,
}

#[derive(Default)]
struct DiagnosticRequests {
    generation: Option<usize>,
    refresh: bool,
}

impl DiagnosticRequests {
    fn push(&mut self, event: DiagnosticEvent) {
        match event {
            DiagnosticEvent::CursorLineChanged { generation } => {
                self.generation = Some(
                    self.generation
                        .map_or(generation, |current| current.max(generation)),
                );
            }
            DiagnosticEvent::Refresh => self.refresh = true,
        }
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }

    fn is_empty(&self) -> bool {
        self.generation.is_none() && !self.refresh
    }
}

enum DiagnosticWake {}

#[derive(Clone)]
struct DiagnosticInbox {
    requests: Arc<Mutex<DiagnosticRequests>>,
    wake: PulseHandle<DiagnosticWake>,
}

impl DiagnosticInbox {
    fn spawn(active_generation: Arc<AtomicUsize>, work: Work, redraw: FrameHandle) -> Self {
        let mut gate = PulseGate::<DiagnosticWake>::new();
        let wake = gate.handle();
        let wake_rx = gate.take_receiver();
        let requests = Arc::new(Mutex::new(DiagnosticRequests::default()));
        let actor_requests = requests.clone();

        work.spawn(async move {
            DiagnosticTimeout::new(active_generation, redraw)
                .run(wake_rx, actor_requests)
                .await;
        })
        .detach();

        Self { requests, wake }
    }

    fn send(&self, event: DiagnosticEvent) {
        self.requests
            .lock()
            .expect("diagnostics request lock poisoned")
            .push(event);
        self.wake.request();
    }
}

struct DiagnosticTimeout {
    active_generation: Arc<AtomicUsize>,
    pending_generation: Option<usize>,
    deadline: Option<tokio::time::Instant>,
    redraw: FrameHandle,
}

const TIMEOUT: Duration = Duration::from_millis(350);

impl DiagnosticTimeout {
    fn new(active_generation: Arc<AtomicUsize>, redraw: FrameHandle) -> Self {
        Self {
            active_generation,
            pending_generation: None,
            deadline: None,
            redraw,
        }
    }

    async fn run(
        mut self,
        mut wake_rx: helix_runtime::PulseReceiver<DiagnosticWake>,
        requests: Arc<Mutex<DiagnosticRequests>>,
    ) {
        loop {
            let Some(deadline) = self.deadline else {
                if wake_rx.recv().await.is_none() {
                    return;
                }
                self.consume_requests(&requests);
                continue;
            };

            tokio::select! {
                biased;
                wake = wake_rx.recv() => {
                    if wake.is_none() {
                        return;
                    }
                    self.consume_requests(&requests);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    self.consume_requests(&requests);
                    if self.deadline.is_some_and(|current| current <= tokio::time::Instant::now()) {
                        self.commit_pending_generation();
                        self.deadline = None;
                    }
                }
            }
        }
    }

    fn consume_requests(&mut self, requests: &Mutex<DiagnosticRequests>) {
        let requests = requests
            .lock()
            .expect("diagnostics request lock poisoned")
            .take();
        if requests.is_empty() {
            return;
        }

        let mut restart = false;
        if let Some(generation) = requests.generation {
            if self
                .pending_generation
                .is_none_or(|pending| generation > pending)
            {
                self.pending_generation = Some(generation);
                restart = true;
            }
        }
        if requests.refresh && self.pending_generation.is_some() {
            restart = true;
        }
        if restart {
            self.deadline = Some(tokio::time::Instant::now() + TIMEOUT);
        }
    }

    fn commit_pending_generation(&mut self) {
        let Some(generation) = self.pending_generation.take() else {
            return;
        };
        if self.active_generation.load(atomic::Ordering::Relaxed) < generation {
            self.active_generation
                .store(generation, atomic::Ordering::Relaxed);
            self.redraw.request_redraw();
        }
    }
}

pub struct DiagnosticsHandler {
    active_generation: Arc<AtomicUsize>,
    generation: AtomicUsize,
    last_doc: AtomicUsize,
    last_cursor_line: AtomicUsize,
    pub active: bool,
    event_source: Mutex<DiagnosticEventSource>,
}

#[derive(Clone)]
struct DiagnosticRuntime {
    runtime: Runtime,
    redraw: FrameHandle,
}

enum DiagnosticEventSource {
    Unbound,
    Bound(DiagnosticRuntime),
    Spawned(DiagnosticInbox),
}

enum DiagnosticEventTarget {
    Unbound,
    Spawned(DiagnosticInbox),
}

impl DiagnosticEventTarget {
    fn send(self, event: DiagnosticEvent) {
        let Self::Spawned(inbox) = self else {
            return;
        };
        inbox.send(event);
    }
}

// Views currently clone this handler during splits. Keep each clone independent
// until diagnostics state moves out of the view entirely.
impl Clone for DiagnosticsHandler {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl DiagnosticsHandler {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            active_generation: Arc::new(AtomicUsize::new(0)),
            generation: AtomicUsize::new(0),
            event_source: Mutex::new(DiagnosticEventSource::Unbound),
            // usize::MAX encodes a "no document" sentinel.
            last_doc: AtomicUsize::new(usize::MAX),
            last_cursor_line: AtomicUsize::new(usize::MAX),
            active: true,
        }
    }

    pub fn bind_runtime(&mut self, runtime: Runtime, redraw: FrameHandle) {
        let mut source = self
            .event_source
            .lock()
            .expect("diagnostics event source lock poisoned");
        if matches!(&*source, DiagnosticEventSource::Spawned(_)) {
            return;
        }
        *source = DiagnosticEventSource::Bound(DiagnosticRuntime { runtime, redraw });
    }

    fn load_last_doc(&self) -> DocumentId {
        let raw = self.last_doc.load(Ordering::Relaxed);
        // Safety: we only store values from DocumentId::value() which are NonZeroUsize,
        // or usize::MAX which is also non-zero.
        DocumentId::new(unsafe { NonZeroUsize::new_unchecked(raw) })
    }

    fn store_last_doc(&self, id: DocumentId) {
        self.last_doc.store(id.value().get(), Ordering::Relaxed);
    }

    fn events(&self) -> DiagnosticEventTarget {
        let mut source = self
            .event_source
            .lock()
            .expect("diagnostics event source lock poisoned");

        match &*source {
            DiagnosticEventSource::Unbound => DiagnosticEventTarget::Unbound,
            DiagnosticEventSource::Spawned(inbox) => DiagnosticEventTarget::Spawned(inbox.clone()),
            DiagnosticEventSource::Bound(runtime) => {
                let inbox = DiagnosticInbox::spawn(
                    self.active_generation.clone(),
                    runtime.runtime.work().clone(),
                    runtime.redraw.clone(),
                );
                *source = DiagnosticEventSource::Spawned(inbox.clone());
                DiagnosticEventTarget::Spawned(inbox)
            }
        }
    }

    pub fn refresh(&self) {
        self.events().send(DiagnosticEvent::Refresh);
    }

    pub fn immediately_show_diagnostic(&self, doc: &Document, view: ViewId) {
        self.store_last_doc(doc.id());
        let cursor_line = doc
            .selection(view)
            .primary()
            .cursor_line(doc.text().slice(..));
        self.last_cursor_line.store(cursor_line, Ordering::Relaxed);
        self.active_generation
            .store(self.generation.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    pub fn show_cursorline_diagnostics(&self, doc: &Document, view: ViewId) -> bool {
        if !self.active {
            return false;
        }
        let cursor_line = doc
            .selection(view)
            .primary()
            .cursor_line(doc.text().slice(..));
        if self.last_cursor_line.load(Ordering::Relaxed) == cursor_line
            && self.load_last_doc() == doc.id()
        {
            let active_generation = self.active_generation.load(Ordering::Relaxed);
            self.generation.load(Ordering::Relaxed) == active_generation
        } else {
            self.store_last_doc(doc.id());
            self.last_cursor_line.store(cursor_line, Ordering::Relaxed);
            let new_gen = self.generation.load(Ordering::Relaxed) + 1;
            self.generation.store(new_gen, Ordering::Relaxed);
            self.events().send(DiagnosticEvent::CursorLineChanged {
                generation: new_gen,
            });
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_runtime::{test::RuntimeTest, FrameGate, TryRecvError};

    #[test]
    fn cursor_generation_coalesces_under_saturation() {
        let rt = RuntimeTest::new_paused();
        let active = Arc::new(AtomicUsize::new(0));
        let mut frames = FrameGate::new();
        let inbox =
            DiagnosticInbox::spawn(active.clone(), rt.runtime().work().clone(), frames.handle());
        let mut frame_rx = frames.take_receiver();

        for generation in 1..=10_000 {
            inbox.send(DiagnosticEvent::CursorLineChanged { generation });
        }
        rt.block_on(tokio::task::yield_now());
        rt.advance(TIMEOUT - Duration::from_millis(1));
        assert_eq!(active.load(Ordering::Relaxed), 0);
        rt.advance(Duration::from_millis(1));

        assert_eq!(active.load(Ordering::Relaxed), 10_000);
        assert!(frame_rx.try_recv().is_ok());
        assert!(matches!(frame_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn refresh_restarts_the_single_pending_deadline() {
        let rt = RuntimeTest::new_paused();
        let active = Arc::new(AtomicUsize::new(0));
        let mut frames = FrameGate::new();
        let inbox =
            DiagnosticInbox::spawn(active.clone(), rt.runtime().work().clone(), frames.handle());
        let _frame_rx = frames.take_receiver();

        inbox.send(DiagnosticEvent::CursorLineChanged { generation: 7 });
        rt.block_on(tokio::task::yield_now());
        rt.advance(Duration::from_millis(300));
        inbox.send(DiagnosticEvent::Refresh);
        rt.block_on(tokio::task::yield_now());
        rt.advance(TIMEOUT - Duration::from_millis(1));
        assert_eq!(active.load(Ordering::Relaxed), 0);
        rt.advance(Duration::from_millis(1));
        assert_eq!(active.load(Ordering::Relaxed), 7);
    }
}
