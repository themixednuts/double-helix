use std::num::NonZeroU64;

use indexmap::IndexMap;

use super::{
    action, backend, config, context, effect, event, history, mention, mode, review, thread,
};

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

    pub fn active_thread(&self) -> Option<(thread::Id, &thread::Thread)> {
        let id = self.active()?;
        Some((id, self.thread(id)?))
    }

    pub fn active_thread_mut(&mut self) -> Option<(thread::Id, &mut thread::Thread)> {
        let id = self.active()?;
        Some((id, self.thread_mut(id)?))
    }

    pub fn active_thread_owned(&self) -> Option<(thread::Id, thread::Thread)> {
        self.active_thread()
            .map(|(id, thread)| (id, thread.clone()))
    }

    pub fn active_snapshot(&self) -> Option<thread::Snapshot> {
        let (_, thread) = self.active_thread()?;
        Some(thread.snapshot())
    }

    pub fn active_context(&self) -> Option<Vec<context::Item>> {
        let (_, thread) = self.active_thread()?;
        Some(thread.context_items().to_vec())
    }

    pub fn active_scope(&self) -> Option<thread::Scope> {
        let (_, thread) = self.active_thread()?;
        Some(thread.clone_scope())
    }

    #[must_use]
    pub fn active_scope_or_layout(&self) -> thread::Scope {
        self.active_scope()
            .unwrap_or_else(super::layout::current_scope)
    }

    pub fn active_change_summary(&self) -> Option<super::change::Summary> {
        let (_, thread) = self.active_thread()?;
        thread.change_summary()
    }

    pub fn selected_change_summary(&self) -> Option<super::change::Summary> {
        let (_, thread) = self.active_thread()?;
        thread.selected_change_summary()
    }

    pub fn selected_review_target(&self) -> Option<(thread::Id, review::Target)> {
        let (thread_id, thread) = self.active_thread()?;
        let entry = thread.selected()?;
        let thread::EntryKind::ChangeSummary(summary) = &entry.kind else {
            return None;
        };
        let file = summary
            .files
            .iter()
            .filter_map(|file| file.review.as_ref())
            .find(|file| file.status.is_pending())?;
        Some((thread_id, review::Target::File(file.path.clone())))
    }

    pub fn selected_subagent(&self) -> Option<super::tool::SubagentSessionInfo> {
        let (_, thread) = self.active_thread()?;
        let entry = thread.selected()?;
        let thread::EntryKind::ToolCall(call) = &entry.kind else {
            return None;
        };
        call.subagent.clone()
    }

    pub fn active_backend_id(&self) -> Option<backend::Id> {
        let (_, thread) = self.active_thread()?;
        thread.backend_id()
    }

    pub fn active_id(&self) -> Option<thread::Id> {
        self.active_thread().map(|(id, _)| id)
    }

    pub fn active_cycle_config(
        &self,
        category: &str,
    ) -> Option<(thread::Id, config::Id, config::ValueId)> {
        let (id, thread) = self.active_thread()?;
        let (option, value) = thread.cycle_config(category)?;
        Some((id, option, value))
    }

    pub fn active_next_mode(&self) -> Option<(thread::Id, mode::Id)> {
        let (id, thread) = self.active_thread()?;
        Some((id, thread.next_mode()?))
    }

    pub fn history_entries(&self, scope: &thread::Scope) -> Option<Vec<history::Stub>> {
        self.history(scope).map(|page| page.entries.clone())
    }

    pub fn history_records(&self) -> Vec<history::Record> {
        self.threads().map(history::Record::from_thread).collect()
    }

    #[must_use]
    pub fn has_threads(&self) -> bool {
        self.threads().next().is_some()
    }

    #[must_use]
    pub fn layout_history_entries(&self) -> Vec<history::Stub> {
        self.history_entries(&super::layout::current_scope())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn assistant_model(&self, focused: bool) -> crate::model::AssistantModel {
        let fallback_agent_name = self
            .active_thread()
            .and_then(|(_, thread)| thread.title().map(ToOwned::to_owned))
            .unwrap_or_else(|| {
                if self.threads().next().is_none() {
                    "No agent".to_string()
                } else {
                    "Agent".to_string()
                }
            });
        self.model(focused, self.layout_history_entries(), fallback_agent_name)
    }

    pub fn next_thread(&self, delta: isize) -> Option<thread::Id> {
        let (active, _) = self.active_thread()?;
        let tabs: Vec<_> = self.threads().map(|thread| thread.id).collect();
        if tabs.len() < 2 {
            return None;
        }
        let index = tabs.iter().position(|id| *id == active)?;
        let next = (index as isize + delta).rem_euclid(tabs.len() as isize) as usize;
        Some(tabs[next])
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

    pub fn delete_history_thread(
        &mut self,
        thread: thread::Id,
        delete_remote: bool,
    ) -> Vec<effect::Effect> {
        let mut remote_delete = None;
        if let Some(state) = self.thread(thread) {
            if let thread::Origin::Backend { backend, remote } = state.origin() {
                remote_delete = Some((backend.clone(), remote.clone()));
            }
        }

        let closed = self.close(thread).ok().flatten();
        let removed = match self {
            Self::Empty(history) | Self::Ready { history, .. } => history.remove(thread),
        };

        if remote_delete.is_none() {
            remote_delete = removed.as_ref().and_then(|entry| match &entry.origin {
                Some(thread::Origin::Backend { backend, remote }) => {
                    Some((backend.clone(), remote.clone()))
                }
                _ => None,
            });
        }

        let mut effects = Vec::new();
        if let Some(closed) = closed {
            effects.push(effect::Effect::LeaveParticipant { thread: closed.id });
        }
        effects.push(effect::Effect::Delete { thread });
        if delete_remote {
            if let Some((backend, remote)) = remote_delete {
                effects.push(effect::Effect::SendBackendCommand {
                    backend,
                    command: backend::Command::DeleteThread { remote },
                });
            }
        }
        effects.push(effect::Effect::SyncModel);
        effects
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
                    let event_applies_profile_mode = matches!(event, thread::Event::Mode(_));
                    let event_applies_profile_config = matches!(event, thread::Event::Config(_));
                    state.apply(event);
                    let mut effects = Vec::new();
                    if event_applies_profile_mode || event_applies_profile_config {
                        effects.extend(Self::apply_profile_defaults(thread, state));
                    }
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
            event::Event::Auth { thread, event } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.apply(thread::Event::Auth(event));
                    self.sync_history(thread);
                    vec![effect::Effect::Save { thread }, effect::Effect::SyncModel]
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
            event::Event::ReviewAcceptedFile { thread, path, text } => {
                if self.thread(thread).is_some() {
                    vec![effect::Effect::ApplyReviewAcceptedFile { thread, path, text }]
                } else {
                    Vec::new()
                }
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
                    effects.push(effect::Effect::SaveNow {
                        record: Box::new(record),
                    });
                    effects.push(effect::Effect::SyncModel);
                    effects
                } else {
                    Vec::new()
                }
            }
            action::Action::DeleteHistoryThread {
                thread,
                delete_remote,
            } => self.delete_history_thread(thread, delete_remote),
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
            action::Action::SetProfile { thread, profile } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.set_profile(Some(profile));
                    let mut effects = Self::apply_profile_defaults(thread, state);
                    effects.push(effect::Effect::Save { thread });
                    effects.push(effect::Effect::SyncModel);
                    effects
                } else {
                    Vec::new()
                }
            }
            action::Action::SetRating { thread, rating } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.toggle_rating(rating);
                    self.sync_history(thread);
                    vec![effect::Effect::Save { thread }, effect::Effect::SyncModel]
                } else {
                    Vec::new()
                }
            }
            action::Action::SetNote { thread, note } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.set_note(note);
                    self.sync_history(thread);
                    vec![effect::Effect::Save { thread }, effect::Effect::SyncModel]
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
            action::Action::SetMentionContext { thread, items } => {
                if let Some(state) = self.thread_mut(thread) {
                    state.retain_context_items(|ctx| !mention::is_context_id(&ctx.id));
                    let mut seen = std::collections::BTreeSet::new();
                    for item in items {
                        let key = mention::key_for_kind(&item);
                        if seen.insert(key.clone()) {
                            state.push_context_item(context::Item {
                                id: mention::context_id(&key),
                                kind: item,
                            });
                        }
                    }
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
            action::Action::ForkSubmit {
                thread,
                entry,
                text,
            } => {
                if let Some(state) = self.thread_mut(thread) {
                    let thread::Origin::Backend { backend, .. } = state.origin() else {
                        return Vec::new();
                    };
                    let backend = backend.clone();
                    let supports_fork = state.caps().is_some_and(|caps| caps.fork_session);
                    if !state.fork_before(entry) {
                        return Vec::new();
                    }
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
                    let mut effects = Vec::new();
                    if !supports_fork {
                        effects.push(effect::Effect::SetStatus {
                            message: "agent doesn't support editing history — resent as a new message; earlier context may be retained".to_string(),
                        });
                    }
                    effects.push(effect::Effect::SendBackendCommand {
                        backend,
                        command: backend::Command::ForkSubmit {
                            thread,
                            prompt: prompt.build(),
                        },
                    });
                    effects.push(effect::Effect::Save { thread });
                    effects.push(effect::Effect::SyncModel);
                    effects
                } else {
                    Vec::new()
                }
            }
            action::Action::Cancel { thread } => {
                let Some(state) = self.thread_mut(thread) else {
                    return Vec::new();
                };
                let thread::Origin::Backend { backend, .. } = state.origin() else {
                    return Vec::new();
                };
                let backend = backend.clone();
                state.apply(thread::Event::Run(thread::Run::Canceling));
                state.apply(thread::Event::Content(thread::Content::Append(
                    thread::NewEntry {
                        turn: None,
                        kind: thread::EntryKind::Status {
                            text: "Canceling assistant run...".to_string(),
                        },
                        locations: Vec::new(),
                    },
                )));
                vec![
                    effect::Effect::SendBackendCommand {
                        backend,
                        command: backend::Command::Cancel { thread },
                    },
                    effect::Effect::Save { thread },
                    effect::Effect::SyncModel,
                ]
            }
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
            action::Action::CompleteElicitation {
                thread,
                id,
                response,
            } => {
                let Some(state) = self.thread_mut(thread) else {
                    return Vec::new();
                };
                let thread::Origin::Backend { backend, .. } = state.origin() else {
                    return Vec::new();
                };
                let backend = backend.clone();
                let status = match response {
                    thread::ElicitationResponse::Accept(_) => thread::ElicitationStatus::Completed,
                    thread::ElicitationResponse::Decline => thread::ElicitationStatus::Declined,
                    thread::ElicitationResponse::Cancel => thread::ElicitationStatus::Canceled,
                };
                state.apply(thread::Event::Elicitation(
                    thread::ElicitationEvent::Complete {
                        id: id.clone(),
                        status,
                    },
                ));
                vec![
                    effect::Effect::SendBackendCommand {
                        backend,
                        command: backend::Command::CompleteElicitation {
                            thread,
                            id,
                            response,
                        },
                    },
                    effect::Effect::Save { thread },
                    effect::Effect::SyncModel,
                ]
            }
            action::Action::Authenticate { thread, method } => {
                let Some(state) = self.thread_mut(thread) else {
                    return Vec::new();
                };
                let thread::Origin::Backend { backend, .. } = state.origin() else {
                    return Vec::new();
                };
                let backend = backend.clone();
                if !state.auth_mut().authenticate(&method) {
                    return Vec::new();
                }
                vec![
                    effect::Effect::SendBackendCommand {
                        backend,
                        command: backend::Command::Authenticate { thread, method },
                    },
                    effect::Effect::Save { thread },
                    effect::Effect::SyncModel,
                ]
            }
            action::Action::SetReviewMode { thread, mode } => {
                let Some(state) = self.thread_mut(thread) else {
                    return Vec::new();
                };
                state.apply(thread::Event::Review(review::Event::Mode(mode)));
                let mut effects = Vec::new();
                if let thread::Origin::Backend { backend, .. } = state.origin() {
                    effects.push(effect::Effect::SendBackendCommand {
                        backend: backend.clone(),
                        command: backend::Command::Review {
                            thread,
                            command: review::Command::SetMode(mode),
                        },
                    });
                }
                effects.push(effect::Effect::Save { thread });
                effects.push(effect::Effect::SyncModel);
                effects
            }
            action::Action::ResolveReview {
                thread,
                target,
                decision,
            } => {
                let Some(state) = self.thread_mut(thread) else {
                    return Vec::new();
                };
                state.apply(thread::Event::Review(review::Event::Resolve {
                    target: target.clone(),
                    decision,
                }));
                let mut effects = Vec::new();
                if let thread::Origin::Backend { backend, .. } = state.origin() {
                    effects.push(effect::Effect::SendBackendCommand {
                        backend: backend.clone(),
                        command: backend::Command::Review {
                            thread,
                            command: review::Command::Resolve { target, decision },
                        },
                    });
                }
                effects.push(effect::Effect::Save { thread });
                effects.push(effect::Effect::SyncModel);
                effects
            }
            action::Action::NewThread {
                backend,
                scope,
                profile,
            } => {
                let thread = self.create(thread::Origin::Local, scope.clone());
                if let Some(state) = self.thread_mut(thread) {
                    state.set_profile(profile);
                }
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
            action::Action::LoadThread { record, activation } => {
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
                let thread = (*record).into_thread();
                let id = thread.id;
                self.insert(thread);
                if activation.should_activate() {
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
            action::Action::LoadRemoteThread {
                backend,
                remote,
                scope,
                activation,
            } => {
                let thread = self.ensure_remote(backend.clone(), remote.clone(), scope);
                if activation.should_activate() {
                    let _ = self.activate(thread);
                }
                vec![
                    effect::Effect::SendBackendCommand {
                        backend,
                        command: backend::Command::LoadThread { thread, remote },
                    },
                    effect::Effect::SyncModel,
                ]
            }
        }
    }
}

impl Store {
    fn apply_profile_defaults(
        thread: thread::Id,
        state: &mut thread::Thread,
    ) -> Vec<effect::Effect> {
        let Some(profile) = state.profile() else {
            return Vec::new();
        };

        let mut effects = Vec::new();
        let pending_mode = profile.mode_pending().cloned();
        let pending_config = profile.config_pending().map(|config| config.to_vec());

        if let Some(mode) = pending_mode {
            if let Some(mode_set) = state.mode_mut() {
                if mode_set.set_pending(mode.clone()).is_ok() {
                    if let thread::Origin::Backend { backend, .. } = state.origin() {
                        effects.push(effect::Effect::SendBackendCommand {
                            backend: backend.clone(),
                            command: backend::Command::SetMode { thread, mode },
                        });
                    }
                }
                if let Some(profile) = state.profile_mut() {
                    profile.mark_mode_applied();
                }
            }
        }

        if let Some(config) = pending_config {
            if state.config().items().next().is_some() {
                for (option, value) in config {
                    if state
                        .config_mut()
                        .set_pending(&option, value.clone())
                        .is_ok()
                    {
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
                    }
                }
                if let Some(profile) = state.profile_mut() {
                    profile.mark_config_applied();
                }
            }
        }
        effects
    }

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

    fn append(store: &mut Store, thread: thread::Id, kind: thread::EntryKind) -> thread::EntryId {
        let _ = store.apply(event::Event::Thread {
            thread,
            event: thread::Event::Content(thread::Content::Append(thread::NewEntry {
                turn: None,
                kind,
                locations: Vec::new(),
            })),
        });
        store
            .thread(thread)
            .and_then(|thread| thread.entries().last())
            .expect("entry")
            .id
    }

    fn set_caps(store: &mut Store, thread: thread::Id, caps: helix_acp::AgentCaps) {
        let _ = store.apply(event::Event::Thread {
            thread,
            event: thread::Event::Caps(caps),
        });
    }

    #[test]
    fn review_accepted_file_emits_apply_effect_for_live_thread() {
        let (mut store, thread) = store();
        let path = std::path::PathBuf::from("src/lib.rs");
        let text = "accepted\n".to_string();

        let effects = store.apply(event::Event::ReviewAcceptedFile {
            thread,
            path: path.clone(),
            text: text.clone(),
        });

        assert_eq!(
            effects,
            vec![effect::Effect::ApplyReviewAcceptedFile { thread, path, text }]
        );
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
    fn fork_submit_truncates_at_target_and_emits_fork_backend_command() {
        let (mut store, thread, backend) = backend_store();
        let mut caps = helix_acp::AgentCaps::default();
        caps.fork_session = true;
        set_caps(&mut store, thread, caps);
        append(
            &mut store,
            thread,
            thread::EntryKind::UserPrompt { text: "U0".into() },
        );
        append(
            &mut store,
            thread,
            thread::EntryKind::AssistantText { text: "A0".into() },
        );
        let u1 = append(
            &mut store,
            thread,
            thread::EntryKind::UserPrompt { text: "U1".into() },
        );
        append(
            &mut store,
            thread,
            thread::EntryKind::AssistantText { text: "A1".into() },
        );
        append(
            &mut store,
            thread,
            thread::EntryKind::UserPrompt { text: "U2".into() },
        );

        let effects = store.act(action::Action::ForkSubmit {
            thread,
            entry: u1,
            text: "edited U1".to_string(),
        });

        let entries = store.thread(thread).expect("thread").entries();
        assert_eq!(entries.len(), 3);
        assert!(matches!(
            &entries[0].kind,
            thread::EntryKind::UserPrompt { text } if text == "U0"
        ));
        assert!(matches!(
            &entries[1].kind,
            thread::EntryKind::AssistantText { text } if text == "A0"
        ));
        assert!(matches!(
            &entries[2].kind,
            thread::EntryKind::UserPrompt { text } if text == "edited U1"
        ));
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::ForkSubmit { thread: current_thread, prompt },
                } if current == &backend
                    && *current_thread == thread
                    && prompt.parts().iter().any(|part| {
                        matches!(part, super::super::prompt::Part::Text(text) if text == "edited U1")
                    })
            )
        }));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, effect::Effect::SetStatus { .. })));
    }

    #[test]
    fn fork_submit_without_capability_emits_fallback_status_and_still_truncates() {
        let (mut store, thread, backend) = backend_store();
        append(
            &mut store,
            thread,
            thread::EntryKind::UserPrompt { text: "U0".into() },
        );
        let u1 = append(
            &mut store,
            thread,
            thread::EntryKind::UserPrompt { text: "U1".into() },
        );
        append(
            &mut store,
            thread,
            thread::EntryKind::AssistantText { text: "A1".into() },
        );

        let effects = store.act(action::Action::ForkSubmit {
            thread,
            entry: u1,
            text: "edited U1".to_string(),
        });

        let entries = store.thread(thread).expect("thread").entries();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[1].kind,
            thread::EntryKind::UserPrompt { text } if text == "edited U1"
        ));
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SetStatus { message }
                    if message.contains("doesn't support editing history")
            )
        }));
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::ForkSubmit { thread: current_thread, .. },
                } if current == &backend && *current_thread == thread
            )
        }));
    }

    #[test]
    fn new_thread_emits_backend_command_and_creates_local_thread() {
        let (mut store, _thread, backend) = backend_store();
        let scope = thread::Scope::new(std::path::PathBuf::from("."));

        let effects = store.act(action::Action::NewThread {
            backend: backend.clone(),
            scope: scope.clone(),
            profile: None,
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
    fn complete_elicitation_updates_state_and_emits_backend_command() {
        let (mut store, thread, backend) = backend_store();
        let request = thread::Elicitation {
            id: "elicit-1".to_string(),
            status: thread::ElicitationStatus::Pending,
            mode: thread::ElicitationMode::Form {
                message: "Need input".to_string(),
                fields: vec![thread::ElicitationField {
                    name: "approved".to_string(),
                    field_type: thread::ElicitationFieldType::Bool,
                    label: None,
                    required: true,
                    options: Vec::new(),
                }],
            },
        };
        let _ = store.apply(event::Event::Thread {
            thread,
            event: thread::Event::Elicitation(thread::ElicitationEvent::Request(request)),
        });

        let response = thread::ElicitationResponse::Accept(vec![(
            "approved".to_string(),
            thread::ElicitationValue::Boolean(true),
        )]);
        let effects = store.act(action::Action::CompleteElicitation {
            thread,
            id: "elicit-1".to_string(),
            response: response.clone(),
        });

        assert_eq!(
            store
                .thread(thread)
                .and_then(|thread| thread.pending_elicitations().first())
                .map(|elicitation| elicitation.status),
            Some(thread::ElicitationStatus::Completed)
        );
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::CompleteElicitation {
                        thread: current_thread,
                        id,
                        response: current_response,
                    },
                } if current == &backend
                    && *current_thread == thread
                    && id == "elicit-1"
                    && current_response == &response
            )
        }));
    }

    #[test]
    fn set_review_mode_updates_thread_and_emits_backend_command() {
        let (mut store, thread, backend) = backend_store();

        let effects = store.act(action::Action::SetReviewMode {
            thread,
            mode: super::super::review::Mode::Review,
        });

        assert_eq!(
            store.thread(thread).map(|thread| thread.review_mode()),
            Some(super::super::review::Mode::Review)
        );
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::Review {
                        thread: current_thread,
                        command: super::super::review::Command::SetMode(super::super::review::Mode::Review),
                    },
                } if current == &backend && *current_thread == thread
            )
        }));
    }

    #[test]
    fn resolve_review_marks_pending_file() {
        let (mut store, thread, _backend) = backend_store();
        let path = std::path::PathBuf::from("file.rs");
        let _ = store.apply(event::Event::Thread {
            thread,
            event: thread::Event::Review(super::super::review::Event::Stage {
                mode: super::super::review::Mode::Review,
                file: super::super::review::File::staged(path.clone(), "old".into(), "new".into()),
            }),
        });

        let _ = store.act(action::Action::ResolveReview {
            thread,
            target: super::super::review::Target::File(path),
            decision: super::super::review::Decision::Accept,
        });

        let entry = store
            .thread(thread)
            .and_then(|thread| thread.entries().last())
            .expect("review entry");
        let thread::EntryKind::ChangeSummary(summary) = &entry.kind else {
            panic!("expected change summary");
        };
        assert_eq!(
            summary.files[0].review.as_ref().map(|file| file.status),
            Some(super::super::review::Status::Accepted)
        );
    }

    #[test]
    fn cancel_marks_thread_canceling_and_preserves_entries() {
        let (mut store, thread, backend) = backend_store();
        let effects = store.apply(event::Event::Thread {
            thread,
            event: thread::Event::Content(thread::Content::Append(thread::NewEntry {
                turn: None,
                kind: thread::EntryKind::AssistantText {
                    text: "partial".to_string(),
                },
                locations: Vec::new(),
            })),
        });
        assert!(!effects.is_empty());

        let effects = store.act(action::Action::Cancel { thread });

        let state = store.thread(thread).expect("thread");
        assert_eq!(state.run(), &thread::Run::Canceling);
        assert!(matches!(
            state.entries().first(),
            Some(thread::Entry {
                kind: thread::EntryKind::AssistantText { text },
                ..
            }) if text == "partial"
        ));
        assert!(matches!(
            state.entries().last(),
            Some(thread::Entry {
                kind: thread::EntryKind::Status { text },
                ..
            }) if text.contains("Canceling")
        ));
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::Cancel { thread: current_thread },
                } if current == &backend && *current_thread == thread
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
    fn delete_history_thread_removes_local_record_without_remote_command_when_disallowed() {
        let mut store = Store::default();
        let scope = thread::Scope::new(std::path::PathBuf::from("."));
        let thread = thread::Id::new(NonZeroU64::new(9).unwrap());
        store.replace_history(
            scope.clone(),
            vec![history::Stub {
                id: thread,
                origin: None,
                title: Some("local".to_string()),
                scope: scope.clone(),
                unread: false,
                run: thread::Run::Idle,
                feedback: thread::Feedback::default(),
            }],
            None,
        );

        let effects = store.act(action::Action::DeleteHistoryThread {
            thread,
            delete_remote: false,
        });

        assert!(store
            .history(&scope)
            .is_some_and(|page| page.entries.is_empty()));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, effect::Effect::Delete { thread: current } if *current == thread)));
        assert!(!effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    command: backend::Command::DeleteThread { .. },
                    ..
                }
            )
        }));
    }

    #[test]
    fn delete_history_thread_sends_protocol_delete_when_allowed() {
        let mut store = Store::default();
        let scope = thread::Scope::new(std::path::PathBuf::from("."));
        let thread = thread::Id::new(NonZeroU64::new(9).unwrap());
        let backend = backend::Id::new("backend");
        let remote = backend::Remote::new("remote-session");
        store.replace_history(
            scope,
            vec![history::Stub::remote_origin(
                thread,
                backend.clone(),
                remote.clone(),
                Some("remote".to_string()),
                thread::Scope::new(std::path::PathBuf::from(".")),
            )],
            None,
        );

        let effects = store.act(action::Action::DeleteHistoryThread {
            thread,
            delete_remote: true,
        });

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                effect::Effect::SendBackendCommand {
                    backend: current,
                    command: backend::Command::DeleteThread { remote: current_remote },
                } if current == &backend && current_remote == &remote
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
            record: Box::new(record),
            activation: crate::editor::Activation::Activate,
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
