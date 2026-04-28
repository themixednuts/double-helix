use std::num::NonZeroUsize;
use std::sync::atomic::{self, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_runtime::{send_blocking, Clock, Debounce, FrameHandle, Runtime, Sender, Work};

use crate::{Document, DocumentId, ViewId};

#[derive(Debug)]
pub enum DiagnosticEvent {
    CursorLineChanged { generation: usize },
    Refresh,
    FlushDebounced,
}

struct DiagnosticTimeout {
    active_generation: Arc<AtomicUsize>,
    pending_generation: Option<usize>,
    debounce: Debounce,
    work: Work,
    clock: Clock,
    tx: Sender<DiagnosticEvent>,
    redraw: FrameHandle,
}

const TIMEOUT: Duration = Duration::from_millis(350);

impl DiagnosticTimeout {
    fn spawn(
        active_generation: Arc<AtomicUsize>,
        work: Work,
        clock: Clock,
        redraw: FrameHandle,
    ) -> Sender<DiagnosticEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let mut timeout = Self {
            active_generation,
            pending_generation: None,
            debounce: Debounce::new(TIMEOUT),
            work,
            clock,
            tx: tx.clone(),
            redraw,
        };
        timeout
            .work
            .clone()
            .spawn(async move {
                while let Some(event) = rx.recv().await {
                    timeout.handle_event(event);
                }
                timeout.debounce.cancel();
            })
            .detach();
        tx
    }

    fn handle_event(&mut self, event: DiagnosticEvent) {
        match event {
            DiagnosticEvent::CursorLineChanged { generation } => {
                if self
                    .pending_generation
                    .is_none_or(|pending| generation > pending)
                {
                    self.pending_generation = Some(generation);
                    self.restart();
                }
            }
            DiagnosticEvent::Refresh => {
                if self.pending_generation.is_some() {
                    self.restart();
                }
            }
            DiagnosticEvent::FlushDebounced => self.commit_pending_generation(),
        }
    }

    fn restart(&mut self) {
        let tx = self.tx.clone();
        self.debounce.restart(&self.work, &self.clock, async move {
            let _ = tx.send(DiagnosticEvent::FlushDebounced).await;
        });
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
    events: Mutex<Option<Sender<DiagnosticEvent>>>,
    runtime: Mutex<Option<DiagnosticRuntime>>,
}

#[derive(Clone)]
struct DiagnosticRuntime {
    runtime: Runtime,
    redraw: FrameHandle,
}

// make sure we never share handlers across multiple views this is a stop
// gap solution. We just shouldn't be cloneing a view to begin with (we do
// for :hsplit/vsplit) and really this should not be view specific to begin with
// but to fix that larger architecutre changes are needed
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
            events: Mutex::new(None),
            runtime: Mutex::new(None),
            // usize::MAX encodes a "no document" sentinel.
            last_doc: AtomicUsize::new(usize::MAX),
            last_cursor_line: AtomicUsize::new(usize::MAX),
            active: true,
        }
    }

    pub fn bind_runtime(&mut self, runtime: Runtime, redraw: FrameHandle) {
        *self
            .runtime
            .lock()
            .expect("diagnostics runtime lock poisoned") =
            Some(DiagnosticRuntime { runtime, redraw });
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

    fn events(&self) -> Option<Sender<DiagnosticEvent>> {
        let mut events = self
            .events
            .lock()
            .expect("diagnostics handler lock poisoned");
        if let Some(tx) = events.as_ref() {
            return Some(tx.clone());
        }
        let runtime = self
            .runtime
            .lock()
            .expect("diagnostics runtime lock poisoned")
            .as_ref()?
            .clone();
        let tx = DiagnosticTimeout::spawn(
            self.active_generation.clone(),
            runtime.runtime.work().clone(),
            runtime.runtime.clock().clone(),
            runtime.redraw,
        );
        *events = Some(tx.clone());
        Some(tx)
    }
}

impl DiagnosticsHandler {
    pub fn refresh(&self) {
        if let Some(events) = self.events() {
            send_blocking(&events, DiagnosticEvent::Refresh);
        }
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
            if let Some(events) = self.events() {
                send_blocking(
                    &events,
                    DiagnosticEvent::CursorLineChanged {
                        generation: new_gen,
                    },
                );
            }
            false
        }
    }
}
