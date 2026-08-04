use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use parking_lot::RwLock;
use slotmap::Key;

use crate::{events, DocumentId};

#[derive(Clone, Default)]
pub struct Replication {
    inner: Arc<RwLock<State>>,
}

#[derive(Default)]
struct State {
    session: Option<helix_collab::GuestSessionHandle>,
    role: Option<helix_collab::Role>,
    by_document: HashMap<DocumentId, helix_collab::BufferId>,
    by_buffer: HashMap<helix_collab::BufferId, DocumentId>,
    paths: HashMap<DocumentId, helix_workspace::WorkspacePath>,
    pending_host_bindings: HashSet<DocumentId>,
    participants: HashSet<helix_collab::ParticipantId>,
    following: Option<helix_collab::ParticipantId>,
    hosting: bool,
    published_presence: HashMap<(DocumentId, crate::ViewId), helix_collab::LocalPresence>,
    expected_follow_presence: Option<PublishedPresence>,
    pending_presence: HashMap<helix_collab::BufferId, helix_collab::ResolvedPresence>,
}

#[derive(Clone, PartialEq, Eq)]
struct PublishedPresence {
    document: DocumentId,
    view: crate::ViewId,
    presence: helix_collab::LocalPresence,
}

impl State {
    fn prepare_presence(
        &mut self,
        document: DocumentId,
        view: crate::ViewId,
        presence: helix_collab::LocalPresence,
    ) -> bool {
        let key = (document, view);
        if self.published_presence.get(&key) == Some(&presence) {
            return false;
        }
        let expected = self
            .expected_follow_presence
            .as_ref()
            .is_some_and(|expected| {
                expected.document == document
                    && expected.view == view
                    && expected.presence == presence
            });
        if expected {
            self.expected_follow_presence = None;
        } else if self.following.is_some()
            && self
                .published_presence
                .get(&key)
                .is_none_or(|previous| previous.viewport != presence.viewport)
        {
            self.following = None;
        }
        self.published_presence.insert(key, presence);
        true
    }
}

impl Replication {
    pub fn attach(&self, session: helix_collab::GuestSessionHandle, hosting: bool) {
        let role = session.participant().role;
        let participants = session
            .project()
            .participants
            .into_iter()
            .map(|participant| participant.id)
            .collect();
        let mut state = self.inner.write();
        state.session = Some(session);
        state.role = Some(role);
        state.by_document.clear();
        state.by_buffer.clear();
        state.paths.clear();
        state.pending_host_bindings.clear();
        state.participants = participants;
        state.following = None;
        state.hosting = hosting;
        state.published_presence.clear();
        state.expected_follow_presence = None;
        state.pending_presence.clear();
    }

    pub fn detach(&self) -> Option<helix_collab::GuestSessionHandle> {
        let mut state = self.inner.write();
        state.role = None;
        state.by_document.clear();
        state.by_buffer.clear();
        state.paths.clear();
        state.pending_host_bindings.clear();
        state.participants.clear();
        state.following = None;
        state.hosting = false;
        state.published_presence.clear();
        state.expected_follow_presence = None;
        state.pending_presence.clear();
        state.session.take()
    }

    pub fn session(&self) -> Option<helix_collab::GuestSessionHandle> {
        self.inner.read().session.clone()
    }

    pub fn participant(&self) -> Option<helix_collab::ParticipantId> {
        self.inner
            .read()
            .session
            .as_ref()
            .map(|session| session.participant().id)
    }

    pub fn is_hosting(&self) -> bool {
        self.inner.read().hosting
    }

    pub fn role(&self) -> Option<helix_collab::Role> {
        self.inner.read().role
    }

    pub fn set_connection_role(&self, role: Option<helix_collab::Role>) {
        self.inner.write().role = role;
    }

    pub fn set_role(
        &self,
        participant: helix_collab::ParticipantId,
        role: helix_collab::Role,
    ) -> bool {
        let mut state = self.inner.write();
        let is_local = state
            .session
            .as_ref()
            .is_some_and(|session| session.participant().id == participant);
        if is_local {
            if state.role == Some(role) {
                return false;
            }
            state.role = Some(role);
        }
        is_local
    }

    pub fn participant_joined(&self, participant: helix_collab::ParticipantId) {
        self.inner.write().participants.insert(participant);
    }

    pub fn participant_left(&self, participant: helix_collab::ParticipantId) {
        self.inner.write().participants.remove(&participant);
    }

    pub fn replace_participants(
        &self,
        participants: &[helix_collab::ParticipantInfo],
    ) -> Vec<helix_collab::ParticipantId> {
        let next: HashSet<_> = participants
            .iter()
            .map(|participant| participant.id)
            .collect();
        let mut state = self.inner.write();
        let removed = state.participants.difference(&next).copied().collect();
        state.participants = next;
        removed
    }

    pub fn begin_host_binding(&self, document: DocumentId) -> bool {
        let mut state = self.inner.write();
        state.hosting
            && state.session.is_some()
            && !state.by_document.contains_key(&document)
            && state.pending_host_bindings.insert(document)
    }

    pub fn cancel_host_binding(&self, document: DocumentId) {
        self.inner.write().pending_host_bindings.remove(&document);
    }

    pub fn bind(
        &self,
        document: DocumentId,
        buffer: helix_collab::BufferId,
        path: helix_workspace::WorkspacePath,
    ) {
        let mut state = self.inner.write();
        state.pending_host_bindings.remove(&document);
        if let Some(previous) = state.by_document.insert(document, buffer) {
            state.by_buffer.remove(&previous);
        }
        if let Some(previous) = state.by_buffer.insert(buffer, document) {
            state.by_document.remove(&previous);
            state
                .published_presence
                .retain(|(published, _), _| *published != previous);
        }
        state.paths.insert(document, path);
        state
            .published_presence
            .retain(|(published, _), _| *published != document);
    }

    pub fn unbind_document(&self, document: DocumentId) {
        let mut state = self.inner.write();
        if let Some(buffer) = state.by_document.remove(&document) {
            state.by_buffer.remove(&buffer);
            state.pending_presence.remove(&buffer);
        }
        state.paths.remove(&document);
        state.pending_host_bindings.remove(&document);
        state
            .published_presence
            .retain(|(published, _), _| *published != document);
        if state
            .expected_follow_presence
            .as_ref()
            .is_some_and(|presence| presence.document == document)
        {
            state.expected_follow_presence = None;
        }
    }

    pub fn document(&self, buffer: helix_collab::BufferId) -> Option<DocumentId> {
        self.inner.read().by_buffer.get(&buffer).copied()
    }

    pub fn buffer(&self, document: DocumentId) -> Option<helix_collab::BufferId> {
        self.inner.read().by_document.get(&document).copied()
    }

    pub fn path(&self, document: DocumentId) -> Option<helix_workspace::WorkspacePath> {
        self.inner.read().paths.get(&document).cloned()
    }

    pub fn set_path(&self, document: DocumentId, path: helix_workspace::WorkspacePath) -> bool {
        let mut state = self.inner.write();
        let Some(current) = state.paths.get_mut(&document) else {
            return false;
        };
        if *current == path {
            return false;
        }
        *current = path;
        true
    }

    pub fn documents(&self) -> Vec<DocumentId> {
        self.inner.read().by_document.keys().copied().collect()
    }

    pub fn queue_presence(&self, presence: helix_collab::ResolvedPresence) {
        self.inner
            .write()
            .pending_presence
            .insert(presence.buffer, presence);
    }

    pub fn take_presence(
        &self,
        buffer: helix_collab::BufferId,
    ) -> Option<helix_collab::ResolvedPresence> {
        self.inner.write().pending_presence.remove(&buffer)
    }

    pub fn clear_presence(&self, buffer: helix_collab::BufferId) {
        self.inner.write().pending_presence.remove(&buffer);
    }

    pub fn set_following(&self, participant: Option<helix_collab::ParticipantId>) {
        let mut state = self.inner.write();
        state.following = participant;
        state.expected_follow_presence = None;
    }

    pub fn following(&self) -> Option<helix_collab::ParticipantId> {
        self.inner.read().following
    }

    pub(crate) fn document_changed(&self, event: &events::DocumentDidChange<'_>) {
        if event.origin == events::DocumentChangeOrigin::Collaboration
            || event.ghost_transaction
            || event.changes.is_empty()
        {
            return;
        }

        self.set_following(None);

        let (session, buffer, role) = {
            let state = self.inner.read();
            let Some(session) = state.session.clone() else {
                return;
            };
            let Some(buffer) = state.by_document.get(&event.doc.id()).copied() else {
                return;
            };
            (session, buffer, state.role)
        };
        if !role.is_some_and(|role| role.allows(helix_collab::Role::Write)) {
            log::warn!(
                "discarding local collaboration edit without write access: document={:?}",
                event.doc.id()
            );
            return;
        }

        let changes = event
            .changes
            .changes_iter()
            .map(|(start, end, insert)| helix_collab::TextChange {
                start,
                end,
                insert: insert.map_or_else(String::new, |insert| insert.to_string()),
            })
            .collect();
        session.queue_edit(buffer, changes, || event.doc.text().to_string());
    }

    pub(crate) fn selection_changed(&self, event: &events::SelectionDidChange<'_>) {
        self.set_following(None);
        self.publish_presence(event.doc, event.view);
    }

    pub(crate) fn publish_presence(&self, doc: &crate::Document, view: crate::ViewId) {
        let (session, buffer) = {
            let state = self.inner.read();
            let Some(session) = state.session.clone() else {
                return;
            };
            let Some(buffer) = state.by_document.get(&doc.id()).copied() else {
                return;
            };
            (session, buffer)
        };
        let active_view = collaboration_view_id(session.participant().id, view);
        let presence = local_presence(doc, view, buffer, Some(active_view));
        let mut state = self.inner.write();
        if !state.prepare_presence(doc.id(), view, presence.clone()) {
            return;
        }
        drop(state);
        session.queue_presence(presence);
    }

    pub(crate) fn record_followed_presence(&self, doc: &crate::Document, view: crate::ViewId) {
        let mut state = self.inner.write();
        if state.following.is_none() {
            return;
        }
        let Some(buffer) = state.by_document.get(&doc.id()).copied() else {
            return;
        };
        let active_view = state
            .session
            .as_ref()
            .map(|session| collaboration_view_id(session.participant().id, view));
        state.expected_follow_presence = Some(PublishedPresence {
            document: doc.id(),
            view,
            presence: local_presence(doc, view, buffer, active_view),
        });
    }
}

fn local_presence(
    doc: &crate::Document,
    view: crate::ViewId,
    buffer: helix_collab::BufferId,
    active_view: Option<helix_collab::ViewId>,
) -> helix_collab::LocalPresence {
    let primary = doc.selection(view).primary();
    let cursor = primary.cursor(doc.text().slice(..));
    let viewport = doc.view_offset(view).anchor.min(doc.text().len_chars());
    helix_collab::LocalPresence {
        buffer,
        cursor: Some(cursor),
        selection: (primary.anchor != primary.head).then_some((primary.anchor, primary.head)),
        viewport: Some(viewport),
        active_view,
    }
}

fn collaboration_view_id(
    participant: helix_collab::ParticipantId,
    view: crate::ViewId,
) -> helix_collab::ViewId {
    let mut id = participant.0;
    for (target, source) in id[8..].iter_mut().zip(view.data().as_ffi().to_le_bytes()) {
        *target ^= source;
    }
    helix_collab::ViewId(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(viewport: usize) -> helix_collab::LocalPresence {
        helix_collab::LocalPresence {
            buffer: helix_collab::BufferId(1),
            cursor: Some(viewport),
            selection: None,
            viewport: Some(viewport),
            active_view: None,
        }
    }

    #[test]
    fn remote_follow_updates_are_expected_but_local_viewport_changes_unfollow() {
        let document = DocumentId::default();
        let view = crate::ViewId::default();
        let participant = helix_collab::ParticipantId([7; 16]);
        let mut state = State {
            following: Some(participant),
            ..State::default()
        };
        assert!(state.prepare_presence(document, view, presence(4)));
        state.following = Some(participant);
        state.expected_follow_presence = Some(PublishedPresence {
            document,
            view,
            presence: presence(8),
        });

        assert!(state.prepare_presence(document, view, presence(8)));
        assert_eq!(state.following, Some(participant));
        assert!(state.expected_follow_presence.is_none());
        assert!(!state.prepare_presence(document, view, presence(8)));

        assert!(state.prepare_presence(document, view, presence(9)));
        assert_eq!(state.following, None);
    }
}
