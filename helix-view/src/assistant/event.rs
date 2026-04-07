use super::{backend, context, permission, thread};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Thread {
        thread: thread::Id,
        event: thread::Event,
    },
    ContextResolved {
        thread: thread::Id,
        item: context::Kind,
    },
    ContextResolveFailed {
        thread: thread::Id,
        error: String,
    },
    Permission {
        thread: thread::Id,
        request: permission::Request,
    },
    Backend {
        backend: backend::Id,
        event: backend::Event,
    },
}
