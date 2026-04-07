use std::{collections::HashMap, num::NonZeroU64, sync::Arc};

use helix_core::completion::CompletionProvider;
use helix_runtime::{send_blocking, Token};

use crate::{document::SavePoint, DocumentId, ViewId};

use helix_runtime::Sender;

pub struct CompletionHandler {
    event_tx: Sender<CompletionEvent>,
    pub active_completions: HashMap<CompletionProvider, ResponseContext>,
    current_request: Option<(RequestId, Token)>,
    next_request: u64,
}

impl CompletionHandler {
    pub fn new(event_tx: Sender<CompletionEvent>) -> Self {
        Self {
            event_tx,
            active_completions: HashMap::new(),
            current_request: None,
            next_request: 1,
        }
    }

    pub fn event(&self, event: CompletionEvent) {
        send_blocking(&self.event_tx, event);
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
