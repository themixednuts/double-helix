use std::path::PathBuf;

use super::{auth, backend, context, permission, thread};

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
    ReviewAcceptedFile {
        thread: thread::Id,
        path: PathBuf,
        text: String,
    },
    Permission {
        thread: thread::Id,
        request: permission::Request,
    },
    Auth {
        thread: thread::Id,
        event: auth::Event,
    },
    Backend {
        backend: backend::Id,
        event: backend::Event,
    },
}
