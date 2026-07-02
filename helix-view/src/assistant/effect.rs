use crate::collab::Location;

use std::path::PathBuf;

use super::{backend, history, thread};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    EnsureParticipant {
        thread: thread::Id,
    },
    LeaveParticipant {
        thread: thread::Id,
    },
    PublishLocation {
        thread: thread::Id,
        location: Location,
    },
    RevealLocation {
        location: Location,
    },
    SendBackendCommand {
        backend: backend::Id,
        command: backend::Command,
    },
    OpenEntryDoc {
        thread: thread::Id,
        entry: thread::EntryId,
        action: crate::editor::Action,
    },
    ApplyReviewAcceptedFile {
        thread: thread::Id,
        path: PathBuf,
        text: String,
    },
    SetStatus {
        message: String,
    },
    Save {
        thread: thread::Id,
    },
    SaveNow {
        record: Box<history::Record>,
    },
    Delete {
        thread: thread::Id,
    },
    SyncModel,
}
