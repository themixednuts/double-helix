//! Assistant domain (`docs/collaboration-assistant-architecture-spec.md`).

pub mod acp;
pub mod action;
pub mod auth;
pub mod backend;
pub mod change;
pub mod config;
pub mod context;
pub mod effect;
pub mod elicitation;
pub mod event;
pub mod history;
pub mod host;
pub mod layout;
pub mod mention;
pub mod mode;
pub mod model;
pub mod permission;
pub mod plan;
pub mod profile;
pub mod prompt;
pub mod review;
pub mod store;
pub mod terminal;
pub mod thread;
pub mod tool;

pub use action::Action;
pub use backend::{Command as BackendCommand, Driver as BackendDriver, Handle as BackendHandle};
pub use model::{EntryView, Follow, Panel, Pill, Tab, ThreadView};
pub use store::Store;
pub use thread::Id as ThreadId;

#[must_use]
pub fn retry_prompt(thread: &thread::Thread) -> Option<String> {
    if !retry_available(thread) {
        return None;
    }

    thread.entries().iter().rev().find_map(|entry| {
        if let thread::EntryKind::UserPrompt { text } = &entry.kind {
            Some(text.clone())
        } else {
            None
        }
    })
}

#[must_use]
pub fn retry_available(thread: &thread::Thread) -> bool {
    matches!(thread.run(), thread::Run::Failed { .. })
        || matches!(thread.run(), thread::Run::Idle)
            && thread.entries().iter().rev().any(|entry| {
                matches!(
                    &entry.kind,
                    thread::EntryKind::Status { text }
                        if text.to_ascii_lowercase().contains("cancel")
                )
            })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    fn thread_with_entries(entries: Vec<thread::Entry>) -> thread::Thread {
        let mut thread = thread::Thread::new(
            thread::Id::new(NonZeroU64::new(1).unwrap()),
            thread::Origin::Local,
            thread::Scope::new(std::path::PathBuf::from(".")),
        );
        thread.restore_persisted_state(thread::PersistedState {
            title: None,
            entries,
            turns: Vec::new(),
            plan: Vec::new(),
            draft: String::new(),
            context: Vec::new(),
            follow: crate::collab::FollowState::Off,
            run: thread::Run::Failed {
                message: "failed".to_string(),
            },
            unread: false,
            mode: None,
            config: config::State::new(Vec::new()),
            terminals: Vec::new(),
            auth: auth::State::default(),
            review_mode: review::Mode::Write,
            usage: thread::Usage::default(),
            commands: Vec::new(),
            pending_elicitations: Vec::new(),
            caps: None,
            profile: None,
            feedback: thread::Feedback::default(),
        });
        thread
    }

    fn entry(value: u64, kind: thread::EntryKind) -> thread::Entry {
        thread::Entry {
            id: thread::EntryId::new(NonZeroU64::new(value).unwrap()),
            turn: None,
            stream: None,
            kind,
            locations: Vec::new(),
        }
    }

    #[test]
    fn retry_prompt_returns_last_user_prompt_for_failed_run() {
        let thread = thread_with_entries(vec![
            entry(
                1,
                thread::EntryKind::UserPrompt {
                    text: "first".to_string(),
                },
            ),
            entry(
                2,
                thread::EntryKind::UserPrompt {
                    text: "second".to_string(),
                },
            ),
        ]);

        assert_eq!(retry_prompt(&thread), Some("second".to_string()));
    }

    #[test]
    fn retry_prompt_is_unavailable_without_failed_or_canceled_run() {
        let mut thread = thread_with_entries(vec![entry(
            1,
            thread::EntryKind::UserPrompt {
                text: "hello".to_string(),
            },
        )]);
        thread.set_run(thread::Run::Idle);

        assert_eq!(retry_prompt(&thread), None);
    }
}
