use std::num::NonZeroU64;

use indexmap::IndexMap;

use super::{action, backend, context, effect, event, history, thread};

#[derive(Debug, Clone)]
pub enum Store {
    Empty(history::State),
    Ready {
        threads: Threads,
        history: history::State,
    },
}

impl Default for Store {
    fn default() -> Self {
        Self::Empty(history::State::default())
    }
}

#[derive(Debug, Clone)]
pub struct Threads {
    active: thread::Id,
    threads: IndexMap<thread::Id, thread::Thread>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("thread not found")]
pub struct MissingThread;

#[derive(Debug)]
pub enum Close {
    Empty {
        thread: thread::Thread,
    },
    Remaining {
        thread: thread::Thread,
        active: thread::Id,
    },
    Missing(MissingThread),
}

impl Store {
    #[must_use]
    pub fn ready(active: thread::Thread) -> Self {
        Self::Ready {
            threads: Threads::new(active, IndexMap::new()),
            history: history::State::default(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty(_))
    }

    #[must_use]
    pub fn active(&self) -> Option<thread::Id> {
        match self {
            Self::Empty(_) => None,
            Self::Ready { threads, .. } => Some(threads.active()),
        }
    }

    pub fn thread(&self, id: thread::Id) -> Option<&thread::Thread> {
        match self {
            Self::Empty(_) => None,
            Self::Ready { threads, .. } => threads.thread(id),
        }
    }

    pub fn thread_mut(&mut self, id: thread::Id) -> Option<&mut thread::Thread> {
        match self {
            Self::Empty(_) => None,
            Self::Ready { threads, .. } => threads.thread_mut(id),
        }
    }

    pub fn threads(&self) -> Box<dyn Iterator<Item = &thread::Thread> + '_> {
        match self {
            Self::Empty(_) => Box::new(std::iter::empty()),
            Self::Ready { threads, .. } => Box::new(threads.threads()),
        }
    }

    pub fn history(&self, scope: &thread::Scope) -> Option<&history::Page> {
        match self {
            Self::Empty(history) | Self::Ready { history, .. } => history.page(scope),
        }
    }

    pub fn record(&self, id: thread::Id) -> Option<history::Record> {
        self.thread(id).map(history::Record::from_thread)
    }

    pub fn activate(&mut self, id: thread::Id) -> Result<(), MissingThread> {
        match self {
            Self::Empty(_) => Err(MissingThread),
            Self::Ready { threads, .. } => threads.activate(id),
        }
    }

    pub fn insert(&mut self, thread: thread::Thread) {
        match self {
            Self::Empty(history) => {
                *self = Self::Ready {
                    threads: Threads::new(thread, IndexMap::new()),
                    history: std::mem::take(history),
                }
            }
            Self::Ready { threads, .. } => threads.insert(thread),
        }
    }

    pub fn create(&mut self, origin: thread::Origin, scope: thread::Scope) -> thread::Id {
        let id = self.next_id();
        self.insert(thread::Thread::new(id, origin, scope));
        id
    }

    pub fn bind_remote(
        &mut self,
        thread_id: thread::Id,
        backend: backend::Id,
        remote: backend::Remote,
    ) -> Result<(), MissingThread> {
        let thread = self.thread_mut(thread_id).ok_or(MissingThread)?;
        thread.set_origin(thread::Origin::Backend { backend, remote });
        Ok(())
    }

    pub fn thread_id_by_origin(&self, origin: &thread::Origin) -> Option<thread::Id> {
        self.threads()
            .find(|thread| thread.origin() == origin)
            .map(|thread| thread.id)
    }

    pub fn ensure_remote(
        &mut self,
        backend: backend::Id,
        remote: backend::Remote,
        scope: thread::Scope,
    ) -> thread::Id {
        let origin = thread::Origin::Backend {
            backend: backend.clone(),
            remote: remote.clone(),
        };
        if let Some(id) = self.thread_id_by_origin(&origin) {
            id
        } else {
            self.create(origin, scope)
        }
    }

    pub fn close(&mut self, id: thread::Id) -> Result<Option<thread::Thread>, MissingThread> {
        match self {
            Self::Empty(_) => Err(MissingThread),
            Self::Ready { threads, history } => match threads.close(id) {
                Close::Empty { thread } => {
                    *self = Self::Empty(std::mem::take(history));
                    Ok(Some(thread))
                }
                Close::Remaining { thread, .. } => Ok(Some(thread)),
                Close::Missing(err) => Err(err),
            },
        }
    }

    pub fn apply(&mut self, event: event::Event) -> Vec<effect::Effect> {
        match event {
            event::Event::Thread { thread, event } => {
                let active = self.active();
                if let Some(state) = self.thread_mut(thread) {
                    let publish_location = match &event {
                        thread::Event::Follow(location) => Some(location.clone()),
                        _ => None,
                    };
                    state.apply(event);
                    let mut effects = Vec::new();
                    if let Some(location) = publish_location {
                        effects.push(effect::Effect::PublishLocation {
                            thread,
                            location: location.clone(),
                        });
                        let participant = thread::participant(thread);
                        if Some(thread) == active
                            && matches!(
                                state.follow(),
                                crate::collab::FollowState::On {
                                    participant: current,
                                    ..
                                } if *current == participant
                            )
                        {
                            effects.push(effect::Effect::RevealLocation { location });
                        }
                    }
                    if Some(thread) == active {
                        state.set_unread(false);
                        self.sync_history(thread);
                        effects.push(effect::Effect::SyncModel);
                    } else {
                        state.set_unread(true);
                        self.sync_history(thread);
                        effects.push(effect::Effect::SyncModel);
                    }
                    effects
                } else {
                    Vec::new()
                }
            }
            event::Event::ContextResolved { thread, item } => {
                if let Some(state) = self.thread_mut(thread) {
                    let next = state.context_items().len() + 1;
                    state.push_context_item(context::Item {
                        id: context::Id::new(format!("ctx-{next}")),
                        kind: item,
                    });
                    vec![effect::Effect::Save { thread }, effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            event::Event::ContextResolveFailed { .. } | event::Event::Permission { .. } => {
                Vec::new()
            }
            event::Event::Backend { backend, event } => match event {
                super::backend::Event::Ready { .. } | super::backend::Event::Stopped => Vec::new(),
                super::backend::Event::Bound { thread, remote } => {
                    if self.bind_remote(thread, backend, remote).is_ok() {
                        self.sync_history(thread);
                        vec![
                            effect::Effect::EnsureParticipant { thread },
                            effect::Effect::Save { thread },
                            effect::Effect::SyncModel,
                        ]
                    } else {
                        Vec::new()
                    }
                }
            },
        }
    }

    pub fn act(&mut self, action: action::Action) -> Vec<effect::Effect> {
        match action {
            action::Action::Activate { thread } => {
                if self.activate(thread).is_ok() {
                    if let Some(state) = self.thread_mut(thread) {
                        state.set_unread(false);
                    }
                    self.sync_history(thread);
                    vec![effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::Focus { thread, focus } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.set_focus(focus);
                    vec![effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::Close { thread } => {
                if let Ok(Some(closed)) = self.close(thread) {
                    let record = history::Record::from_thread(&closed);
                    let backend_command = match closed.origin() {
                        thread::Origin::Backend { backend, .. } => {
                            Some(effect::Effect::SendBackendCommand {
                                backend: backend.clone(),
                                command: backend::Command::CloseThread { thread },
                            })
                        }
                        thread::Origin::Local => None,
                    };
                    match self {
                        Self::Empty(history) | Self::Ready { history, .. } => {
                            history.upsert(history::Stub::from_thread(&closed));
                        }
                    }
                    let mut effects = Vec::new();
                    if let Some(effect) = backend_command {
                        effects.push(effect);
                    }
                    effects.push(effect::Effect::LeaveParticipant { thread });
                    effects.push(effect::Effect::SaveNow { record });
                    effects.push(effect::Effect::SyncModel);
                    effects
                } else {
                    Vec::new()
                }
            }
            action::Action::SelectEntry { thread, entry } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.set_selected_entry(entry);
                    vec![effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::SetContentScroll {
                thread,
                content_scroll,
            } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.set_content_scroll(content_scroll);
                    vec![effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::SetFolded {
                thread,
                entry,
                folded,
            } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.set_folded(entry, folded);
                    vec![effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::TrackEntryDoc { thread, entry, doc } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.track_opened_doc(entry, doc);
                    vec![effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::OpenEntryDoc {
                thread,
                entry,
                action,
            } => {
                if self.thread(thread).is_some() {
                    vec![effect::Effect::OpenEntryDoc {
                        thread,
                        entry,
                        action,
                    }]
                } else {
                    Vec::new()
                }
            }
            action::Action::UntrackEntryDoc { thread, entry } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.untrack_opened_doc(entry);
                    vec![effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::UntrackDoc { doc } => {
                let mut changed = false;
                if let Self::Ready { threads, .. } = self {
                    for thread in threads.threads.values_mut() {
                        changed |= thread.untrack_document(doc);
                    }
                }
                if changed {
                    vec![effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::SetDraft { thread, text } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.set_draft(text);
                    vec![effect::Effect::Save { thread }, effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::SetConfig {
                thread,
                option,
                value,
            } => {
                if let Some(state) = self.thread_mut(thread) {
                    if state
                        .config_mut()
                        .set_pending(&option, value.clone())
                        .is_ok()
                    {
                        let mut effects = Vec::new();
                        if let thread::Origin::Backend { backend, .. } = state.origin() {
                            effects.push(effect::Effect::SendBackendCommand {
                                backend: backend.clone(),
                                command: backend::Command::SetConfig {
                                    thread,
                                    option,
                                    value,
                                },
                            });
                        }
                        effects.push(effect::Effect::Save { thread });
                        effects.push(effect::Effect::SyncModel);
                        effects
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            }
            action::Action::SetMode { thread, mode } => {
                if let Some(state) = self.thread_mut(thread) {
                    if let Some(set) = state.mode_mut() {
                        if set.set_pending(mode.clone()).is_ok() {
                            let mut effects = Vec::new();
                            if let thread::Origin::Backend { backend, .. } = state.origin() {
                                effects.push(effect::Effect::SendBackendCommand {
                                    backend: backend.clone(),
                                    command: backend::Command::SetMode { thread, mode },
                                });
                            }
                            effects.push(effect::Effect::Save { thread });
                            effects.push(effect::Effect::SyncModel);
                            effects
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            }
            action::Action::Follow { thread } => {
                if let Some(state) = self.thread_mut(thread) {
                    let was_off = matches!(state.follow(), crate::collab::FollowState::Off);
                    state.set_follow(match state.follow() {
                        crate::collab::FollowState::Off => crate::collab::FollowState::On {
                            mode: crate::collab::FollowMode::AutoSwitchAndReveal,
                            participant: thread::participant(thread),
                            last: None,
                        },
                        crate::collab::FollowState::On {
                            mode,
                            participant,
                            last,
                        }
                        | crate::collab::FollowState::Paused {
                            mode,
                            participant,
                            last,
                            ..
                        } => crate::collab::FollowState::On {
                            mode: *mode,
                            participant: *participant,
                            last: last.clone(),
                        },
                    });
                    let mut effects = Vec::new();
                    if was_off {
                        effects.push(effect::Effect::EnsureParticipant { thread });
                    }
                    effects.push(effect::Effect::Save { thread });
                    effects.push(effect::Effect::SyncModel);
                    effects
                } else {
                    Vec::new()
                }
            }
            action::Action::Unfollow { thread } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.set_follow(crate::collab::FollowState::Off);
                    vec![effect::Effect::Save { thread }, effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::PauseFollow { thread, reason } => {
                if let Some(state) = self.thread_mut(thread) {
                    match state.follow() {
                        crate::collab::FollowState::On {
                            mode,
                            participant,
                            last,
                        }
                        | crate::collab::FollowState::Paused {
                            mode,
                            participant,
                            last,
                            ..
                        } => {
                            state.set_follow(crate::collab::FollowState::Paused {
                                mode: *mode,
                                participant: *participant,
                                last: last.clone(),
                                reason,
                            });
                            vec![effect::Effect::Save { thread }, effect::Effect::SyncModel]
                        }
                        crate::collab::FollowState::Off => Vec::new(),
                    }
                } else {
                    Vec::new()
                }
            }
            action::Action::AttachContext { thread, item } => {
                self.apply(event::Event::ContextResolved { thread, item })
            }
            action::Action::DetachContext { thread, item } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.retain_context_items(|ctx| ctx.id != item);
                    vec![effect::Effect::Save { thread }, effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::Submit { thread, text } => {
                if let Some(state) = self.thread_mut(thread) {
                    let thread::Origin::Backend { backend, .. } = state.origin() else {
                        return Vec::new();
                    };
                    let backend = backend.clone();
                    let mut prompt =
                        super::prompt::Request::builder(thread, super::prompt::Role::User)
                            .text(text.clone());
                    for item in state.context_items() {
                        prompt = prompt.push_context(item.kind.clone());
                    }
                    state.set_draft(String::new());
                    state.apply(thread::Event::Content(thread::Content::Append(
                        thread::NewEntry {
                            turn: None,
                            kind: thread::EntryKind::UserPrompt { text },
                            locations: Vec::new(),
                        },
                    )));
                    vec![
                        effect::Effect::SendBackendCommand {
                            backend,
                            command: backend::Command::Submit {
                                thread,
                                prompt: prompt.build(),
                            },
                        },
                        effect::Effect::Save { thread },
                        effect::Effect::SyncModel,
                    ]
                } else {
                    Vec::new()
                }
            }
            action::Action::Cancel { thread } => self
                .thread(thread)
                .and_then(|state| match state.origin() {
                    thread::Origin::Backend { backend, .. } => {
                        Some(vec![effect::Effect::SendBackendCommand {
                            backend: backend.clone(),
                            command: backend::Command::Cancel { thread },
                        }])
                    }
                    thread::Origin::Local => None,
                })
                .unwrap_or_default(),
            action::Action::ResolvePermission {
                thread,
                request,
                decision,
            } => self
                .thread(thread)
                .and_then(|state| match state.origin() {
                    thread::Origin::Backend { backend, .. } => {
                        Some(vec![effect::Effect::SendBackendCommand {
                            backend: backend.clone(),
                            command: backend::Command::ResolvePermission {
                                thread,
                                request,
                                decision,
                            },
                        }])
                    }
                    thread::Origin::Local => None,
                })
                .unwrap_or_default(),
            action::Action::NewThread { backend, scope } => {
                let thread = self.create(thread::Origin::Local, scope.clone());
                let _ = self.activate(thread);
                self.sync_history(thread);
                vec![
                    effect::Effect::SendBackendCommand {
                        backend,
                        command: backend::Command::NewThread { thread, scope },
                    },
                    effect::Effect::Save { thread },
                    effect::Effect::SyncModel,
                ]
            }
            action::Action::LoadThread { record, activate } => {
                let backend_command = match &record.origin {
                    thread::Origin::Backend { backend, remote } => {
                        Some(effect::Effect::SendBackendCommand {
                            backend: backend.clone(),
                            command: backend::Command::LoadThread {
                                thread: record.id,
                                remote: remote.clone(),
                            },
                        })
                    }
                    thread::Origin::Local => None,
                };
                let thread = record.into_thread();
                let id = thread.id;
                self.insert(thread);
                if activate {
                    let _ = self.activate(id);
                }
                self.sync_history(id);
                let mut effects = Vec::new();
                if let Some(effect) = backend_command {
                    effects.push(effect);
                }
                effects.push(effect::Effect::SyncModel);
                effects
            }
        }
    }
}

impl Store {
    fn next_id(&self) -> thread::Id {
        let next = self
            .threads()
            .map(|thread| thread.id.value().get())
            .max()
            .unwrap_or(0)
            + 1;
        thread::Id::new(NonZeroU64::new(next).expect("thread id must be non-zero"))
    }

    pub fn replace_history(
        &mut self,
        scope: thread::Scope,
        entries: Vec<history::Stub>,
        next: Option<history::Cursor>,
    ) -> Vec<effect::Effect> {
        match self {
            Self::Empty(history) => {
                let count = entries.len();
                history.replace(scope, entries, next);
                vec![
                    effect::Effect::SetStatus {
                        message: format!("Assistant history updated ({count} sessions)"),
                    },
                    effect::Effect::SyncModel,
                ]
            }
            Self::Ready { threads, history } => {
                let count = entries.len();
                history.replace(scope, entries, next);
                let ids: Vec<_> = threads.threads.keys().copied().collect();
                for id in ids {
                    if let Some(thread) = threads.thread(id).cloned() {
                        history.sync_thread(&thread);
                    }
                }
                vec![
                    effect::Effect::SetStatus {
                        message: format!("Assistant history updated ({count} sessions)"),
                    },
                    effect::Effect::SyncModel,
                ]
            }
        }
    }

    fn sync_history(&mut self, thread: thread::Id) {
        if let Self::Ready { threads, history } = self {
            if let Some(thread) = threads.thread(thread).cloned() {
                history.sync_thread(&thread);
            }
        }
    }
}

impl Threads {
    pub fn new(active: thread::Thread, mut others: IndexMap<thread::Id, thread::Thread>) -> Self {
        let active_id = active.id;
        others.shift_remove(&active_id);
        others.insert(active_id, active);
        Self {
            active: active_id,
            threads: others,
        }
    }

    #[must_use]
    pub fn active(&self) -> thread::Id {
        self.active
    }

    pub fn thread(&self, id: thread::Id) -> Option<&thread::Thread> {
        self.threads.get(&id)
    }

    pub fn thread_mut(&mut self, id: thread::Id) -> Option<&mut thread::Thread> {
        self.threads.get_mut(&id)
    }

    pub fn threads(&self) -> impl Iterator<Item = &thread::Thread> {
        self.threads.values()
    }

    pub fn activate(&mut self, id: thread::Id) -> Result<(), MissingThread> {
        if self.threads.contains_key(&id) {
            self.active = id;
            Ok(())
        } else {
            Err(MissingThread)
        }
    }

    pub fn insert(&mut self, thread: thread::Thread) {
        self.threads.shift_remove(&thread.id);
        self.threads.insert(thread.id, thread);
    }

    pub fn close(&mut self, id: thread::Id) -> Close {
        let Some(thread) = self.threads.shift_remove(&id) else {
            return Close::Missing(MissingThread);
        };

        if self.threads.is_empty() {
            return Close::Empty { thread };
        }

        if self.active == id {
            self.active = *self
                .threads
                .last()
                .map(|(id, _)| id)
                .expect("non-empty threads");
        }

        Close::Remaining {
            thread,
            active: self.active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, thread::Id) {
        let thread = thread::Thread::new(
            thread::Id::new(NonZeroU64::new(1).unwrap()),
            thread::Origin::Local,
            thread::Scope::new(std::path::PathBuf::from(".")),
        );
        let id = thread.id;
        (Store::ready(thread), id)
    }

    fn backend_store() -> (Store, thread::Id, backend::Id) {
        let backend = backend::Id::new("backend");
        let thread = thread::Thread::new(
            thread::Id::new(NonZeroU64::new(1).unwrap()),
            thread::Origin::Backend {
                backend: backend.clone(),
                remote: backend::Remote::new("remote"),
            },
            thread::Scope::new(std::path::PathBuf::from(".")),
        );
        let id = thread.id;
        (Store::ready(thread), id, backend)
    }

    fn local_thread(id: u64) -> thread::Thread {
        thread::Thread::new(
            thread::Id::new(NonZeroU64::new(id).unwrap()),
            thread::Origin::Local,
            thread::Scope::new(std::path::PathBuf::from(".")),
        )
    }

    #[test]
    fn follow_turns_on_from_off() {
        let (mut store, thread) = store();

        store.act(action::Action::Follow { thread });

        assert!(matches!(
            store.thread(thread).map(|thread| thread.follow()),
            Some(crate::collab::FollowState::On { participant, .. })
                if *participant == thread::participant(thread)
        ));
    }

    #[test]
    fn pause_follow_moves_on_state_to_paused() {
        let (mut store, thread) = store();
        store.act(action::Action::Follow { thread });

        store.act(action::Action::PauseFollow {
            thread,
            reason: crate::collab::FollowPause::LocalMove,
        });

        assert!(matches!(
            store.thread(thread).map(|thread| thread.follow()),
            Some(crate::collab::FollowState::Paused {
                reason: crate::collab::FollowPause::LocalMove,
                ..
            })
        ));
    }

    #[test]
    fn follow_resumes_from_paused() {
        let (mut store, thread) = store();
        store.act(action::Action::Follow { thread });
        store.act(action::Action::PauseFollow {
            thread,
            reason: crate::collab::FollowPause::LocalScroll,
        });

        store.act(action::Action::Follow { thread });

        assert!(matches!(
            store.thread(thread).map(|thread| thread.follow()),
            Some(crate::collab::FollowState::On { .. })
        ));
    }

    #[test]
    fn follow_event_emits_publish_location_effect() {
        let (mut store, thread) = store();
        let location = crate::collab::Location::new(
            std::path::PathBuf::from("file.rs"),
            crate::collab::location::Source::Tool,
        );

        let effects = store.apply(event::Event::Thread {
            thread,
            event: thread::Event::Follow(location.clone()),
        });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::PublishLocation { thread: current, location: current_location }
                    if *current == thread && current_location == &location
            )
        }));
    }

    #[test]
    fn active_follow_event_emits_reveal_location_effect() {
        let (mut store, thread) = store();
        store.act(action::Action::Follow { thread });
        let location = crate::collab::Location::new(
            std::path::PathBuf::from("file.rs"),
            crate::collab::location::Source::Tool,
        );

        let effects = store.apply(event::Event::Thread {
            thread,
            event: thread::Event::Follow(location.clone()),
        });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::RevealLocation { location: current } if current == &location
            )
        }));
    }

    #[test]
    fn bound_backend_emits_ensure_participant_effect() {
        let (mut store, thread) = store();
        let backend = backend::Id::new("backend");
        let remote = backend::Remote::new("remote");

        let effects = store.apply(event::Event::Backend {
            backend,
            event: backend::Event::Bound { thread, remote },
        });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::EnsureParticipant { thread: current } if *current == thread
            )
        }));
    }

    #[test]
    fn follow_from_off_emits_ensure_participant_effect() {
        let (mut store, thread) = store();

        let effects = store.act(action::Action::Follow { thread });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::EnsureParticipant { thread: current } if *current == thread
            )
        }));
    }

    #[test]
    fn submit_emits_backend_command_and_appends_prompt() {
        let (mut store, thread, backend) = backend_store();
        let prompt =
            super::super::prompt::Request::builder(thread, super::super::prompt::Role::User)
                .text("hello")
                .build();

        let effects = store.act(action::Action::Submit {
            thread,
            text: "hello".to_string(),
        });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand { backend: current, command: backend::Command::Submit { thread: current_thread, prompt: current_prompt } }
                    if current == &backend && *current_thread == thread && current_prompt == &prompt
            )
        }));
        assert!(matches!(
            store.thread(thread).and_then(|thread| thread.entries().last()),
            Some(thread::Entry { kind: thread::EntryKind::UserPrompt { text }, .. }) if text == "hello"
        ));
    }

    #[test]
    fn new_thread_emits_backend_command_and_creates_local_thread() {
        let (mut store, _thread, backend) = backend_store();
        let scope = thread::Scope::new(std::path::PathBuf::from("."));

        let effects = store.act(action::Action::NewThread {
            backend: backend.clone(),
            scope: scope.clone(),
        });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::NewThread { scope: current_scope, .. },
                } if current == &backend && current_scope == &scope
            )
        }));
        assert_eq!(store.threads().count(), 2);
        let active = store.active().expect("new thread should be active");
        let active_thread = store.thread(active).expect("active thread");
        assert!(matches!(active_thread.origin(), thread::Origin::Local));
        assert_eq!(active_thread.scope(), &scope);
    }

    #[test]
    fn resolve_permission_emits_backend_command() {
        let (mut store, thread, backend) = backend_store();
        let request = super::super::permission::RequestId::new("request");
        let decision = super::super::permission::Decision::Dismiss;

        let effects = store.act(action::Action::ResolvePermission {
            thread,
            request: request.clone(),
            decision: decision.clone(),
        });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::ResolvePermission {
                        thread: current_thread,
                        request: current_request,
                        decision: current_decision,
                    },
                } if current == &backend
                    && *current_thread == thread
                    && current_request == &request
                    && current_decision == &decision
            )
        }));
    }

    #[test]
    fn close_emits_backend_close_command() {
        let (mut store, thread, backend) = backend_store();

        let effects = store.act(action::Action::Close { thread });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::LeaveParticipant { thread: current } if *current == thread
            )
        }));
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::CloseThread { thread: current_thread },
                } if current == &backend && *current_thread == thread
            )
        }));
    }

    #[test]
    fn closing_active_thread_promotes_most_recent_remaining_thread() {
        let first = local_thread(1);
        let second = local_thread(2);
        let third = local_thread(3);
        let second_id = second.id;
        let third_id = third.id;

        let mut threads = Threads::new(first, IndexMap::new());
        threads.insert(second);
        threads.insert(third);

        let closed = threads.close(thread::Id::new(NonZeroU64::new(1).unwrap()));

        assert!(matches!(closed, Close::Remaining { active, .. } if active == third_id));
        assert_eq!(threads.active(), third_id);
        assert_eq!(
            threads
                .threads()
                .map(|thread| thread.id)
                .collect::<Vec<_>>(),
            vec![second_id, third_id]
        );
    }

    #[test]
    fn reinserting_thread_moves_it_to_end_of_order() {
        let first = local_thread(1);
        let second = local_thread(2);
        let third = local_thread(3);
        let second_id = second.id;
        let third_id = third.id;

        let mut threads = Threads::new(first, IndexMap::new());
        threads.insert(second.clone());
        threads.insert(third);

        let mut updated_second = second;
        updated_second.set_title(Some("updated".to_string()));
        threads.insert(updated_second);

        assert_eq!(
            threads
                .threads()
                .map(|thread| thread.id)
                .collect::<Vec<_>>(),
            vec![
                thread::Id::new(NonZeroU64::new(1).unwrap()),
                third_id,
                second_id
            ]
        );

        let closed = threads.close(thread::Id::new(NonZeroU64::new(1).unwrap()));
        assert!(matches!(closed, Close::Remaining { active, .. } if active == second_id));
        assert_eq!(threads.active(), second_id);
    }

    #[test]
    fn restore_emits_backend_load_command() {
        let (_current, thread, backend) = backend_store();
        let record = history::Record::from_thread(&thread::Thread::new(
            thread,
            thread::Origin::Backend {
                backend: backend.clone(),
                remote: backend::Remote::new("remote"),
            },
            thread::Scope::new(std::path::PathBuf::from(".")),
        ));
        let mut store = Store::default();

        let effects = store.act(action::Action::LoadThread {
            record,
            activate: true,
        });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::LoadThread {
                        thread: current_thread,
                        remote,
                    },
                } if current == &backend
                    && *current_thread == thread
                    && remote.as_str() == "remote"
            )
        }));
    }

    #[test]
    fn replace_history_emits_status_effect() {
        let mut store = Store::default();
        let scope = thread::Scope::new(std::path::PathBuf::from("."));

        let effects = store.replace_history(scope, Vec::new(), None);

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SetStatus { message }
                    if message == "Assistant history updated (0 sessions)"
            )
        }));
    }

    #[test]
    fn untrack_doc_removes_matching_opened_docs() {
        let (mut store, thread) = store();
        let doc = crate::DocumentId::default();

        let _ = store.act(action::Action::TrackEntryDoc {
            thread,
            entry: crate::assistant::thread::EntryId::new(NonZeroU64::new(10).unwrap()),
            doc,
        });

        let effects = store.act(action::Action::UntrackDoc { doc });

        assert!(effects
            .iter()
            .any(|effect| matches!(effect, effect::Effect::SyncModel)));
        assert!(store
            .thread(thread)
            .map(|thread| thread.opened_docs().is_empty())
            .unwrap_or(false));
    }
}
