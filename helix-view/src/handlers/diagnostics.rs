use std::num::NonZeroUsize;
use std::sync::atomic::{self, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use helix_event::{request_redraw, send_blocking, AsyncHook};
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;

use crate::{Document, DocumentId, ViewId};

#[derive(Debug)]
pub enum DiagnosticEvent {
    CursorLineChanged { generation: usize },
    Refresh,
}

struct DiagnosticTimeout {
    active_generation: Arc<AtomicUsize>,
    generation: usize,
}

const TIMEOUT: Duration = Duration::from_millis(350);

impl AsyncHook for DiagnosticTimeout {
    type Event = DiagnosticEvent;

    fn handle_event(
        &mut self,
        event: DiagnosticEvent,
        timeout: Option<Instant>,
    ) -> Option<Instant> {
        match event {
            DiagnosticEvent::CursorLineChanged { generation } => {
                if generation > self.generation {
                    self.generation = generation;
                    Some(Instant::now() + TIMEOUT)
                } else {
                    timeout
                }
            }
            DiagnosticEvent::Refresh if timeout.is_some() => Some(Instant::now() + TIMEOUT),
            DiagnosticEvent::Refresh => None,
        }
    }

    fn finish_debounce(&mut self) {
        if self.active_generation.load(atomic::Ordering::Relaxed) < self.generation {
            self.active_generation
                .store(self.generation, atomic::Ordering::Relaxed);
            request_redraw();
        }
    }
}

pub struct DiagnosticsHandler {
    active_generation: Arc<AtomicUsize>,
    generation: AtomicUsize,
    last_doc: AtomicUsize,
    last_cursor_line: AtomicUsize,
    pub active: bool,
    pub events: Sender<DiagnosticEvent>,
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
        let active_generation = Arc::new(AtomicUsize::new(0));
        let events = DiagnosticTimeout {
            active_generation: active_generation.clone(),
            generation: 0,
        }
        .spawn();
        Self {
            active_generation,
            generation: AtomicUsize::new(0),
            events,
            // usize::MAX encodes a "no document" sentinel.
            last_doc: AtomicUsize::new(usize::MAX),
            last_cursor_line: AtomicUsize::new(usize::MAX),
            active: true,
        }
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
}

impl DiagnosticsHandler {
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
            send_blocking(
                &self.events,
                DiagnosticEvent::CursorLineChanged {
                    generation: new_gen,
                },
            );
            false
        }
    }
}
