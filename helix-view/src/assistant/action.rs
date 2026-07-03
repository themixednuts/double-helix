use super::{config, context, history, mode, permission, profile, review, thread};
use crate::DocumentId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    NewThread {
        backend: super::backend::Id,
        scope: thread::Scope,
        profile: Option<profile::Defaults>,
    },
    LoadThread {
        record: Box<history::Record>,
        activation: crate::editor::Activation,
    },
    LoadRemoteThread {
        backend: super::backend::Id,
        remote: super::backend::Remote,
        scope: thread::Scope,
        activation: crate::editor::Activation,
    },
    Activate {
        thread: thread::Id,
    },
    Focus {
        thread: thread::Id,
        focus: thread::Focus,
    },
    Close {
        thread: thread::Id,
    },
    DeleteHistoryThread {
        thread: thread::Id,
        delete_remote: bool,
    },
    SelectEntry {
        thread: thread::Id,
        entry: Option<thread::EntryId>,
    },
    SetContentScroll {
        thread: thread::Id,
        content_scroll: usize,
    },
    SetFolded {
        thread: thread::Id,
        entry: thread::EntryId,
        folded: bool,
    },
    TrackEntryDoc {
        thread: thread::Id,
        entry: thread::EntryId,
        doc: DocumentId,
    },
    UntrackEntryDoc {
        thread: thread::Id,
        entry: thread::EntryId,
    },
    OpenEntryDoc {
        thread: thread::Id,
        entry: thread::EntryId,
        action: crate::editor::Action,
    },
    UntrackDoc {
        doc: DocumentId,
    },
    SetDraft {
        thread: thread::Id,
        text: String,
    },
    Submit {
        thread: thread::Id,
        text: String,
    },
    ForkSubmit {
        thread: thread::Id,
        entry: thread::EntryId,
        text: String,
    },
    Cancel {
        thread: thread::Id,
    },
    AttachContext {
        thread: thread::Id,
        item: context::Kind,
    },
    DetachContext {
        thread: thread::Id,
        item: context::Id,
    },
    SetMentionContext {
        thread: thread::Id,
        items: Vec<context::Kind>,
    },
    Follow {
        thread: thread::Id,
    },
    PauseFollow {
        thread: thread::Id,
        reason: crate::collab::FollowPause,
    },
    Unfollow {
        thread: thread::Id,
    },
    SetMode {
        thread: thread::Id,
        mode: mode::Id,
    },
    SetConfig {
        thread: thread::Id,
        option: config::Id,
        value: config::ValueId,
    },
    SetProfile {
        thread: thread::Id,
        profile: profile::Defaults,
    },
    SetRating {
        thread: thread::Id,
        rating: thread::Rating,
    },
    SetNote {
        thread: thread::Id,
        note: Option<String>,
    },
    ResolvePermission {
        thread: thread::Id,
        request: permission::RequestId,
        decision: permission::Decision,
    },
    CompleteElicitation {
        thread: thread::Id,
        id: String,
        response: thread::ElicitationResponse,
    },
    Authenticate {
        thread: thread::Id,
        method: String,
    },
    SetReviewMode {
        thread: thread::Id,
        mode: review::Mode,
    },
    ResolveReview {
        thread: thread::Id,
        target: review::Target,
        decision: review::Decision,
    },
}
