use std::{collections::HashMap, num::NonZeroU64, sync::Arc};

use helix_core::completion::CompletionProvider;
use helix_runtime::{Runtime, Sender, Token, Work};

use super::{CoalescingState, EventRelay};
use crate::{document::SavePoint, DocumentId, ViewId};

pub struct CompletionHandler {
    events: EventRelay<CompletionEventState>,
    pub active_completions: HashMap<CompletionProvider, ResponseContext>,
    current_request: Option<(RequestId, Token)>,
    next_request: u64,
}

impl CompletionHandler {
    pub fn new(event_tx: Sender<CompletionEvent>) -> Self {
        match Runtime::current() {
            Ok(runtime) => Self::spawn(runtime.work().clone(), event_tx),
            Err(_) => Self::disconnected(event_tx),
        }
    }

    pub fn spawn(work: Work, event_tx: Sender<CompletionEvent>) -> Self {
        Self::with_events(EventRelay::spawn(work, event_tx))
    }

    pub(super) fn disconnected(event_tx: Sender<CompletionEvent>) -> Self {
        Self::with_events(EventRelay::disconnected(event_tx))
    }

    fn with_events(events: EventRelay<CompletionEventState>) -> Self {
        Self {
            events,
            active_completions: HashMap::new(),
            current_request: None,
            next_request: 1,
        }
    }

    pub fn event(&self, event: CompletionEvent) {
        self.events.send(event);
    }

    pub fn begin_request(&mut self) -> (RequestId, Token) {
        self.cancel_request();
        let id = RequestId::new(NonZeroU64::new(self.next_request).expect("non-zero request id"));
        self.next_request += 1;
        let token = Token::new();
        self.current_request = Some((id, token.clone()));
        (id, token)
    }

    pub fn cancel_request(&mut self) {
        if let Some((_, token)) = self.current_request.take() {
            token.cancel();
        }
    }

    pub fn is_current(&self, id: RequestId) -> bool {
        self.current_request
            .as_ref()
            .is_some_and(|(current, token)| *current == id && !token.is_canceled())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(NonZeroU64);

impl RequestId {
    #[must_use]
    pub const fn new(id: NonZeroU64) -> Self {
        Self(id)
    }
}

pub struct ResponseContext {
    /// Whether the completion response is marked as "incomplete."
    ///
    /// This is used by LSP. When completions are "incomplete" and you continue typing, the
    /// completions should be recomputed by the server instead of filtered.
    pub is_incomplete: bool,
    pub priority: i8,
    pub savepoint: Arc<SavePoint>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CompletionEvent {
    /// Auto completion was triggered by typing a word char
    AutoTrigger {
        cursor: usize,
        doc: DocumentId,
        view: ViewId,
    },
    /// Auto completion was triggered by typing a trigger char
    /// specified by the LSP
    TriggerChar {
        cursor: usize,
        doc: DocumentId,
        view: ViewId,
    },
    /// A completion was manually requested (c-x)
    ManualTrigger {
        cursor: usize,
        doc: DocumentId,
        view: ViewId,
    },
    /// Some text was deleted and the cursor is now at `pos`
    DeleteText { cursor: usize },
    /// Invalidate the current auto completion trigger
    Cancel,
}

impl CompletionEvent {
    fn trigger_cursor(&self) -> Option<usize> {
        match self {
            Self::AutoTrigger { cursor, .. }
            | Self::TriggerChar { cursor, .. }
            | Self::ManualTrigger { cursor, .. } => Some(*cursor),
            Self::DeleteText { .. } | Self::Cancel => None,
        }
    }
}

#[derive(Debug)]
enum CompletionReset {
    Delete { cursor: usize },
    Cancel,
}

impl CompletionReset {
    fn into_event(self) -> CompletionEvent {
        match self {
            Self::Delete { cursor } => CompletionEvent::DeleteText { cursor },
            Self::Cancel => CompletionEvent::Cancel,
        }
    }
}

enum CompletionDelivery {
    Reset,
    Trigger,
}

#[derive(Default)]
struct CompletionEventState {
    sequence: u64,
    reset: Option<(u64, CompletionReset)>,
    trigger: Option<(u64, CompletionEvent)>,
    in_flight: Option<CompletionDelivery>,
}

impl CompletionEventState {
    fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        self.sequence
    }

    fn merge_delete(&mut self, sequence: u64, cursor: usize) {
        match &mut self.reset {
            Some((_, CompletionReset::Cancel)) => {}
            Some((_, CompletionReset::Delete { cursor: pending })) => {
                *pending = (*pending).min(cursor);
            }
            None => self.reset = Some((sequence, CompletionReset::Delete { cursor })),
        }
    }
}

impl CoalescingState for CompletionEventState {
    type Event = CompletionEvent;
    type Delivery = CompletionDelivery;

    fn push(&mut self, event: Self::Event) {
        let sequence = self.next_sequence();
        match event {
            CompletionEvent::Cancel => {
                self.reset = Some((sequence, CompletionReset::Cancel));
                self.trigger = None;
            }
            CompletionEvent::DeleteText { cursor } => {
                let cancels_pending_trigger = self
                    .trigger
                    .as_ref()
                    .and_then(|(_, trigger)| trigger.trigger_cursor())
                    .is_some_and(|trigger_cursor| cursor < trigger_cursor);
                if cancels_pending_trigger {
                    self.trigger = None;
                }
                if self.trigger.is_none() {
                    self.merge_delete(sequence, cursor);
                }
            }
            trigger => self.trigger = Some((sequence, trigger)),
        }
    }

    fn begin_delivery(&mut self) -> Option<(Self::Event, Self::Delivery)> {
        if self.in_flight.is_some() {
            return None;
        }
        let reset_sequence = self
            .reset
            .as_ref()
            .map_or(u64::MAX, |(sequence, _)| *sequence);
        let trigger_sequence = self
            .trigger
            .as_ref()
            .map_or(u64::MAX, |(sequence, _)| *sequence);

        if reset_sequence <= trigger_sequence {
            let (_, reset) = self.reset.take()?;
            self.in_flight = Some(CompletionDelivery::Reset);
            Some((reset.into_event(), CompletionDelivery::Reset))
        } else {
            let (_, trigger) = self.trigger.take()?;
            self.in_flight = Some(CompletionDelivery::Trigger);
            Some((trigger, CompletionDelivery::Trigger))
        }
    }

    fn finish_delivery(&mut self, _delivery: Self::Delivery) {
        self.in_flight = None;
    }

    fn clear(&mut self) {
        self.reset = None;
        self.trigger = None;
        self.in_flight = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_runtime::test::RuntimeTest;

    #[test]
    fn saturated_completion_events_reduce_to_cancel_then_latest_manual_trigger() {
        let rt = RuntimeTest::default();
        let (tx, mut rx) = helix_runtime::channel(1);
        tx.try_send(CompletionEvent::Cancel).unwrap();
        let handler = CompletionHandler::spawn(rt.runtime().work().clone(), tx);
        let doc = DocumentId::default();
        let view = ViewId::default();

        for cursor in 1..=10_000 {
            handler.event(CompletionEvent::AutoTrigger { cursor, doc, view });
        }
        handler.event(CompletionEvent::Cancel);
        handler.event(CompletionEvent::ManualTrigger {
            cursor: 42,
            doc,
            view,
        });

        rt.block_on(async {
            assert_eq!(rx.recv().await, Some(CompletionEvent::Cancel));
            assert_eq!(rx.recv().await, Some(CompletionEvent::Cancel));
            assert_eq!(
                rx.recv().await,
                Some(CompletionEvent::ManualTrigger {
                    cursor: 42,
                    doc,
                    view,
                })
            );
        });
        assert!(matches!(
            rx.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
    }

    #[test]
    fn deletion_before_pending_trigger_reduces_to_one_reset() {
        let mut state = CompletionEventState::default();
        let doc = DocumentId::default();
        let view = ViewId::default();
        state.push(CompletionEvent::AutoTrigger {
            cursor: 20,
            doc,
            view,
        });
        state.push(CompletionEvent::DeleteText { cursor: 10 });
        state.push(CompletionEvent::DeleteText { cursor: 5 });

        let (event, delivery) = state.begin_delivery().expect("pending reset");
        assert_eq!(event, CompletionEvent::DeleteText { cursor: 5 });
        state.finish_delivery(delivery);
        assert!(state.begin_delivery().is_none());
    }
}
