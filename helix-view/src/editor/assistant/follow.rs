use anyhow::Context;

use super::Editor;
use crate::editor::types::AssistantFollowSnapshot;

impl Editor {
    fn current_assistant_follow_snapshot(&self) -> Option<AssistantFollowSnapshot> {
        let view = self.tree.get(self.tree.focus);
        let doc = self.document(view.doc)?;
        Some(AssistantFollowSnapshot {
            doc: view.doc,
            version: doc.version(),
            cursor: doc
                .selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
            scroll: doc.view_offset(view.id).vertical_offset,
        })
    }

    pub fn pause_assistant_follow_if_local_change(&mut self) {
        let current = self.current_assistant_follow_snapshot();
        let Some(snapshot) = current.clone() else {
            self.assistant_follow.snapshot = current;
            self.assistant_follow.suppress_pause = false;
            return;
        };

        let Some(previous) = self.assistant_follow.snapshot.replace(snapshot.clone()) else {
            self.assistant_follow.suppress_pause = false;
            return;
        };

        if previous == snapshot {
            self.assistant_follow.suppress_pause = false;
            return;
        }

        if self.assistant_follow.suppress_pause {
            self.assistant_follow.suppress_pause = false;
            return;
        }

        let event = if previous.doc != snapshot.doc {
            super::super::EditorEvent::BufferSwitched
        } else if previous.version != snapshot.version {
            super::super::EditorEvent::Edited
        } else if previous.cursor != snapshot.cursor {
            super::super::EditorEvent::CursorMoved
        } else if previous.scroll != snapshot.scroll {
            super::super::EditorEvent::Scrolled
        } else {
            return;
        };

        let Some(reason) = self.pause_current_surface(&event) else {
            return;
        };
        if let Some(effects) = self.pause_active_assistant_follow(reason) {
            self.apply_assistant_effects(effects);
        }
    }

    pub fn ensure_assistant_participant(&mut self, thread: crate::assistant::thread::Id) {
        let participant = crate::assistant::thread::participant(thread);
        if self.participant(participant).is_some() {
            return;
        }

        let name = self
            .assistant
            .thread(thread)
            .and_then(|thread| thread.title().map(ToOwned::to_owned))
            .unwrap_or_else(|| format!("assistant-{}", thread.value().get()));
        let effects = self.join_participant(crate::collab::Participant {
            id: participant,
            kind: crate::collab::participant::Kind::Agent,
            name,
            access: crate::collab::participant::Access::Read,
        });
        self.apply_collab_effects(effects);
    }

    pub fn toggle_active_assistant_follow(
        &mut self,
    ) -> anyhow::Result<(&'static str, Vec<crate::assistant::effect::Effect>)> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.toggle_assistant_follow(thread))
    }

    pub fn pause_active_assistant_follow(
        &mut self,
        reason: crate::collab::FollowPause,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        let (thread, state) = self.assistant.active_thread_owned()?;
        if !matches!(state.follow(), crate::collab::FollowState::On { .. }) {
            return None;
        }
        Some(self.pause_assistant_follow(thread, reason))
    }

    pub fn pause_assistant_follow(
        &mut self,
        thread: crate::assistant::thread::Id,
        reason: crate::collab::FollowPause,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::PauseFollow { thread, reason })
    }

    pub fn follow_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Follow { thread })
    }

    pub fn unfollow_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Unfollow { thread })
    }

    pub fn toggle_assistant_follow(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> (&'static str, Vec<crate::assistant::effect::Effect>) {
        let follow = self
            .assistant
            .thread(thread)
            .map(|thread| thread.follow().clone())
            .unwrap_or(crate::collab::FollowState::Off);

        let status = match follow {
            crate::collab::FollowState::Off => "Assistant follow enabled",
            crate::collab::FollowState::On { .. } => "Assistant follow disabled",
            crate::collab::FollowState::Paused { .. } => "Assistant follow resumed",
        };

        let effects = match follow {
            crate::collab::FollowState::Off | crate::collab::FollowState::Paused { .. } => {
                self.follow_assistant_thread(thread)
            }
            crate::collab::FollowState::On { .. } => self.unfollow_assistant_thread(thread),
        };
        (status, effects)
    }
}
